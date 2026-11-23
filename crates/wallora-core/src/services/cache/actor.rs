use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tokio::task::{Id as TaskId, JoinHandle, JoinSet};
use tokio::time::Instant;

use crate::domain::{AccountId, SessionScope};
use crate::services::cache::seams::{
    CacheClock, CacheEntryKey, CacheFs, CacheIndex, CacheIoReadError, CacheLockError,
    CacheLockGuard, Crc32Hasher, PendingDeleteRecord, PublishedEntryRecord, RealCacheFs,
    SqliteCacheIndex, StagingWriter, SystemCacheClock, crc32_ieee,
};
use crate::services::cache::types::{
    CacheByteStream, CacheConfig, CacheConnectivity, CacheError, CacheKey, CacheLookup,
    CacheNamespace, CachePutResult, CacheScopeTransition, CacheSourceError, CacheStats,
    CancelToken, SessionCacheSink, WATCHDOG_TIMEOUT, validate_cache_cap,
};

#[derive(Debug, Default)]
struct ByteLedger {
    published_bytes: u64,
    reserved_bytes: u64,
    tombstone_bytes: u64,
}

impl ByteLedger {
    fn total_charged(&self) -> Option<u64> {
        self.published_bytes
            .checked_add(self.reserved_bytes)?
            .checked_add(self.tombstone_bytes)
    }

    fn non_evictable_bytes(&self) -> Option<u64> {
        self.reserved_bytes.checked_add(self.tombstone_bytes)
    }

    fn charge_published(&mut self, bytes: u64) -> Result<(), CacheError> {
        self.published_bytes = self
            .published_bytes
            .checked_add(bytes)
            .ok_or_else(|| CacheError::Index("published byte ledger overflow".to_string()))?;
        Ok(())
    }

    fn release_published(&mut self, bytes: u64) -> Result<(), CacheError> {
        self.published_bytes = self
            .published_bytes
            .checked_sub(bytes)
            .ok_or_else(|| CacheError::Index("published byte ledger underflow".to_string()))?;
        Ok(())
    }

    fn charge_reserved(&mut self, bytes: u64) -> Result<(), CacheError> {
        self.reserved_bytes = self
            .reserved_bytes
            .checked_add(bytes)
            .ok_or_else(|| CacheError::Index("reserved byte ledger overflow".to_string()))?;
        Ok(())
    }

    fn release_reserved(&mut self, bytes: u64) -> Result<(), CacheError> {
        self.reserved_bytes = self
            .reserved_bytes
            .checked_sub(bytes)
            .ok_or_else(|| CacheError::Index("reserved byte ledger underflow".to_string()))?;
        Ok(())
    }

    fn charge_tombstone(&mut self, bytes: u64) -> Result<(), CacheError> {
        self.tombstone_bytes = self
            .tombstone_bytes
            .checked_add(bytes)
            .ok_or_else(|| CacheError::Index("tombstone byte ledger overflow".to_string()))?;
        Ok(())
    }

    fn release_tombstone(&mut self, bytes: u64) -> Result<(), CacheError> {
        self.tombstone_bytes = self
            .tombstone_bytes
            .checked_sub(bytes)
            .ok_or_else(|| CacheError::Index("tombstone byte ledger underflow".to_string()))?;
        Ok(())
    }
}

struct LiveReservation {
    reservation_id: u64,
    caller_id: u64,
    entry_key: CacheEntryKey,
    scope: SessionScope,
    charge: u64,
    staging_rel_path: String,
    cancel_token: CancelToken,
    cancelled: bool,
    watchdog_deadline: Option<Instant>,
    overdue: bool,
    reply_tx: Option<oneshot::Sender<Result<CachePutResult, CacheError>>>,
}

#[derive(Debug)]
enum WriterExit {
    Complete { actual_len: u64, crc32: u32 },
    Cancelled,
    ExceededReservation { attempted: u64, limit: u64 },
    SourceError(CacheSourceError),
    Failed(String),
    PanickedOrAborted,
}

enum ActorCommand {
    Get {
        key: CacheKey,
        scope: SessionScope,
        reply: oneshot::Sender<Result<CacheLookup, CacheError>>,
    },
    BeginPut {
        caller_id: u64,
        key: CacheKey,
        scope: SessionScope,
        expected_len: Option<u64>,
        stream: Box<dyn CacheByteStream>,
        reply: oneshot::Sender<Result<CachePutResult, CacheError>>,
    },
    CancelCaller {
        caller_id: u64,
    },
    SetCap {
        cap_bytes: u64,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
    SetConnectivity {
        connectivity: CacheConnectivity,
        reply: oneshot::Sender<()>,
    },
    TransitionScope {
        transition: CacheScopeTransition,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
    PurgeAccount {
        account_id: AccountId,
        reply: oneshot::Sender<Result<(), CacheError>>,
    },
    InvalidateEntry {
        key: CacheKey,
        scope: SessionScope,
        reply: oneshot::Sender<Result<bool, CacheError>>,
    },
    Stats {
        reply: oneshot::Sender<Result<CacheStats, CacheError>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
    #[cfg(any(test, feature = "test-util"))]
    InjectFinishedForTest {
        reservation_id: u64,
        scope: SessionScope,
        key: CacheKey,
        actual_len: u64,
        crc32: u32,
        reply: oneshot::Sender<()>,
    },
}

struct CancelOnDrop {
    caller_id: u64,
    cmd_tx: mpsc::Sender<ActorCommand>,
    armed: bool,
}

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            let cmd = ActorCommand::CancelCaller {
                caller_id: self.caller_id,
            };
            if let Err(mpsc::error::TrySendError::Full(cmd)) = self.cmd_tx.try_send(cmd)
                && let Ok(handle) = tokio::runtime::Handle::try_current()
            {
                let tx = self.cmd_tx.clone();
                handle.spawn(async move {
                    let _ = tx.send(cmd).await;
                });
            }
        }
    }
}

