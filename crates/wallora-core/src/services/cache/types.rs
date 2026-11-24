use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{mpsc, watch};

use crate::domain::{AccountId, SessionKind, SessionScope};
use crate::provider::ImageClass;

/// Default thumbnail/preview cache capacity: 2 GB.
pub const DEFAULT_CACHE_CAP_BYTES: u64 = 2_000_000_000;
/// Hard maximum thumbnail/preview cache capacity: 10,000,000,000 bytes (10 GB).
pub const HARD_MAX_CACHE_CAP_BYTES: u64 = 10_000_000_000;
/// Watchdog threshold after which a still-live cancelled writer is marked overdue.
pub const WATCHDOG_TIMEOUT: Duration = Duration::from_secs(10);

/// Validates a user-configured cache cap against the 10 GB hard ceiling.
pub fn validate_cache_cap(cap_bytes: u64) -> Result<u64, CacheError> {
    if cap_bytes > HARD_MAX_CACHE_CAP_BYTES {
        return Err(CacheError::CapExceedsHardMax {
            requested: cap_bytes,
            hard_max: HARD_MAX_CACHE_CAP_BYTES,
        });
    }
    Ok(cap_bytes)
}

/// Durable cache namespace separating Guest and each signed-in AccountId.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CacheNamespace {
    Guest,
    Account(AccountId),
}

impl CacheNamespace {
    pub fn from_scope(scope: &SessionScope) -> Self {
        match &scope.kind {
            SessionKind::Guest => Self::Guest,
            SessionKind::Account(id) => Self::Account(id.clone()),
        }
    }

    pub fn as_db_str(&self) -> String {
        match self {
            Self::Guest => "guest".to_string(),
            Self::Account(id) => format!("acct:{}", id.as_str()),
        }
    }

    pub fn from_db_str(raw: &str) -> Option<Self> {
        if raw == "guest" {
            Some(Self::Guest)
        } else if let Some(rest) = raw.strip_prefix("acct:")
            && !rest.is_empty()
        {
            Some(Self::Account(AccountId::new(rest)))
        } else {
            None
        }
    }
}

/// Logical key identifying a cached thumbnail or preview inside a session namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    pub class: ImageClass,
    pub key: String,
}

impl std::hash::Hash for CacheKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let tag = match self.class {
            ImageClass::Thumbnail => 0u8,
            ImageClass::Preview => 1u8,
        };
        tag.hash(state);
        self.key.hash(state);
    }
}

impl CacheKey {
    pub fn new(class: ImageClass, key: impl Into<String>) -> Result<Self, CacheError> {
        let key = key.into();
        if key.trim().is_empty() {
            return Err(CacheError::InvalidKey);
        }
        Ok(Self { class, key })
    }
}

/// Configuration for opening [`crate::services::cache::CacheStore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheConfig {
    pub root_dir: PathBuf,
    pub cap_bytes: u64,
    pub initial_scope: SessionScope,
}

impl CacheConfig {
    pub fn new(root_dir: PathBuf, initial_scope: SessionScope) -> Self {
        Self {
            root_dir,
            cap_bytes: DEFAULT_CACHE_CAP_BYTES,
            initial_scope,
        }
    }

    pub fn with_cap_bytes(mut self, cap_bytes: u64) -> Result<Self, CacheError> {
        validate_cache_cap(cap_bytes)?;
        self.cap_bytes = cap_bytes;
        Ok(self)
    }
}

/// Connectivity mode affecting whether persisted cache entries are returned as stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheConnectivity {
    #[default]
    Online,
    Offline,
}

/// Result of reading an entry from the cache store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLookup {
    Hit {
        scope: SessionScope,
        bytes: Vec<u8>,
        stale: bool,
    },
    Miss,
    CorruptPurged,
}

/// Result of admitting and completing a cache write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachePutResult {
    pub scope: SessionScope,
    pub byte_len: u64,
}

/// Snapshot of the actor's checked byte ledger and writer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheStats {
    pub cap_bytes: u64,
    pub published_bytes: u64,
    pub reserved_bytes: u64,
    pub tombstone_bytes: u64,
    pub total_charged_bytes: u64,
    pub entry_count: usize,
    pub live_reservations: usize,
    pub pending_deletes: usize,
    pub overdue_writers: usize,
    pub overdue_reservation_ids: Vec<u64>,
    pub current_scope: SessionScope,
    pub connectivity: CacheConnectivity,
}