/// Single-writer actor handle for the scoped thumbnail and preview cache.
///
/// All reads, writes, evictions, cap updates, and scope transitions are serialized through
/// one actor task that owns the exclusive cache root lock and SQLite ready index.
#[derive(Clone)]
pub struct CacheStore {
    cmd_tx: mpsc::Sender<ActorCommand>,
    transition_tx: mpsc::UnboundedSender<CacheScopeTransition>,
    next_caller_id: Arc<AtomicU64>,
    actor_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl CacheStore {
    /// Opens the cache root with the OS filesystem, SQLite index, and system clock.
    pub async fn open(config: CacheConfig) -> Result<Self, CacheError> {
        Self::open_with_seams(
            config,
            Arc::new(RealCacheFs),
            Arc::new(SqliteCacheIndex::new()),
            Arc::new(SystemCacheClock),
        )
        .await
    }

    /// Opens the cache root using injected filesystem, index, and clock seams.
    pub async fn open_with_seams(
        config: CacheConfig,
        fs: Arc<dyn CacheFs>,
        index: Arc<dyn CacheIndex>,
        clock: Arc<dyn CacheClock>,
    ) -> Result<Self, CacheError> {
        let cap_bytes = validate_cache_cap(config.cap_bytes)?;
        let root_dir = config.root_dir.clone();
        let initial_scope = config.initial_scope.clone();

        let startup = tokio::task::spawn_blocking(move || {
            init_and_reconcile(root_dir, cap_bytes, initial_scope, fs, index, clock)
        })
        .await
        .map_err(|e| CacheError::Io(format!("startup reconciliation task failed: {e}")))??;

        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        let (transition_tx, transition_rx) = mpsc::unbounded_channel();

        let actor_handle = tokio::spawn(startup.run(cmd_rx, transition_rx));
        Ok(Self {
            cmd_tx,
            transition_tx,
            next_caller_id: Arc::new(AtomicU64::new(1)),
            actor_task: Arc::new(Mutex::new(Some(actor_handle))),
        })
    }

    /// Reads an entry if it belongs to `scope` (matching both `kind` and `generation`).
    pub async fn get(&self, key: CacheKey, scope: SessionScope) -> Result<CacheLookup, CacheError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::Get {
                key,
                scope,
                reply: tx,
            })
            .await
            .map_err(|_| CacheError::ShuttingDown)?;
        rx.await.map_err(|_| CacheError::ShuttingDown)?
    }

    /// Reserves space, streams bytes to staging through a supervised writer, and publishes
    /// atomically without overwriting any prior content file.
    ///
    /// Dropping the returned future cancels the reservation cooperatively through the actor
    /// without touching the staging file directly.
    pub async fn put<S>(
        &self,
        key: CacheKey,
        scope: SessionScope,
        expected_len: Option<u64>,
        stream: S,
    ) -> Result<CachePutResult, CacheError>
    where
        S: CacheByteStream,
    {
        let caller_id = self.next_caller_id.fetch_add(1, Ordering::Relaxed);
        let mut cancel_guard = CancelOnDrop {
            caller_id,
            cmd_tx: self.cmd_tx.clone(),
            armed: true,
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::BeginPut {
                caller_id,
                key,
                scope,
                expected_len,
                stream: Box::new(stream),
                reply: reply_tx,
            })
            .await
            .map_err(|_| CacheError::ShuttingDown)?;

        let result = reply_rx.await.map_err(|_| CacheError::ShuttingDown)?;
        cancel_guard.disarm();
        result
    }

    /// Updates the cache capacity cap (up to `10,000,000,000` bytes).
    ///
    /// Rejects lowering the cap below what safe LRU eviction can reclaim while reservations or
    /// undeleted tombstones hold bytes.
    pub async fn set_cap(&self, cap_bytes: u64) -> Result<(), CacheError> {
        validate_cache_cap(cap_bytes)?;
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::SetCap {
                cap_bytes,
                reply: tx,
            })
            .await
            .map_err(|_| CacheError::ShuttingDown)?;
        rx.await.map_err(|_| CacheError::ShuttingDown)?
    }

    /// Updates connectivity mode (`Online` vs `Offline`) for stale-hit visibility.
    pub async fn set_connectivity(
        &self,
        connectivity: CacheConnectivity,
    ) -> Result<(), CacheError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::SetConnectivity {
                connectivity,
                reply: tx,
            })
            .await
            .map_err(|_| CacheError::ShuttingDown)?;
        rx.await.map_err(|_| CacheError::ShuttingDown)
    }

    /// Applies a session scope transition synchronously-acknowledged through the actor.
    pub async fn transition_scope(
        &self,
        transition: CacheScopeTransition,
    ) -> Result<(), CacheError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::TransitionScope {
                transition,
                reply: tx,
            })
            .await
            .map_err(|_| CacheError::ShuttingDown)?;
        rx.await.map_err(|_| CacheError::ShuttingDown)?
    }

    /// Purges all entries for `account_id` (for example on 401/403 reconnect).
    pub async fn purge_account(&self, account_id: AccountId) -> Result<(), CacheError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::PurgeAccount {
                account_id,
                reply: tx,
            })
            .await
            .map_err(|_| CacheError::ShuttingDown)?;
        rx.await.map_err(|_| CacheError::ShuttingDown)?
    }

    /// Removes a specific entry (for example after decode failure) so the caller can refetch.
    pub async fn invalidate_entry(
        &self,
        key: CacheKey,
        scope: SessionScope,
    ) -> Result<bool, CacheError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::InvalidateEntry {
                key,
                scope,
                reply: tx,
            })
            .await
            .map_err(|_| CacheError::ShuttingDown)?;
        rx.await.map_err(|_| CacheError::ShuttingDown)?
    }

    /// Returns a snapshot of the actor's checked byte ledger and writer state.
    pub async fn stats(&self) -> Result<CacheStats, CacheError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::Stats { reply: tx })
            .await
            .map_err(|_| CacheError::ShuttingDown)?;
        rx.await.map_err(|_| CacheError::ShuttingDown)?
    }

    /// Stops new admissions, cancels and joins all active writers, closes the index,
    /// and only then releases the exclusive root lock.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(ActorCommand::Shutdown { reply: tx })
            .await
            .is_ok()
        {
            let _ = rx.await;
        }
        let handle = self.actor_task.lock().ok().and_then(|mut g| g.take());
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    /// Test-only helper to inject a duplicate or retired writer completion event into the actor.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn inject_writer_finished_for_test(
        &self,
        reservation_id: u64,
        scope: SessionScope,
        key: CacheKey,
        actual_len: u64,
        crc32: u32,
    ) -> Result<(), CacheError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(ActorCommand::InjectFinishedForTest {
                reservation_id,
                scope,
                key,
                actual_len,
                crc32,
                reply: tx,
            })
            .await
            .map_err(|_| CacheError::ShuttingDown)?;
        rx.await.map_err(|_| CacheError::ShuttingDown)?;
        Ok(())
    }
}

impl SessionCacheSink for CacheStore {
    fn on_scope_transition(&self, transition: CacheScopeTransition) {
        let _ = self.transition_tx.send(transition);
    }
}

struct CacheActor {
    root_dir: PathBuf,
    cap_bytes: u64,
    current_scope: SessionScope,
    connectivity: CacheConnectivity,
    fs: Arc<dyn CacheFs>,
    index: Option<Arc<dyn CacheIndex>>,
    clock: Arc<dyn CacheClock>,
    lock_guard: Option<Box<dyn CacheLockGuard>>,
    ledger: ByteLedger,
    ready_entries: HashMap<CacheEntryKey, PublishedEntryRecord>,
    tombstones: HashMap<String, u64>,
    live_reservations: HashMap<u64, LiveReservation>,
    task_to_reservation: HashMap<TaskId, u64>,
    caller_to_reservation: HashMap<u64, u64>,
    cancelled_callers: HashSet<u64>,
    join_set: JoinSet<(u64, WriterExit)>,
    next_reservation_id: u64,
    next_access_seq: u64,
    shutting_down: bool,
}