/// Scope transition message emitted by [`crate::services::SessionManager`] without blocking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheScopeTransition {
    pub new_scope: SessionScope,
    pub purge_account: Option<AccountId>,
}

/// Non-blocking sink notified whenever [`crate::services::SessionManager`] transitions scope
/// or must purge an account's cache (such as 401 revocation or 403 on reconnect).
pub trait SessionCacheSink: Send + Sync + 'static {
    fn on_scope_transition(&self, transition: CacheScopeTransition);
    fn on_purge_account(&self, account_id: &AccountId);
}

/// Default no-op [`SessionCacheSink`] used when [`crate::services::SessionManager`] is
/// constructed without an attached cache actor.
#[derive(Debug, Default)]
pub struct NoopSessionCacheSink;

impl SessionCacheSink for NoopSessionCacheSink {
    fn on_scope_transition(&self, _transition: CacheScopeTransition) {}
    fn on_purge_account(&self, _account_id: &AccountId) {}
}

/// Cooperative cancellation token passed to supervised cache writers.
#[derive(Debug, Clone)]
pub struct CancelToken {
    tx: watch::Sender<bool>,
    rx: watch::Receiver<bool>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(false);
        Self { tx, rx }
    }

    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    pub async fn cancelled(&mut self) {
        while !*self.rx.borrow_and_update() {
            if self.rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Errors from a streaming byte source feeding a cache writer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CacheSourceError {
    #[error("cache write stream cancelled")]
    Cancelled,
    #[error("cache write stream failed: {0}")]
    Io(String),
}

/// Boxed future returned by [`CacheByteStream::next_chunk`].
pub type CacheChunkFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Vec<u8>>, CacheSourceError>> + Send + 'a>>;

/// Async chunk stream consumed by a supervised cache writer.
pub trait CacheByteStream: Send + 'static {
    fn next_chunk<'a>(&'a mut self) -> CacheChunkFuture<'a>;
}

impl CacheByteStream for Vec<u8> {
    fn next_chunk<'a>(&'a mut self) -> CacheChunkFuture<'a> {
        Box::pin(async move {
            if self.is_empty() {
                Ok(None)
            } else {
                Ok(Some(std::mem::take(self)))
            }
        })
    }
}

impl CacheByteStream for mpsc::Receiver<Result<Vec<u8>, CacheSourceError>> {
    fn next_chunk<'a>(&'a mut self) -> CacheChunkFuture<'a> {
        Box::pin(async move {
            match self.recv().await {
                Some(Ok(chunk)) => Ok(Some(chunk)),
                Some(Err(err)) => Err(err),
                None => Ok(None),
            }
        })
    }
}

/// Errors returned by [`crate::services::cache::CacheStore`] operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CacheError {
    #[error("cache cap {requested} bytes exceeds the {hard_max} byte hard maximum")]
    CapExceedsHardMax { requested: u64, hard_max: u64 },
    #[error("cache key must not be empty")]
    InvalidKey,
    #[error("cache root is already locked by another cache store")]
    Locked,
    #[error("cache scope does not match the active session scope")]
    ScopeMismatch,
    #[error("payload size ({actual} bytes) exceeds class limit ({limit} bytes)")]
    PayloadTooLarge { actual: u64, limit: u64 },
    #[error("cache capacity exceeded; could not free enough space")]
    CapacityExceeded,
    #[error(
        "cannot lower cache cap to {requested} bytes while {charged} bytes remain safely un-evictable"
    )]
    CapReductionRejected { requested: u64, charged: u64 },
    #[error("startup cache usage ({charged} bytes) exceeds cap ({cap} bytes) and cleanup failed")]
    StartupCapExceeded { charged: u64, cap: u64 },
    #[error("cache write was cancelled")]
    Cancelled,
    #[error("cache writer failed: {0}")]
    WriterFailed(String),
    #[error("cache filesystem error: {0}")]
    Io(String),
    #[error("cache index error: {0}")]
    Index(String),
    #[error("cache store is shutting down")]
    ShuttingDown,
}