fn init_and_reconcile(
    root_dir: PathBuf,
    cap_bytes: u64,
    initial_scope: SessionScope,
    fs: Arc<dyn CacheFs>,
    index: Arc<dyn CacheIndex>,
    clock: Arc<dyn CacheClock>,
) -> Result<CacheActor, CacheError> {
    fs.create_dir_all(&root_dir)
        .map_err(|e| CacheError::Io(e.to_string()))?;

    let lock_guard = match fs.try_lock_exclusive(&root_dir.join("cache.lock")) {
        Ok(guard) => guard,
        Err(CacheLockError::Locked) => return Err(CacheError::Locked),
        Err(CacheLockError::Io(msg)) => return Err(CacheError::Io(msg)),
    };

    let staging_dir = root_dir.join("staging");
    let objects_dir = root_dir.join("objects");
    fs.create_dir_all(&staging_dir)
        .map_err(|e| CacheError::Io(e.to_string()))?;
    fs.create_dir_all(&objects_dir)
        .map_err(|e| CacheError::Io(e.to_string()))?;

    let snapshot = index.open_and_load(&root_dir.join("cache_index.db"))?;

    let mut tombstones: HashMap<String, u64> = HashMap::new();
    let mut new_pending_deletes: Vec<PendingDeleteRecord> = Vec::new();

    // 1. Reconcile any leftover staging files from an interrupted run.
    let staging_entries = fs
        .list_dir(&staging_dir)
        .map_err(|e| CacheError::Io(e.to_string()))?;
    for entry in staging_entries {
        if !entry.is_file {
            continue;
        }
        let rel_path = format!("staging/{}", entry.file_name);
        if fs.remove_file(&root_dir.join(&rel_path)).is_err() {
            let actual_len = fs
                .file_len(&root_dir.join(&rel_path))
                .ok()
                .flatten()
                .unwrap_or(entry.byte_len);
            tombstones.insert(rel_path.clone(), actual_len);
            new_pending_deletes.push(PendingDeleteRecord {
                rel_path,
                byte_len: actual_len,
            });
        }
    }

    // 2. Reconcile previously recorded pending deletes.
    for del in snapshot.pending_deletes {
        if tombstones.contains_key(&del.rel_path) {
            continue;
        }
        match fs.remove_file(&root_dir.join(&del.rel_path)) {
            Ok(()) => {
                index.mark_delete_completed(&del.rel_path)?;
            }
            Err(_) => {
                let actual_len = fs
                    .file_len(&root_dir.join(&del.rel_path))
                    .ok()
                    .flatten()
                    .unwrap_or(del.byte_len);
                tombstones.insert(del.rel_path, actual_len);
            }
        }
    }

    // 3. Reconcile ready entries against object files on disk.
    let mut ready_entries: HashMap<CacheEntryKey, PublishedEntryRecord> = HashMap::new();
    let mut indexed_paths: HashSet<String> = HashSet::new();
    let mut dead_entry_keys: Vec<CacheEntryKey> = Vec::new();
    let mut max_access_seq: u64 = 0;

    for record in snapshot.ready_entries {
        max_access_seq = max_access_seq.max(record.last_access);
        let abs_path = root_dir.join(&record.rel_path);
        match fs.read_bounded(&abs_path, record.entry_key.key.class.byte_limit()) {
            Ok(bytes)
                if bytes.len() as u64 == record.byte_len && crc32_ieee(&bytes) == record.crc32 =>
            {
                indexed_paths.insert(record.rel_path.clone());
                ready_entries.insert(record.entry_key.clone(), record);
            }
            Ok(_) | Err(CacheIoReadError::TooLarge) | Err(CacheIoReadError::Io(_)) => {
                dead_entry_keys.push(record.entry_key);
                if fs.remove_file(&abs_path).is_err() {
                    let actual_len = fs
                        .file_len(&abs_path)
                        .ok()
                        .flatten()
                        .unwrap_or(record.byte_len);
                    tombstones.insert(record.rel_path.clone(), actual_len);
                    new_pending_deletes.push(PendingDeleteRecord {
                        rel_path: record.rel_path,
                        byte_len: actual_len,
                    });
                }
            }
            Err(CacheIoReadError::NotFound) => {
                dead_entry_keys.push(record.entry_key);
            }
        }
    }

    if !dead_entry_keys.is_empty() || !new_pending_deletes.is_empty() {
        index.hide_and_queue_deletes(&dead_entry_keys, &new_pending_deletes)?;
    }

    // 4. Scan objects/ for unindexed content (e.g., crash after rename and before index commit).
    let mut unindexed_deletes: Vec<PendingDeleteRecord> = Vec::new();
    let object_entries = fs
        .list_dir(&objects_dir)
        .map_err(|e| CacheError::Io(e.to_string()))?;
    for entry in object_entries {
        if !entry.is_file {
            continue;
        }
        let rel_path = format!("objects/{}", entry.file_name);
        if indexed_paths.contains(&rel_path) || tombstones.contains_key(&rel_path) {
            continue;
        }
        if fs.remove_file(&root_dir.join(&rel_path)).is_err() {
            let actual_len = fs
                .file_len(&root_dir.join(&rel_path))
                .ok()
                .flatten()
                .unwrap_or(entry.byte_len);
            tombstones.insert(rel_path.clone(), actual_len);
            unindexed_deletes.push(PendingDeleteRecord {
                rel_path,
                byte_len: actual_len,
            });
        }
    }
    if !unindexed_deletes.is_empty() {
        index.record_pending_deletes(&unindexed_deletes)?;
    }

    let mut ledger = ByteLedger::default();
    for record in ready_entries.values() {
        ledger.charge_published(record.byte_len)?;
    }
    for &tomb_len in tombstones.values() {
        ledger.charge_tombstone(tomb_len)?;
    }

    let mut actor = CacheActor {
        root_dir,
        cap_bytes,
        current_scope: initial_scope,
        connectivity: CacheConnectivity::Online,
        fs,
        index: Some(index),
        clock,
        lock_guard: Some(lock_guard),
        ledger,
        ready_entries,
        tombstones,
        live_reservations: HashMap::new(),
        task_to_reservation: HashMap::new(),
        caller_to_reservation: HashMap::new(),
        cancelled_callers: HashSet::new(),
        join_set: JoinSet::new(),
        next_reservation_id: 1,
        next_access_seq: max_access_seq.saturating_add(1),
        shutting_down: false,
    };

    // 5. Enforce cap after reconciliation before admitting work.
    actor.evict_ready_to_fit_sync(cap_bytes)?;
    let charged = actor
        .ledger
        .total_charged()
        .ok_or_else(|| CacheError::Index("byte ledger overflow at startup".to_string()))?;
    if charged > cap_bytes {
        // Drop DB and root lock cleanly before failing startup.
        actor.index.take();
        actor.lock_guard.take();
        return Err(CacheError::StartupCapExceeded {
            charged,
            cap: cap_bytes,
        });
    }

    Ok(actor)
}

impl CacheActor {
    fn alloc_access_seq(&mut self) -> u64 {
        let seq = self.next_access_seq;
        self.next_access_seq = self.next_access_seq.saturating_add(1);
        seq
    }

    fn require_index(&self) -> Result<&Arc<dyn CacheIndex>, CacheError> {
        self.index.as_ref().ok_or(CacheError::ShuttingDown)
    }

    fn next_watchdog_deadline(&self) -> Option<Instant> {
        self.live_reservations
            .values()
            .filter(|res| res.cancelled && !res.overdue)
            .filter_map(|res| res.watchdog_deadline)
            .min()
    }

    async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<ActorCommand>,
        mut transition_rx: mpsc::UnboundedReceiver<CacheScopeTransition>,
    ) {
        loop {
            let next_deadline = self.next_watchdog_deadline();
            tokio::select! {
                biased;

                maybe_transition = transition_rx.recv() => {
                    match maybe_transition {
                        Some(transition) => {
                            let _ = self.handle_scope_transition(transition).await;
                        }
                        None => {
                            // Transition channel closed only when all CacheStore handles drop.
                        }
                    }
                }

                Some(join_res) = self.join_set.join_next_with_id(), if !self.join_set.is_empty() => {
                    self.handle_writer_join(join_res).await;
                }

                () = async {
                    if let Some(deadline) = next_deadline {
                        tokio::time::sleep_until(deadline).await;
                    }
                }, if next_deadline.is_some() => {
                    self.handle_watchdog_tick(Instant::now());
                }

                maybe_cmd = cmd_rx.recv() => {
                    match maybe_cmd {
                        Some(ActorCommand::Shutdown { reply }) => {
                            self.perform_shutdown().await;
                            let _ = reply.send(());
                            return;
                        }
                        Some(cmd) => {
                            self.handle_command(cmd).await;
                        }
                        None => {
                            self.perform_shutdown().await;
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn handle_command(&mut self, cmd: ActorCommand) {
        match cmd {
            ActorCommand::Get { key, scope, reply } => {
                let res = self.handle_get(key, scope).await;
                let _ = reply.send(res);
            }
            ActorCommand::BeginPut {
                caller_id,
                key,
                scope,
                expected_len,
                stream,
                reply,
            } => {
                self.handle_begin_put(caller_id, key, scope, expected_len, stream, reply)
                    .await;
            }
            ActorCommand::CancelCaller { caller_id } => {
                self.cancel_by_caller(caller_id);
            }
            ActorCommand::SetCap { cap_bytes, reply } => {
                let res = self.handle_set_cap(cap_bytes).await;
                let _ = reply.send(res);
            }
            ActorCommand::SetConnectivity {
                connectivity,
                reply,
            } => {
                self.connectivity = connectivity;
                let _ = reply.send(());
            }
            ActorCommand::TransitionScope { transition, reply } => {
                let res = self.handle_scope_transition(transition).await;
                let _ = reply.send(res);
            }
            ActorCommand::PurgeAccount { account_id, reply } => {
                let res = self.purge_account_entries(&account_id).await;
                let _ = reply.send(res);
            }
            ActorCommand::InvalidateEntry { key, scope, reply } => {
                let res = self.handle_invalidate(key, scope).await;
                let _ = reply.send(res);
            }
            ActorCommand::Stats { reply } => {
                let res = self.build_stats();
                let _ = reply.send(res);
            }
            ActorCommand::Shutdown { reply } => {
                self.perform_shutdown().await;
                let _ = reply.send(());
            }
            #[cfg(any(test, feature = "test-util"))]
            ActorCommand::InjectFinishedForTest {
                reservation_id,
                scope,
                key,
                actual_len,
                crc32,
                reply,
            } => {
                self.handle_synthetic_finished_for_test(
                    reservation_id,
                    scope,
                    key,
                    actual_len,
                    crc32,
                )
                .await;
                let _ = reply.send(());
            }
        }
    }

    fn build_stats(&self) -> Result<CacheStats, CacheError> {
        let total_charged_bytes = self
            .ledger
            .total_charged()
            .ok_or_else(|| CacheError::Index("byte ledger overflow".to_string()))?;
        let mut overdue_ids: Vec<u64> = self
            .live_reservations
            .values()
            .filter(|r| r.overdue)
            .map(|r| r.reservation_id)
            .collect();
        overdue_ids.sort_unstable();
        Ok(CacheStats {
            cap_bytes: self.cap_bytes,
            published_bytes: self.ledger.published_bytes,
            reserved_bytes: self.ledger.reserved_bytes,
            tombstone_bytes: self.ledger.tombstone_bytes,
            total_charged_bytes,
            entry_count: self.ready_entries.len(),
            live_reservations: self.live_reservations.len(),
            pending_deletes: self.tombstones.len(),
            overdue_writers: overdue_ids.len(),
            overdue_reservation_ids: overdue_ids,
            current_scope: self.current_scope.clone(),
            connectivity: self.connectivity,
        })
    }

    async fn run_blocking<F, R>(&self, f: F) -> Result<R, CacheError>
    where
        F: FnOnce() -> Result<R, CacheError> + Send + 'static,
        R: Send + 'static,
    {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| CacheError::Io(format!("blocking cache task failed: {e}")))?
    }

    async fn retry_pending_deletes(&mut self) {
        if self.tombstones.is_empty() {
            return;
        }
        let paths: Vec<String> = self.tombstones.keys().cloned().collect();
        let root_dir = self.root_dir.clone();
        let fs = Arc::clone(&self.fs);
        let Ok(index) = self.require_index().map(Arc::clone) else {
            return;
        };

        let removed = self
            .run_blocking(move || {
                let mut deleted = Vec::new();
                for rel_path in paths {
                    if fs.remove_file(&root_dir.join(&rel_path)).is_ok() {
                        let _ = index.mark_delete_completed(&rel_path);
                        deleted.push(rel_path);
                    }
                }
                Ok(deleted)
            })
            .await
            .unwrap_or_default();

        for rel_path in removed {
            if let Some(len) = self.tombstones.remove(&rel_path) {
                let _ = self.ledger.release_tombstone(len);
            }
        }
    }

    fn evict_ready_to_fit_sync(&mut self, target_max_bytes: u64) -> Result<(), CacheError> {
        let index = Arc::clone(self.require_index()?);
        let mut candidates: Vec<PublishedEntryRecord> =
            self.ready_entries.values().cloned().collect();
        candidates.sort_by_key(|r| (r.last_access, r.created_at_unix, r.rel_path.clone()));

        for candidate in candidates {
            let current_total = self
                .ledger
                .total_charged()
                .ok_or_else(|| CacheError::Index("byte ledger overflow".to_string()))?;
            if current_total <= target_max_bytes {
                break;
            }
            let pending = PendingDeleteRecord {
                rel_path: candidate.rel_path.clone(),
                byte_len: candidate.byte_len,
            };
            index.hide_and_queue_deletes(
                std::slice::from_ref(&candidate.entry_key),
                std::slice::from_ref(&pending),
            )?;

            self.ready_entries.remove(&candidate.entry_key);
            self.ledger.release_published(candidate.byte_len)?;
            self.ledger.charge_tombstone(candidate.byte_len)?;

            match self
                .fs
                .remove_file(&self.root_dir.join(&candidate.rel_path))
            {
                Ok(()) => {
                    let _ = index.mark_delete_completed(&candidate.rel_path);
                    self.ledger.release_tombstone(candidate.byte_len)?;
                }
                Err(_) => {
                    self.tombstones
                        .insert(candidate.rel_path, candidate.byte_len);
                }
            }
        }
        Ok(())
    }

    async fn evict_ready_to_fit(&mut self, target_max_bytes: u64) -> Result<(), CacheError> {
        self.retry_pending_deletes().await;
        let current_total = self
            .ledger
            .total_charged()
            .ok_or_else(|| CacheError::Index("byte ledger overflow".to_string()))?;
        if current_total <= target_max_bytes {
            return Ok(());
        }

        let mut candidates: Vec<PublishedEntryRecord> =
            self.ready_entries.values().cloned().collect();
        candidates.sort_by_key(|r| (r.last_access, r.created_at_unix, r.rel_path.clone()));

        let mut to_evict = Vec::new();
        let mut projected_total = current_total;
        for candidate in candidates {
            if projected_total <= target_max_bytes {
                break;
            }
            projected_total = projected_total.saturating_sub(candidate.byte_len);
            to_evict.push(candidate);
        }

        if to_evict.is_empty() {
            return Ok(());
        }

        let entry_keys: Vec<CacheEntryKey> = to_evict.iter().map(|c| c.entry_key.clone()).collect();
        let deletes: Vec<PendingDeleteRecord> = to_evict
            .iter()
            .map(|c| PendingDeleteRecord {
                rel_path: c.rel_path.clone(),
                byte_len: c.byte_len,
            })
            .collect();

        let index = Arc::clone(self.require_index()?);
        let fs = Arc::clone(&self.fs);
        let root_dir = self.root_dir.clone();

        let outcomes = self
            .run_blocking(move || {
                index.hide_and_queue_deletes(&entry_keys, &deletes)?;
                let mut results = Vec::with_capacity(deletes.len());
                for del in deletes {
                    let removed = fs.remove_file(&root_dir.join(&del.rel_path)).is_ok();
                    if removed {
                        let _ = index.mark_delete_completed(&del.rel_path);
                    }
                    results.push((del, removed));
                }
                Ok(results)
            })
            .await?;

        for candidate in to_evict {
            self.ready_entries.remove(&candidate.entry_key);
            self.ledger.release_published(candidate.byte_len)?;
            self.ledger.charge_tombstone(candidate.byte_len)?;
        }
        for (del, removed) in outcomes {
            if removed {
                self.ledger.release_tombstone(del.byte_len)?;
            } else {
                self.tombstones.insert(del.rel_path, del.byte_len);
            }
        }
        Ok(())
    }

    async fn handle_get(
        &mut self,
        key: CacheKey,
        scope: SessionScope,
    ) -> Result<CacheLookup, CacheError> {
        if self.shutting_down {
            return Err(CacheError::ShuttingDown);
        }
        if scope != self.current_scope {
            return Ok(CacheLookup::Miss);
        }

        let entry_key = CacheEntryKey::new(CacheNamespace::from_scope(&scope), key);
        let Some(record) = self.ready_entries.get(&entry_key).cloned() else {
            return Ok(CacheLookup::Miss);
        };

        let max_bytes = entry_key.key.class.byte_limit();
        let expected_len = record.byte_len;
        let expected_crc = record.crc32;
        let abs_path = self.root_dir.join(&record.rel_path);
        let fs = Arc::clone(&self.fs);
        let index = Arc::clone(self.require_index()?);
        let next_tick = self.alloc_access_seq();
        let rel_path = record.rel_path.clone();
        let touch_key = entry_key.clone();

        enum ReadOutcome {
            Valid(Vec<u8>),
            CorruptOrMissing,
        }

        let outcome = self
            .run_blocking(move || match fs.read_bounded(&abs_path, max_bytes) {
                Ok(bytes)
                    if bytes.len() as u64 == expected_len && crc32_ieee(&bytes) == expected_crc =>
                {
                    index.touch_last_access(&touch_key, next_tick)?;
                    Ok(ReadOutcome::Valid(bytes))
                }
                Ok(_)
                | Err(CacheIoReadError::NotFound)
                | Err(CacheIoReadError::TooLarge)
                | Err(CacheIoReadError::Io(_)) => Ok(ReadOutcome::CorruptOrMissing),
            })
            .await?;

        match outcome {
            ReadOutcome::Valid(bytes) => {
                if let Some(entry) = self.ready_entries.get_mut(&entry_key) {
                    entry.last_access = next_tick;
                }
                let stale = self.connectivity == CacheConnectivity::Offline;
                Ok(CacheLookup::Hit {
                    scope: self.current_scope.clone(),
                    bytes,
                    stale,
                })
            }
            ReadOutcome::CorruptOrMissing => {
                self.remove_single_ready_entry(&entry_key, &rel_path, expected_len)
                    .await?;
                Ok(CacheLookup::CorruptPurged)
            }
        }
    }

    async fn remove_single_ready_entry(
        &mut self,
        entry_key: &CacheEntryKey,
        rel_path: &str,
        byte_len: u64,
    ) -> Result<(), CacheError> {
        let index = Arc::clone(self.require_index()?);
        let fs = Arc::clone(&self.fs);
        let abs_path = self.root_dir.join(rel_path);
        let key_clone = entry_key.clone();
        let pending = PendingDeleteRecord {
            rel_path: rel_path.to_string(),
            byte_len,
        };

        let removed = self
            .run_blocking(move || {
                index.hide_and_queue_deletes(&[key_clone], std::slice::from_ref(&pending))?;
                let removed = fs.remove_file(&abs_path).is_ok();
                if removed {
                    let _ = index.mark_delete_completed(&pending.rel_path);
                }
                Ok(removed)
            })
            .await?;

        if self.ready_entries.remove(entry_key).is_some() {
            self.ledger.release_published(byte_len)?;
            self.ledger.charge_tombstone(byte_len)?;
        }
        if removed {
            self.ledger.release_tombstone(byte_len)?;
        } else {
            self.tombstones.insert(rel_path.to_string(), byte_len);
        }
        Ok(())
    }

    async fn handle_invalidate(
        &mut self,
        key: CacheKey,
        scope: SessionScope,
    ) -> Result<bool, CacheError> {
        if self.shutting_down {
            return Err(CacheError::ShuttingDown);
        }
        if scope != self.current_scope {
            return Ok(false);
        }
        let entry_key = CacheEntryKey::new(CacheNamespace::from_scope(&scope), key);
        let Some(record) = self.ready_entries.get(&entry_key).cloned() else {
            return Ok(false);
        };
        self.remove_single_ready_entry(&entry_key, &record.rel_path, record.byte_len)
            .await?;
        Ok(true)
    }

    async fn handle_begin_put(
        &mut self,
        caller_id: u64,
        key: CacheKey,
        scope: SessionScope,
        expected_len: Option<u64>,
        stream: Box<dyn CacheByteStream>,
        reply: oneshot::Sender<Result<CachePutResult, CacheError>>,
    ) {
        if self.cancelled_callers.remove(&caller_id) || reply.is_closed() {
            let _ = reply.send(Err(CacheError::Cancelled));
            return;
        }
        if self.shutting_down {
            let _ = reply.send(Err(CacheError::ShuttingDown));
            return;
        }
        if scope != self.current_scope {
            let _ = reply.send(Err(CacheError::ScopeMismatch));
            return;
        }

        let class_max = key.class.byte_limit() as u64;
        let charge = match expected_len {
            Some(len) if len > class_max => {
                let _ = reply.send(Err(CacheError::PayloadTooLarge {
                    actual: len,
                    limit: class_max,
                }));
                return;
            }
            Some(len) => len,
            None => class_max,
        };

        if charge > self.cap_bytes {
            let _ = reply.send(Err(CacheError::CapacityExceeded));
            return;
        }

        // Check whether safe eviction can even possibly make room before evicting anything.
        let Some(non_evictable) = self.ledger.non_evictable_bytes() else {
            let _ = reply.send(Err(CacheError::Index("byte ledger overflow".to_string())));
            return;
        };
        if non_evictable
            .checked_add(charge)
            .is_none_or(|t| t > self.cap_bytes)
        {
            // Try retrying pending tombstones first in case disk deletions can now succeed.
            self.retry_pending_deletes().await;
            let Some(non_evictable) = self.ledger.non_evictable_bytes() else {
                let _ = reply.send(Err(CacheError::Index("byte ledger overflow".to_string())));
                return;
            };
            if non_evictable
                .checked_add(charge)
                .is_none_or(|t| t > self.cap_bytes)
            {
                let _ = reply.send(Err(CacheError::CapacityExceeded));
                return;
            }
        }

        let max_allowed_existing = self.cap_bytes - charge;
        if let Err(err) = self.evict_ready_to_fit(max_allowed_existing).await {
            let _ = reply.send(Err(err));
            return;
        }

        let Some(current_total) = self.ledger.total_charged() else {
            let _ = reply.send(Err(CacheError::Index("byte ledger overflow".to_string())));
            return;
        };
        if current_total
            .checked_add(charge)
            .is_none_or(|total| total > self.cap_bytes)
        {
            let _ = reply.send(Err(CacheError::CapacityExceeded));
            return;
        }

        let reservation_id = self.next_reservation_id;
        self.next_reservation_id = self.next_reservation_id.saturating_add(1);

        let staging_rel_path = format!(
            "staging/{reservation_id:016x}_{}.part",
            uuid::Uuid::new_v4()
        );
        let staging_abs_path = self.root_dir.join(&staging_rel_path);
        let entry_key = CacheEntryKey::new(CacheNamespace::from_scope(&scope), key);
        let cancel_token = CancelToken::new();

        if let Err(err) = self.ledger.charge_reserved(charge) {
            let _ = reply.send(Err(err));
            return;
        }

        let writer_fs = Arc::clone(&self.fs);
        let writer_cancel = cancel_token.clone();
        let abort_handle = self.join_set.spawn(async move {
            let outcome =
                run_supervised_writer(writer_fs, staging_abs_path, charge, stream, writer_cancel)
                    .await;
            (reservation_id, outcome)
        });
        let task_id = abort_handle.id();

        self.task_to_reservation.insert(task_id, reservation_id);
        self.caller_to_reservation.insert(caller_id, reservation_id);
        self.live_reservations.insert(
            reservation_id,
            LiveReservation {
                reservation_id,
                caller_id,
                entry_key,
                scope,
                charge,
                staging_rel_path,
                cancel_token,
                cancelled: false,
                watchdog_deadline: None,
                overdue: false,
                reply_tx: Some(reply),
            },
        );
    }

    fn cancel_by_caller(&mut self, caller_id: u64) {
        if let Some(reservation_id) = self.caller_to_reservation.remove(&caller_id) {
            self.cancel_reservation(reservation_id);
        } else {
            self.cancelled_callers.insert(caller_id);
        }
    }

    fn cancel_reservation(&mut self, reservation_id: u64) {
        if let Some(res) = self.live_reservations.get_mut(&reservation_id)
            && !res.cancelled
        {
            res.cancelled = true;
            res.watchdog_deadline = Some(Instant::now() + WATCHDOG_TIMEOUT);
            res.cancel_token.cancel();
        }
    }

    async fn handle_set_cap(&mut self, new_cap: u64) -> Result<(), CacheError> {
        if self.shutting_down {
            return Err(CacheError::ShuttingDown);
        }
        let new_cap = validate_cache_cap(new_cap)?;
        self.retry_pending_deletes().await;

        let non_evictable = self
            .ledger
            .non_evictable_bytes()
            .ok_or_else(|| CacheError::Index("byte ledger overflow".to_string()))?;
        if non_evictable > new_cap {
            return Err(CacheError::CapReductionRejected {
                requested: new_cap,
                charged: non_evictable,
            });
        }

        if new_cap < self.cap_bytes {
            self.evict_ready_to_fit(new_cap).await?;
            let current_total = self
                .ledger
                .total_charged()
                .ok_or_else(|| CacheError::Index("byte ledger overflow".to_string()))?;
            if current_total > new_cap {
                return Err(CacheError::CapReductionRejected {
                    requested: new_cap,
                    charged: current_total,
                });
            }
        }

        self.cap_bytes = new_cap;
        Ok(())
    }

    async fn handle_scope_transition(
        &mut self,
        transition: CacheScopeTransition,
    ) -> Result<(), CacheError> {
        if self.shutting_down {
            return Err(CacheError::ShuttingDown);
        }

        self.current_scope = transition.new_scope;
        let now = Instant::now();
        for res in self.live_reservations.values_mut() {
            if res.scope != self.current_scope && !res.cancelled {
                res.cancelled = true;
                res.watchdog_deadline = Some(now + WATCHDOG_TIMEOUT);
                res.cancel_token.cancel();
            }
        }

        if let Some(account_id) = transition.purge_account {
            self.purge_account_entries(&account_id).await?;
        }
        Ok(())
    }

    async fn purge_account_entries(&mut self, account_id: &AccountId) -> Result<(), CacheError> {
        let target_ns = CacheNamespace::Account(account_id.clone());
        let outgoing: Vec<PublishedEntryRecord> = self
            .ready_entries
            .values()
            .filter(|r| r.entry_key.namespace == target_ns)
            .cloned()
            .collect();
        if outgoing.is_empty() {
            return Ok(());
        }

        let entry_keys: Vec<CacheEntryKey> = outgoing.iter().map(|r| r.entry_key.clone()).collect();
        let deletes: Vec<PendingDeleteRecord> = outgoing
            .iter()
            .map(|r| PendingDeleteRecord {
                rel_path: r.rel_path.clone(),
                byte_len: r.byte_len,
            })
            .collect();

        let index = Arc::clone(self.require_index()?);
        let fs = Arc::clone(&self.fs);
        let root_dir = self.root_dir.clone();

        let outcomes = self
            .run_blocking(move || {
                index.hide_and_queue_deletes(&entry_keys, &deletes)?;
                let mut results = Vec::with_capacity(deletes.len());
                for del in deletes {
                    let removed = fs.remove_file(&root_dir.join(&del.rel_path)).is_ok();
                    if removed {
                        let _ = index.mark_delete_completed(&del.rel_path);
                    }
                    results.push((del, removed));
                }
                Ok(results)
            })
            .await?;

        for record in outgoing {
            self.ready_entries.remove(&record.entry_key);
            self.ledger.release_published(record.byte_len)?;
            self.ledger.charge_tombstone(record.byte_len)?;
        }
        for (del, removed) in outcomes {
            if removed {
                self.ledger.release_tombstone(del.byte_len)?;
            } else {
                self.tombstones.insert(del.rel_path, del.byte_len);
            }
        }
        Ok(())
    }

    fn handle_watchdog_tick(&mut self, now: Instant) {
        for res in self.live_reservations.values_mut() {
            if res.cancelled
                && !res.overdue
                && res
                    .watchdog_deadline
                    .is_some_and(|deadline| deadline <= now)
            {
                res.overdue = true;
                tracing::warn!(
                    reservation_id = res.reservation_id,
                    charge = res.charge,
                    "cancelled cache writer exceeded 10s watchdog; retaining reservation charge until termination"
                );
            }
        }
    }

    async fn handle_writer_join(
        &mut self,
        join_res: Result<(TaskId, (u64, WriterExit)), tokio::task::JoinError>,
    ) {
        let (reservation_id, outcome) = match join_res {
            Ok((task_id, (reservation_id, outcome))) => {
                self.task_to_reservation.remove(&task_id);
                (reservation_id, outcome)
            }
            Err(join_err) => {
                let task_id = join_err.id();
                let Some(reservation_id) = self.task_to_reservation.remove(&task_id) else {
                    return;
                };
                (reservation_id, WriterExit::PanickedOrAborted)
            }
        };

        self.on_writer_finished(reservation_id, outcome).await;
    }

    async fn on_writer_finished(&mut self, reservation_id: u64, outcome: WriterExit) {
        let Some(mut res) = self.live_reservations.remove(&reservation_id) else {
            // Retired or duplicate completion events have no effect.
            return;
        };
        self.caller_to_reservation.remove(&res.caller_id);

        let reply_tx = res.reply_tx.take();
        let can_publish = !self.shutting_down && !res.cancelled && res.scope == self.current_scope;

        if let WriterExit::Complete { actual_len, crc32 } = outcome
            && can_publish
            && actual_len <= res.charge
        {
            let obj_rel_path = format!("objects/{}.bin", uuid::Uuid::new_v4());
            let staging_abs = self.root_dir.join(&res.staging_rel_path);
            let obj_abs = self.root_dir.join(&obj_rel_path);
            let access_seq = self.alloc_access_seq();
            let created_at_unix = self.clock.now_unix_secs();

            let new_record = PublishedEntryRecord {
                entry_key: res.entry_key.clone(),
                rel_path: obj_rel_path.clone(),
                byte_len: actual_len,
                crc32,
                last_access: access_seq,
                created_at_unix,
            };

            let prev_record = self.ready_entries.get(&res.entry_key).cloned();
            let prev_tombstone = prev_record.as_ref().map(|prev| PendingDeleteRecord {
                rel_path: prev.rel_path.clone(),
                byte_len: prev.byte_len,
            });

            let fs = Arc::clone(&self.fs);
            let index = match self.require_index() {
                Ok(idx) => Arc::clone(idx),
                Err(err) => {
                    self.cleanup_failed_staging(&res).await;
                    if let Some(tx) = reply_tx {
                        let _ = tx.send(Err(err));
                    }
                    return;
                }
            };

            enum PublishResult {
                Committed { prev_removed: bool },
                RenameFailed { staging_removed: bool, err: String },
                IndexFailed { obj_removed: bool, err: CacheError },
            }

            let root_dir = self.root_dir.clone();
            let record_for_db = new_record.clone();
            let prev_for_db = prev_tombstone.clone();
            let staging_rel_for_db = res.staging_rel_path.clone();
            let obj_rel_for_db = obj_rel_path.clone();
            let publish_outcome = self
                .run_blocking(move || {
                    if let Err(err) = fs.rename_no_overwrite(&staging_abs, &obj_abs) {
                        let staging_removed = fs.remove_file(&staging_abs).is_ok();
                        if !staging_removed {
                            let _ = index.record_pending_deletes(&[PendingDeleteRecord {
                                rel_path: staging_rel_for_db,
                                byte_len: actual_len,
                            }]);
                        }
                        return Ok(PublishResult::RenameFailed {
                            staging_removed,
                            err: err.to_string(),
                        });
                    }

                    if let Err(err) = index.commit_publish(&record_for_db, prev_for_db.as_ref()) {
                        let obj_removed = fs.remove_file(&obj_abs).is_ok();
                        if !obj_removed {
                            let _ = index.record_pending_deletes(&[PendingDeleteRecord {
                                rel_path: obj_rel_for_db,
                                byte_len: actual_len,
                            }]);
                        }
                        return Ok(PublishResult::IndexFailed { obj_removed, err });
                    }

                    let prev_removed = match prev_for_db {
                        Some(prev) => {
                            let removed = fs.remove_file(&root_dir.join(&prev.rel_path)).is_ok();
                            if removed {
                                let _ = index.mark_delete_completed(&prev.rel_path);
                            }
                            removed
                        }
                        None => true,
                    };

                    Ok(PublishResult::Committed { prev_removed })
                })
                .await;

            match publish_outcome {
                Ok(PublishResult::Committed { prev_removed }) => {
                    let _ = self.ledger.release_reserved(res.charge);
                    if let Some(prev) = prev_record {
                        let _ = self.ledger.release_published(prev.byte_len);
                        if !prev_removed {
                            let _ = self.ledger.charge_tombstone(prev.byte_len);
                            self.tombstones.insert(prev.rel_path, prev.byte_len);
                        }
                    }
                    let _ = self.ledger.charge_published(actual_len);
                    self.ready_entries.insert(res.entry_key, new_record);
                    if let Some(tx) = reply_tx {
                        let _ = tx.send(Ok(CachePutResult {
                            scope: res.scope,
                            byte_len: actual_len,
                        }));
                    }
                }
                Ok(PublishResult::RenameFailed {
                    staging_removed,
                    err,
                }) => {
                    let _ = self.ledger.release_reserved(res.charge);
                    if !staging_removed {
                        let _ = self.ledger.charge_tombstone(actual_len);
                        self.tombstones.insert(res.staging_rel_path, actual_len);
                    }
                    if let Some(tx) = reply_tx {
                        let _ = tx.send(Err(CacheError::Io(err)));
                    }
                }
                Ok(PublishResult::IndexFailed { obj_removed, err }) => {
                    let _ = self.ledger.release_reserved(res.charge);
                    if !obj_removed {
                        let _ = self.ledger.charge_tombstone(actual_len);
                        self.tombstones.insert(obj_rel_path, actual_len);
                    }
                    if let Some(tx) = reply_tx {
                        let _ = tx.send(Err(err));
                    }
                }
                Err(err) => {
                    let _ = self.ledger.release_reserved(res.charge);
                    if let Some(tx) = reply_tx {
                        let _ = tx.send(Err(err));
                    }
                }
            }
            return;
        }

        // Non-publishing path: clean up staging file and release/tombstone charge.
        self.cleanup_failed_staging(&res).await;

        if let Some(tx) = reply_tx {
            let err = if self.shutting_down {
                CacheError::ShuttingDown
            } else if res.cancelled {
                if res.scope != self.current_scope {
                    CacheError::ScopeMismatch
                } else {
                    CacheError::Cancelled
                }
            } else if res.scope != self.current_scope {
                CacheError::ScopeMismatch
            } else {
                match outcome {
                    WriterExit::Cancelled => CacheError::Cancelled,
                    WriterExit::ExceededReservation { attempted, limit } => {
                        CacheError::PayloadTooLarge {
                            actual: attempted,
                            limit,
                        }
                    }
                    WriterExit::SourceError(CacheSourceError::Cancelled) => CacheError::Cancelled,
                    WriterExit::SourceError(CacheSourceError::Io(msg)) => {
                        CacheError::WriterFailed(msg)
                    }
                    WriterExit::Failed(msg) => CacheError::WriterFailed(msg),
                    WriterExit::PanickedOrAborted => {
                        CacheError::WriterFailed("supervised cache writer panicked".to_string())
                    }
                    WriterExit::Complete { .. } => CacheError::ScopeMismatch,
                }
            };
            let _ = tx.send(Err(err));
        }
    }

    async fn cleanup_failed_staging(&mut self, res: &LiveReservation) {
        let staging_abs = self.root_dir.join(&res.staging_rel_path);
        let staging_rel = res.staging_rel_path.clone();
        let fallback_len = res.charge;
        let fs = Arc::clone(&self.fs);
        let index = self.index.clone();

        let (removed, actual_len) = self
            .run_blocking(move || {
                let removed = fs.remove_file(&staging_abs).is_ok();
                if removed {
                    if let Some(idx) = &index {
                        let _ = idx.mark_delete_completed(&staging_rel);
                    }
                    Ok((true, 0))
                } else {
                    let actual_len = fs
                        .file_len(&staging_abs)
                        .ok()
                        .flatten()
                        .unwrap_or(fallback_len);
                    if let Some(idx) = &index {
                        let _ = idx.record_pending_deletes(&[PendingDeleteRecord {
                            rel_path: staging_rel,
                            byte_len: actual_len,
                        }]);
                    }
                    Ok((false, actual_len))
                }
            })
            .await
            .unwrap_or((false, fallback_len));

        let _ = self.ledger.release_reserved(res.charge);
        if !removed {
            let _ = self.ledger.charge_tombstone(actual_len);
            self.tombstones
                .insert(res.staging_rel_path.clone(), actual_len);
        }
    }

    async fn perform_shutdown(&mut self) {
        self.shutting_down = true;
        let reservation_ids: Vec<u64> = self.live_reservations.keys().copied().collect();
        for id in reservation_ids {
            self.cancel_reservation(id);
        }

        while let Some(join_res) = self.join_set.join_next_with_id().await {
            self.handle_writer_join(join_res).await;
        }

        // Close SQLite index before releasing the exclusive root lock.
        self.index.take();
        self.lock_guard.take();
    }

    #[cfg(any(test, feature = "test-util"))]
    async fn handle_synthetic_finished_for_test(
        &mut self,
        reservation_id: u64,
        scope: SessionScope,
        key: CacheKey,
        actual_len: u64,
        crc32: u32,
    ) {
        let _ = (scope, key);
        self.on_writer_finished(reservation_id, WriterExit::Complete { actual_len, crc32 })
            .await;
    }
}

async fn close_writer_blocking(writer: Option<Box<dyn StagingWriter>>) -> Result<(), String> {
    let Some(writer) = writer else {
        return Ok(());
    };
    tokio::task::spawn_blocking(move || writer.close().map_err(|e| e.to_string()))
        .await
        .map_err(|e| format!("writer close task failed: {e}"))?
}

async fn run_supervised_writer(
    fs: Arc<dyn CacheFs>,
    staging_abs_path: PathBuf,
    max_charge: u64,
    mut stream: Box<dyn CacheByteStream>,
    mut cancel_token: CancelToken,
) -> WriterExit {
    if cancel_token.is_cancelled() {
        return WriterExit::Cancelled;
    }

    let open_fs = Arc::clone(&fs);
    let open_path = staging_abs_path.clone();
    let writer_res =
        tokio::task::spawn_blocking(move || open_fs.create_new_writer(&open_path)).await;
    let mut writer: Option<Box<dyn StagingWriter>> = match writer_res {
        Ok(Ok(w)) => Some(w),
        Ok(Err(err)) => return WriterExit::Failed(err.to_string()),
        Err(_) => return WriterExit::PanickedOrAborted,
    };

    let mut actual_len: u64 = 0;
    let mut hasher = Crc32Hasher::new();

    loop {
        if cancel_token.is_cancelled() {
            let _ = close_writer_blocking(writer.take()).await;
            return WriterExit::Cancelled;
        }

        let next_chunk = tokio::select! {
            biased;
            () = cancel_token.cancelled() => {
                let _ = close_writer_blocking(writer.take()).await;
                return WriterExit::Cancelled;
            }
            chunk = stream.next_chunk() => chunk,
        };

        match next_chunk {
            Ok(Some(chunk)) => {
                let projected = match actual_len.checked_add(chunk.len() as u64) {
                    Some(v) => v,
                    None => {
                        let _ = close_writer_blocking(writer.take()).await;
                        return WriterExit::ExceededReservation {
                            attempted: u64::MAX,
                            limit: max_charge,
                        };
                    }
                };
                if projected > max_charge {
                    let _ = close_writer_blocking(writer.take()).await;
                    return WriterExit::ExceededReservation {
                        attempted: projected,
                        limit: max_charge,
                    };
                }
                if cancel_token.is_cancelled() {
                    let _ = close_writer_blocking(writer.take()).await;
                    return WriterExit::Cancelled;
                }

                hasher.update(&chunk);
                let Some(mut w) = writer.take() else {
                    return WriterExit::Failed("staging writer handle missing".to_string());
                };
                let write_res = tokio::task::spawn_blocking(move || {
                    w.write_all(&chunk)?;
                    Ok::<Box<dyn StagingWriter>, std::io::Error>(w)
                })
                .await;
                match write_res {
                    Ok(Ok(returned)) => {
                        writer = Some(returned);
                        actual_len = projected;
                    }
                    Ok(Err(err)) => {
                        return WriterExit::Failed(err.to_string());
                    }
                    Err(_) => {
                        return WriterExit::PanickedOrAborted;
                    }
                }
            }
            Ok(None) => {
                if cancel_token.is_cancelled() {
                    let _ = close_writer_blocking(writer.take()).await;
                    return WriterExit::Cancelled;
                }
                return match close_writer_blocking(writer.take()).await {
                    Ok(()) => WriterExit::Complete {
                        actual_len,
                        crc32: hasher.finalize(),
                    },
                    Err(msg) => WriterExit::Failed(msg),
                };
            }
            Err(err) => {
                let _ = close_writer_blocking(writer.take()).await;
                return WriterExit::SourceError(err);
            }
        }
    }
}
