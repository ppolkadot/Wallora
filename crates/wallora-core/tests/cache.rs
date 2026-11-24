use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use wallora_core::domain::{
    AccountId, CollectionSummary, CredentialStore, InMemoryCredentialStore, SecretString,
    SessionScope, UsernameVerification,
};
use wallora_core::provider::{ImageClass, ProviderError};
use wallora_core::services::cache::{
    CacheByteStream, CacheChunkFuture, CacheClock, CacheConfig, CacheConnectivity, CacheDirEntry,
    CacheEntryKey, CacheError, CacheFs, CacheIndex, CacheIndexSnapshot, CacheIoReadError, CacheKey,
    CacheLockError, CacheLockGuard, CacheLookup, CacheScopeTransition, CacheSourceError,
    CacheStore, DEFAULT_CACHE_CAP_BYTES, HARD_MAX_CACHE_CAP_BYTES, PendingDeleteRecord,
    PublishedEntryRecord, RealCacheFs, SessionCacheSink, SqliteCacheIndex, StagingWriter,
    WATCHDOG_TIMEOUT,
};
use wallora_core::services::{AccountRefreshPayload, SessionManager, SignInProbe};
use wallora_core::storage::{SessionStorage, SqliteStore};

struct TempCacheDir(PathBuf);

impl TempCacheDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("wallora-cache-test-{}", uuid::Uuid::new_v4()));
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempCacheDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FakeClock {
    now: AtomicI64,
}

impl CacheClock for FakeClock {
    fn now_unix_secs(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

const TEST_STEP_TIMEOUT: Duration = Duration::from_secs(30);

async fn bounded<F: Future>(label: &str, fut: F) -> F::Output {
    tokio::time::timeout(TEST_STEP_TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| panic!("test step timed out after {TEST_STEP_TIMEOUT:?}: {label}"))
}

struct CloseGate {
    pair: Mutex<bool>,
    cv: Condvar,
}

impl CloseGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pair: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    fn release(&self) {
        let mut released = self.pair.lock().expect("lock");
        *released = true;
        self.cv.notify_all();
    }

    fn wait(&self) {
        let mut released = self.pair.lock().expect("lock");
        while !*released {
            released = self.cv.wait(released).expect("wait");
        }
    }
}

struct ObservableLockGuard {
    _inner: Box<dyn CacheLockGuard>,
    lock_held: Arc<AtomicBool>,
}

impl CacheLockGuard for ObservableLockGuard {}

impl Drop for ObservableLockGuard {
    fn drop(&mut self) {
        self.lock_held.store(false, Ordering::SeqCst);
    }
}

struct ObservableWriter {
    inner: Box<dyn StagingWriter>,
    open_handles: Arc<AtomicUsize>,
    open_handles_tx: watch::Sender<usize>,
    hold_close_gate: Option<Arc<CloseGate>>,
    close_entered_count: Arc<AtomicUsize>,
    close_entered_tx: watch::Sender<usize>,
    drop_closed: bool,
}

impl StagingWriter for ObservableWriter {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.inner.write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn close(mut self: Box<Self>) -> io::Result<()> {
        let entered = self.close_entered_count.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.close_entered_tx.send(entered);
        if let Some(gate) = &self.hold_close_gate {
            gate.wait();
        }
        let res = {
            let dummy = Box::new(NoopWriter);
            let inner = std::mem::replace(&mut self.inner, dummy);
            inner.close()
        };
        if !self.drop_closed {
            self.drop_closed = true;
            let remaining = self.open_handles.fetch_sub(1, Ordering::SeqCst) - 1;
            let _ = self.open_handles_tx.send(remaining);
        }
        res
    }
}

impl Drop for ObservableWriter {
    fn drop(&mut self) {
        if !self.drop_closed {
            self.drop_closed = true;
            let remaining = self.open_handles.fetch_sub(1, Ordering::SeqCst) - 1;
            let _ = self.open_handles_tx.send(remaining);
        }
    }
}

struct NoopWriter;

impl StagingWriter for NoopWriter {
    fn write_all(&mut self, _buf: &[u8]) -> io::Result<()> {
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn close(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }
}

struct FaultyCacheFs {
    real: RealCacheFs,
    writers_opened: AtomicUsize,
    writers_opened_tx: watch::Sender<usize>,
    _writers_opened_rx: watch::Receiver<usize>,
    open_writer_handles: Arc<AtomicUsize>,
    open_handles_tx: watch::Sender<usize>,
    _open_handles_rx: watch::Receiver<usize>,
    close_entered_count: Arc<AtomicUsize>,
    close_entered_tx: watch::Sender<usize>,
    _close_entered_rx: watch::Receiver<usize>,
    lock_held: Arc<AtomicBool>,
    next_close_gate: Mutex<Option<Arc<CloseGate>>>,
    file_removed_tx: watch::Sender<usize>,
    _file_removed_rx: watch::Receiver<usize>,
    fail_remove_prefix: Mutex<Option<String>>,
    read_bounded_calls: AtomicUsize,
    fail_rename: AtomicBool,
    panic_after_rename: AtomicBool,
    removed_paths: Mutex<Vec<PathBuf>>,
    renamed_targets: Mutex<Vec<PathBuf>>,
}

impl Default for FaultyCacheFs {
    fn default() -> Self {
        let (writers_opened_tx, _writers_opened_rx) = watch::channel(0usize);
        let (open_handles_tx, _open_handles_rx) = watch::channel(0usize);
        let (close_entered_tx, _close_entered_rx) = watch::channel(0usize);
        let (file_removed_tx, _file_removed_rx) = watch::channel(0usize);
        Self {
            real: RealCacheFs,
            writers_opened: AtomicUsize::new(0),
            writers_opened_tx,
            _writers_opened_rx,
            open_writer_handles: Arc::new(AtomicUsize::new(0)),
            open_handles_tx,
            _open_handles_rx,
            close_entered_count: Arc::new(AtomicUsize::new(0)),
            close_entered_tx,
            _close_entered_rx,
            lock_held: Arc::new(AtomicBool::new(false)),
            next_close_gate: Mutex::new(None),
            file_removed_tx,
            _file_removed_rx,
            fail_remove_prefix: Mutex::new(None),
            read_bounded_calls: AtomicUsize::new(0),
            fail_rename: AtomicBool::new(false),
            panic_after_rename: AtomicBool::new(false),
            removed_paths: Mutex::new(Vec::new()),
            renamed_targets: Mutex::new(Vec::new()),
        }
    }
}

impl FaultyCacheFs {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn arm_close_gate(&self) -> Arc<CloseGate> {
        let gate = CloseGate::new();
        *self.next_close_gate.lock().expect("lock") = Some(Arc::clone(&gate));
        gate
    }

    fn is_lock_held(&self) -> bool {
        self.lock_held.load(Ordering::SeqCst)
    }

    fn subscribe_writers_opened(&self) -> watch::Receiver<usize> {
        self.writers_opened_tx.subscribe()
    }

    async fn wait_writers_opened(&self, count: usize) {
        let mut rx = self.writers_opened_tx.subscribe();
        bounded("wait_writers_opened", rx.wait_for(|&c| c >= count))
            .await
            .expect("writers opened watch");
    }

    async fn wait_open_handles(&self, expected: usize) {
        let mut rx = self.open_handles_tx.subscribe();
        bounded("wait_open_handles", rx.wait_for(|&c| c == expected))
            .await
            .expect("open handles watch");
    }

    async fn wait_close_entered(&self, count: usize) {
        let mut rx = self.close_entered_tx.subscribe();
        bounded("wait_close_entered", rx.wait_for(|&c| c >= count))
            .await
            .expect("close entered watch");
    }

    fn subscribe_removed(&self) -> watch::Receiver<usize> {
        self.file_removed_tx.subscribe()
    }

    fn set_fail_remove_prefix(&self, prefix: Option<&str>) {
        *self.fail_remove_prefix.lock().expect("lock") = prefix.map(str::to_string);
    }

    fn open_handles(&self) -> usize {
        self.open_writer_handles.load(Ordering::SeqCst)
    }

    fn writers_opened(&self) -> usize {
        self.writers_opened.load(Ordering::SeqCst)
    }

    fn renamed_targets(&self) -> Vec<PathBuf> {
        self.renamed_targets.lock().expect("lock").clone()
    }
}

impl CacheFs for FaultyCacheFs {
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.real.create_dir_all(path)
    }

    fn create_recovery_marker(&self, path: &Path) -> io::Result<bool> {
        self.real.create_recovery_marker(path)
    }

    fn try_lock_exclusive(
        &self,
        lock_path: &Path,
    ) -> Result<Box<dyn CacheLockGuard>, CacheLockError> {
        let inner = self.real.try_lock_exclusive(lock_path)?;
        self.lock_held.store(true, Ordering::SeqCst);
        Ok(Box::new(ObservableLockGuard {
            _inner: inner,
            lock_held: Arc::clone(&self.lock_held),
        }))
    }

    fn create_new_writer(&self, path: &Path) -> io::Result<Box<dyn StagingWriter>> {
        let inner = self.real.create_new_writer(path)?;
        let opened = self.writers_opened.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.writers_opened_tx.send(opened);
        let handles = self.open_writer_handles.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.open_handles_tx.send(handles);
        let hold_close_gate = self.next_close_gate.lock().expect("lock").take();
        Ok(Box::new(ObservableWriter {
            inner,
            open_handles: Arc::clone(&self.open_writer_handles),
            open_handles_tx: self.open_handles_tx.clone(),
            hold_close_gate,
            close_entered_count: Arc::clone(&self.close_entered_count),
            close_entered_tx: self.close_entered_tx.clone(),
            drop_closed: false,
        }))
    }

    fn read_bounded(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, CacheIoReadError> {
        self.read_bounded_calls.fetch_add(1, Ordering::SeqCst);
        self.real.read_bounded(path, max_bytes)
    }

    fn file_len(&self, path: &Path) -> Result<Option<u64>, io::Error> {
        self.real.file_len(path)
    }

    fn rename_no_overwrite(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.fail_rename.load(Ordering::SeqCst) {
            return Err(io::Error::other("injected rename failure"));
        }
        self.real.rename_no_overwrite(from, to)?;
        self.renamed_targets
            .lock()
            .expect("lock")
            .push(to.to_path_buf());
        assert!(
            !self.panic_after_rename.load(Ordering::SeqCst),
            "injected publish panic"
        );
        Ok(())
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        if let Some(prefix) = self.fail_remove_prefix.lock().expect("lock").as_deref() {
            let norm = path.to_string_lossy().replace('\\', "/");
            if norm.contains(prefix) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected delete failure",
                ));
            }
        }
        self.real.remove_file(path)?;
        let count = {
            let mut guard = self.removed_paths.lock().expect("lock");
            guard.push(path.to_path_buf());
            guard.len()
        };
        let _ = self.file_removed_tx.send(count);
        Ok(())
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<CacheDirEntry>> {
        self.real.list_dir(path)
    }
}

struct FaultyCacheIndex {
    inner: SqliteCacheIndex,
    fail_commit_publish: AtomicBool,
    fail_hide: AtomicBool,
}

impl FaultyCacheIndex {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: SqliteCacheIndex::new(),
            fail_commit_publish: AtomicBool::new(false),
            fail_hide: AtomicBool::new(false),
        })
    }
}

impl CacheIndex for FaultyCacheIndex {
    fn open_and_load(&self, db_path: &Path) -> Result<CacheIndexSnapshot, CacheError> {
        self.inner.open_and_load(db_path)
    }

    fn commit_publish(
        &self,
        entry: &PublishedEntryRecord,
        previous_to_tombstone: Option<&PendingDeleteRecord>,
    ) -> Result<(), CacheError> {
        if self.fail_commit_publish.load(Ordering::SeqCst) {
            return Err(CacheError::Index(
                "injected index commit failure".to_string(),
            ));
        }
        self.inner.commit_publish(entry, previous_to_tombstone)
    }

    fn touch_last_access(
        &self,
        entry_key: &CacheEntryKey,
        last_access: u64,
    ) -> Result<(), CacheError> {
        self.inner.touch_last_access(entry_key, last_access)
    }

    fn hide_and_queue_deletes(
        &self,
        entries: &[CacheEntryKey],
        deletes: &[PendingDeleteRecord],
    ) -> Result<(), CacheError> {
        if self.fail_hide.load(Ordering::SeqCst) {
            return Err(CacheError::Index("injected hide failure".into()));
        }
        self.inner.hide_and_queue_deletes(entries, deletes)
    }

    fn record_pending_deletes(&self, deletes: &[PendingDeleteRecord]) -> Result<(), CacheError> {
        self.inner.record_pending_deletes(deletes)
    }

    fn mark_delete_completed(&self, rel_path: &str) -> Result<(), CacheError> {
        self.inner.mark_delete_completed(rel_path)
    }
}

struct PanickingStream {
    first_chunk: Option<Vec<u8>>,
    first_sent: Option<oneshot::Sender<()>>,
}

impl CacheByteStream for PanickingStream {
    fn next_chunk<'a>(&'a mut self) -> CacheChunkFuture<'a> {
        Box::pin(async move {
            if let Some(chunk) = self.first_chunk.take() {
                if let Some(tx) = self.first_sent.take() {
                    let _ = tx.send(());
                }
                return Ok(Some(chunk));
            }
            panic!("simulated cache writer stream panic");
        })
    }
}

fn thumb_key(id: &str) -> CacheKey {
    CacheKey::new(ImageClass::Thumbnail, id).expect("valid key")
}

#[tokio::test]
async fn cap_validation_and_default_two_gb() {
    assert_eq!(DEFAULT_CACHE_CAP_BYTES, 2_000_000_000);
    assert_eq!(HARD_MAX_CACHE_CAP_BYTES, 10_000_000_000);

    let dir = TempCacheDir::new();
    let scope = SessionScope::guest(1);
    assert!(matches!(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(HARD_MAX_CACHE_CAP_BYTES + 1),
        Err(CacheError::CapExceedsHardMax { .. })
    ));

    let store = CacheStore::open(CacheConfig::new(dir.path().to_path_buf(), scope))
        .await
        .expect("open default cache");
    assert_eq!(
        store.stats().await.unwrap().cap_bytes,
        DEFAULT_CACHE_CAP_BYTES
    );

    assert!(matches!(
        store.set_cap(HARD_MAX_CACHE_CAP_BYTES + 1).await,
        Err(CacheError::CapExceedsHardMax { .. })
    ));
    store.set_cap(1024).await.expect("lower cap");
    assert_eq!(store.stats().await.unwrap().cap_bytes, 1024);
    store.shutdown().await;
}

#[tokio::test]
async fn normal_complete_publishes_once_without_overwrite_and_reads_back() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let index = Arc::new(SqliteCacheIndex::new());
    let clock = Arc::new(FakeClock::default());
    let scope = SessionScope::guest(1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(10_000)
            .unwrap(),
        fs.clone(),
        index,
        clock,
    )
    .await
    .expect("open cache");

    let key = thumb_key("wp-1");
    let v1 = vec![0xAA; 128];
    let res1 = store
        .put(
            key.clone(),
            scope.clone(),
            Some(v1.len() as u64),
            v1.clone(),
        )
        .await
        .expect("publish v1");
    assert_eq!(res1.byte_len, 128);

    assert_eq!(
        store.get(key.clone(), scope.clone()).await.unwrap(),
        CacheLookup::Hit {
            scope: scope.clone(),
            bytes: v1,
            stale: false,
        }
    );

    let v2 = vec![0xBB; 256];
    let res2 = store
        .put(
            key.clone(),
            scope.clone(),
            Some(v2.len() as u64),
            v2.clone(),
        )
        .await
        .expect("publish v2");
    assert_eq!(res2.byte_len, 256);

    let targets = fs.renamed_targets();
    assert_eq!(targets.len(), 2);
    assert_ne!(targets[0], targets[1], "publication must never overwrite");
    assert!(!targets[0].exists(), "replaced object must be deleted");

    assert_eq!(
        store.get(key, scope.clone()).await.unwrap(),
        CacheLookup::Hit {
            scope,
            bytes: v2,
            stale: false,
        }
    );
    let stats = store.stats().await.unwrap();
    assert_eq!(stats.published_bytes, 256);
    assert_eq!(stats.reserved_bytes, 0);
    assert_eq!(stats.tombstone_bytes, 0);
    assert_eq!(stats.entry_count, 1);

    store.shutdown().await;
}

#[tokio::test]
async fn concurrent_puts_never_exceed_cap_including_staging() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);
    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(1600)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("open cache");

    let (tx1, rx1) = mpsc::channel(2);
    let (tx2, rx2) = mpsc::channel(2);
    let (tx3, rx3) = mpsc::channel(2);

    let s1 = store.clone();
    let k1 = thumb_key("k1");
    let sc1 = scope.clone();
    let h1 = tokio::spawn(async move { s1.put(k1, sc1, Some(800), rx1).await });

    let s2 = store.clone();
    let k2 = thumb_key("k2");
    let sc2 = scope.clone();
    let h2 = tokio::spawn(async move { s2.put(k2, sc2, Some(800), rx2).await });

    // Wait until both 800-byte writers have opened staging (1600 == cap).
    fs.wait_writers_opened(2).await;

    let err3 = store
        .put(thumb_key("k3"), scope.clone(), Some(1), rx3)
        .await
        .unwrap_err();
    assert_eq!(err3, CacheError::CapacityExceeded);
    assert_eq!(
        fs.writers_opened(),
        2,
        "rejected put must not start a writer"
    );
    drop(tx3);

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.reserved_bytes, 1600);
    assert_eq!(stats.total_charged_bytes, 1600);

    tx1.send(Ok(vec![1u8; 800])).await.unwrap();
    drop(tx1);
    tx2.send(Ok(vec![2u8; 800])).await.unwrap();
    drop(tx2);

    h1.await.unwrap().expect("put 1 succeeds");
    h2.await.unwrap().expect("put 2 succeeds");

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.published_bytes, 1600);
    assert_eq!(stats.reserved_bytes, 0);
    store.shutdown().await;
}

#[tokio::test]
async fn unknown_length_body_larger_than_reservation_aborted() {
    let dir = TempCacheDir::new();
    let scope = SessionScope::guest(1);
    let store = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(10_000_000)
            .unwrap(),
    )
    .await
    .expect("open cache");

    // Unknown length reserves Thumbnail limit (2,000,000 bytes). Sending 2,000,001 aborts.
    let (tx, rx) = mpsc::channel(4);
    tx.send(Ok(vec![0x11; 1_500_000])).await.unwrap();
    tx.send(Ok(vec![0x22; 500_001])).await.unwrap();
    drop(tx);

    let err = store
        .put(thumb_key("oversize-unknown"), scope.clone(), None, rx)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        CacheError::PayloadTooLarge {
            actual: 2_000_001,
            limit: 2_000_000,
        }
    );

    // Explicit reservation smaller than actual stream also aborts before exceeding reservation.
    let err2 = store
        .put(
            thumb_key("oversize-hint"),
            scope.clone(),
            Some(100),
            vec![0x33; 101],
        )
        .await
        .unwrap_err();
    assert_eq!(
        err2,
        CacheError::PayloadTooLarge {
            actual: 101,
            limit: 100,
        }
    );

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.total_charged_bytes, 0);
    assert_eq!(stats.entry_count, 0);
    store.shutdown().await;
}

#[tokio::test]
async fn lru_eviction_respects_last_access_and_eviction_arithmetic() {
    let dir = TempCacheDir::new();
    let scope = SessionScope::guest(1);
    let store = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(250)
            .unwrap(),
    )
    .await
    .expect("open cache");

    store
        .put(thumb_key("a"), scope.clone(), Some(100), vec![1; 100])
        .await
        .unwrap();
    store
        .put(thumb_key("b"), scope.clone(), Some(100), vec![2; 100])
        .await
        .unwrap();

    // Reading "a" advances its last_access ahead of "b".
    assert!(matches!(
        store.get(thumb_key("a"), scope.clone()).await.unwrap(),
        CacheLookup::Hit { .. }
    ));

    // Putting "c" (100 bytes) requires evicting 100 bytes -> must evict "b", keeping "a".
    store
        .put(thumb_key("c"), scope.clone(), Some(100), vec![3; 100])
        .await
        .unwrap();

    assert!(matches!(
        store.get(thumb_key("a"), scope.clone()).await.unwrap(),
        CacheLookup::Hit { .. }
    ));
    assert_eq!(
        store.get(thumb_key("b"), scope.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    assert!(matches!(
        store.get(thumb_key("c"), scope.clone()).await.unwrap(),
        CacheLookup::Hit { .. }
    ));

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.published_bytes, 200);
    assert_eq!(stats.entry_count, 2);
    store.shutdown().await;
}

#[tokio::test]
async fn put_cannot_fit_after_safe_eviction_returns_capacity_without_starting_writer() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);
    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(200)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("open cache");

    store
        .put(thumb_key("ready-50"), scope.clone(), Some(50), vec![9; 50])
        .await
        .unwrap();

    let (tx, rx) = mpsc::channel(2);
    let s1 = store.clone();
    let sc1 = scope.clone();
    let h1 =
        tokio::spawn(async move { s1.put(thumb_key("inflight-120"), sc1, Some(120), rx).await });
    fs.wait_writers_opened(2).await;

    // Non-evictable = 120. 120 + 100 > 200, so safe eviction cannot make room.
    let err = store
        .put(
            thumb_key("too-big-100"),
            scope.clone(),
            Some(100),
            vec![7; 100],
        )
        .await
        .unwrap_err();
    assert_eq!(err, CacheError::CapacityExceeded);
    assert_eq!(fs.writers_opened(), 2, "must not start a writer");

    // Existing ready entry must remain intact because eviction could not satisfy the put.
    assert!(matches!(
        store.get(thumb_key("ready-50"), scope).await.unwrap(),
        CacheLookup::Hit { .. }
    ));

    tx.send(Ok(vec![5; 120])).await.unwrap();
    drop(tx);
    h1.await.unwrap().unwrap();
    store.shutdown().await;
}

#[tokio::test]
async fn index_failure_preserves_previous_entry() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let index = FaultyCacheIndex::new();
    let scope = SessionScope::guest(1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(10_000)
            .unwrap(),
        fs.clone(),
        index.clone(),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("open cache");

    let key = thumb_key("replace-me");
    let v1 = vec![0x11; 64];
    store
        .put(key.clone(), scope.clone(), Some(64), v1.clone())
        .await
        .expect("v1 commits");

    index.fail_commit_publish.store(true, Ordering::SeqCst);
    let err = store
        .put(key.clone(), scope.clone(), Some(64), vec![0x22; 64])
        .await
        .unwrap_err();
    assert!(matches!(err, CacheError::Index(_)));

    // Previous entry must remain completely intact and readable.
    assert_eq!(
        store.get(key, scope.clone()).await.unwrap(),
        CacheLookup::Hit {
            scope,
            bytes: v1,
            stale: false,
        }
    );
    let stats = store.stats().await.unwrap();
    assert_eq!(stats.published_bytes, 64);
    assert_eq!(stats.reserved_bytes, 0);
    assert_eq!(stats.tombstone_bytes, 0);
    assert_eq!(
        std::fs::read_dir(dir.path().join("objects"))
            .unwrap()
            .count(),
        1
    );
    store.shutdown().await;
}

#[tokio::test]
async fn lower_cap_rejected_while_reservations_are_charged() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(1000)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("open cache");

    let (tx, rx) = mpsc::channel(2);
    let s1 = store.clone();
    let sc1 = scope.clone();
    let handle =
        tokio::spawn(async move { s1.put(thumb_key("reserved-700"), sc1, Some(700), rx).await });
    fs.wait_writers_opened(1).await;

    let err = store.set_cap(500).await.unwrap_err();
    assert_eq!(
        err,
        CacheError::CapReductionRejected {
            requested: 500,
            charged: 700,
        }
    );
    assert_eq!(store.stats().await.unwrap().cap_bytes, 1000);

    // Finish writer, then evictable cap reduction succeeds.
    tx.send(Ok(vec![0x55; 700])).await.unwrap();
    drop(tx);
    handle.await.unwrap().unwrap();

    store
        .set_cap(500)
        .await
        .expect("cap reduction succeeds once reservation released");
    let stats = store.stats().await.unwrap();
    assert_eq!(stats.cap_bytes, 500);
    assert_eq!(stats.entry_count, 0);
    assert_eq!(stats.total_charged_bytes, 0);
    store.shutdown().await;
}

#[tokio::test]
async fn second_actor_cannot_reconcile_live_staging() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);

    let store1 = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(5000)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("first actor owns lock");

    let (tx, rx) = mpsc::channel(2);
    let s1 = store1.clone();
    let sc1 = scope.clone();
    let writer_task =
        tokio::spawn(async move { s1.put(thumb_key("live-staging"), sc1, Some(64), rx).await });
    fs.wait_writers_opened(1).await;

    let staging_files_before: Vec<_> = std::fs::read_dir(dir.path().join("staging"))
        .unwrap()
        .collect();
    assert_eq!(staging_files_before.len(), 1);

    // Second actor must fail with Locked and must NOT touch actor 1's staging file.
    let err =
        match CacheStore::open(CacheConfig::new(dir.path().to_path_buf(), scope.clone())).await {
            Ok(_) => panic!("second actor must fail to acquire exclusive lock"),
            Err(err) => err,
        };
    assert_eq!(err, CacheError::Locked);

    let staging_files_after: Vec<_> = std::fs::read_dir(dir.path().join("staging"))
        .unwrap()
        .collect();
    assert_eq!(staging_files_after.len(), 1);

    tx.send(Ok(vec![0x77; 64])).await.unwrap();
    drop(tx);
    writer_task.await.unwrap().unwrap();
    assert!(matches!(
        store1.get(thumb_key("live-staging"), scope).await.unwrap(),
        CacheLookup::Hit { .. }
    ));

    store1.shutdown().await;
}

#[tokio::test]
async fn writer_panic_cleans_up_after_handle_closes() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(5000)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("open cache");

    let (first_tx, first_rx) = oneshot::channel();
    let stream = PanickingStream {
        first_chunk: Some(vec![0xAA; 32]),
        first_sent: Some(first_tx),
    };

    let err = store
        .put(thumb_key("panic-key"), scope.clone(), Some(128), stream)
        .await
        .unwrap_err();
    let _ = first_rx.await;
    assert!(matches!(err, CacheError::WriterFailed(_)));

    assert_eq!(
        fs.open_handles(),
        0,
        "writer handle must be closed on panic unwind"
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        0,
        "staging file must be cleaned up after handle closes"
    );
    let stats = store.stats().await.unwrap();
    assert_eq!(stats.reserved_bytes, 0);
    assert_eq!(stats.live_reservations, 0);
    assert_eq!(stats.entry_count, 0);

    // Actor keeps working normally after a supervised writer panics.
    store
        .put(
            thumb_key("after-panic"),
            scope.clone(),
            Some(16),
            vec![1; 16],
        )
        .await
        .expect("subsequent put succeeds");
    store.shutdown().await;
}

#[tokio::test]
async fn corrupt_entry_is_deleted_on_read_so_caller_can_refetch() {
    let dir = TempCacheDir::new();
    let scope = SessionScope::guest(1);
    let store = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(5000)
            .unwrap(),
    )
    .await
    .expect("open cache");

    let key = thumb_key("corruptible");
    store
        .put(key.clone(), scope.clone(), Some(32), vec![0x42; 32])
        .await
        .unwrap();

    // Corrupt the published object bytes on disk (same length, bad CRC).
    let obj_path = std::fs::read_dir(dir.path().join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(&obj_path, vec![0x99; 32]).unwrap();

    assert_eq!(
        store.get(key.clone(), scope.clone()).await.unwrap(),
        CacheLookup::CorruptPurged
    );
    assert!(!obj_path.exists());
    assert_eq!(store.stats().await.unwrap().total_charged_bytes, 0);

    // Refetch succeeds cleanly.
    store
        .put(key.clone(), scope.clone(), Some(32), vec![0x42; 32])
        .await
        .unwrap();
    assert_eq!(
        store.get(key, scope.clone()).await.unwrap(),
        CacheLookup::Hit {
            scope,
            bytes: vec![0x42; 32],
            stale: false,
        }
    );
    store.shutdown().await;
}

#[tokio::test]
async fn dropped_caller_cancels_writer_through_actor_without_touching_staging() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(5000)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("open cache");

    let (_tx, rx) = mpsc::channel::<Result<Vec<u8>, CacheSourceError>>(2);
    let put_fut = store.put(thumb_key("drop-me"), scope.clone(), Some(256), rx);
    let mut put_task = Box::pin(put_fut);

    let mut opened_rx = fs.subscribe_writers_opened();
    // Poll once to send BeginPut, then wait until writer opens staging.
    bounded("wait drop-me writer open", async {
        tokio::select! {
            _ = &mut put_task => panic!("put should wait on empty stream"),
            res = opened_rx.wait_for(|&c| c >= 1) => { res.expect("watch"); }
        }
    })
    .await;
    assert_eq!(fs.open_handles(), 1);

    let mut removed_rx = fs.subscribe_removed();
    // Drop caller future; CancelOnDrop cancels reservation through the actor.
    drop(put_task);

    fs.wait_open_handles(0).await;
    bounded(
        "wait drop-me staging removed",
        removed_rx.wait_for(|&c| c >= 1),
    )
    .await
    .unwrap();

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.live_reservations, 0);
    assert_eq!(stats.reserved_bytes, 0);
    assert_eq!(stats.entry_count, 0);
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
    store.shutdown().await;
}

#[tokio::test]
async fn failed_cleanup_keeps_bytes_charged_and_is_retried() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(200)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("open cache");

    store
        .put(thumb_key("old"), scope.clone(), Some(120), vec![1; 120])
        .await
        .unwrap();

    // Inject delete failure on objects/ during eviction.
    fs.set_fail_remove_prefix(Some("/objects/"));
    let err = store
        .put(thumb_key("new"), scope.clone(), Some(120), vec![2; 120])
        .await
        .unwrap_err();
    assert_eq!(err, CacheError::CapacityExceeded);

    // "old" was hidden (unreadable) before deletion, and stays charged as a 120-byte tombstone.
    assert_eq!(
        store.get(thumb_key("old"), scope.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    let stats = store.stats().await.unwrap();
    assert_eq!(stats.entry_count, 0);
    assert_eq!(stats.published_bytes, 0);
    assert_eq!(stats.tombstone_bytes, 120);
    assert_eq!(stats.pending_deletes, 1);

    // Clear delete failure: maintenance retries and reclaims the tombstone so "new" fits.
    fs.set_fail_remove_prefix(None);
    store
        .put(thumb_key("new"), scope.clone(), Some(120), vec![2; 120])
        .await
        .expect("retried delete reclaims tombstone capacity");

    let stats = store.stats().await.unwrap();
    assert_eq!(stats.tombstone_bytes, 0);
    assert_eq!(stats.pending_deletes, 0);
    assert_eq!(stats.published_bytes, 120);
    store.shutdown().await;
}

#[derive(Default)]
struct FakeSessionCacheSink {
    transitions: Mutex<Vec<CacheScopeTransition>>,
    purges: Mutex<Vec<AccountId>>,
}

impl FakeSessionCacheSink {
    fn transitions(&self) -> Vec<CacheScopeTransition> {
        self.transitions.lock().expect("lock").clone()
    }

    fn purges(&self) -> Vec<AccountId> {
        self.purges.lock().expect("lock").clone()
    }
}

impl SessionCacheSink for FakeSessionCacheSink {
    fn on_scope_transition(&self, transition: CacheScopeTransition) {
        self.transitions.lock().expect("lock").push(transition);
    }

    fn on_purge_account(&self, account_id: &AccountId) {
        self.purges.lock().expect("lock").push(account_id.clone());
    }
}

#[tokio::test(start_paused = true)]
async fn watchdog_does_not_delete_or_uncharge_live_writer() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let gate = fs.arm_close_gate();
    let acct = AccountId::new("acct-watchdog");
    let scope1 = SessionScope::account(acct.clone(), 1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope1.clone())
            .with_cap_bytes(10_000)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("open cache");

    let (_tx, rx) = mpsc::channel::<Result<Vec<u8>, CacheSourceError>>(2);
    let s1 = store.clone();
    let sc1 = scope1.clone();
    let put_handle =
        tokio::spawn(async move { s1.put(thumb_key("held-writer"), sc1, Some(512), rx).await });
    fs.wait_writers_opened(1).await;

    // Logout transition triggers cancellation, while ObservableWriter holds the file open at close().
    let scope2 = SessionScope::guest(2);
    bounded(
        "watchdog transition_scope",
        store.transition_scope(CacheScopeTransition {
            new_scope: scope2.clone(),
            purge_account: Some(acct),
        }),
    )
    .await
    .unwrap();

    fs.wait_close_entered(1).await;
    assert_eq!(fs.open_handles(), 1);
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        1
    );

    let before_watchdog = store.stats().await.unwrap();
    assert_eq!(before_watchdog.reserved_bytes, 512);
    assert_eq!(before_watchdog.overdue_writers, 0);

    // Advance simulated clock past the 10s watchdog.
    tokio::time::advance(WATCHDOG_TIMEOUT).await;
    let after_watchdog = store.stats().await.unwrap();
    assert_eq!(after_watchdog.overdue_writers, 1);
    assert_eq!(after_watchdog.overdue_reservation_ids, vec![1]);
    assert_eq!(
        after_watchdog.reserved_bytes, 512,
        "watchdog must not uncharge live writer"
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        1,
        "watchdog must not delete live writer staging file"
    );

    // Actor keeps serving other requests while the overdue writer still holds the file open.
    store
        .put(thumb_key("guest-ok"), scope2.clone(), Some(32), vec![7; 32])
        .await
        .expect("actor continues serving while overdue writer is live");

    // Release the held handle -> normal cleanup deletes staging and releases reservation.
    let mut removed_rx = fs.subscribe_removed();
    gate.release();
    fs.wait_open_handles(0).await;
    bounded(
        "wait watchdog staging removed",
        removed_rx.wait_for(|&c| c >= 1),
    )
    .await
    .unwrap();
    let put_err = bounded("wait watchdog put_handle", put_handle)
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(put_err, CacheError::ScopeMismatch);

    let final_stats = store.stats().await.unwrap();
    assert_eq!(final_stats.overdue_writers, 0);
    assert_eq!(final_stats.live_reservations, 0);
    assert_eq!(final_stats.reserved_bytes, 0);
    assert_eq!(final_stats.published_bytes, 32);
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
    store.shutdown().await;
}

#[tokio::test]
async fn shutdown_joins_writers_before_releasing_root_lock() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let gate = fs.arm_close_gate();
    let scope = SessionScope::guest(1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(5000)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("open cache");
    assert!(fs.is_lock_held());

    let (_tx, rx) = mpsc::channel::<Result<Vec<u8>, CacheSourceError>>(2);
    let s1 = store.clone();
    let sc1 = scope.clone();
    let put_task =
        tokio::spawn(async move { s1.put(thumb_key("shutting-down"), sc1, Some(256), rx).await });
    fs.wait_writers_opened(1).await;

    let store_for_shutdown = store.clone();
    let shutdown_task = tokio::spawn(async move {
        store_for_shutdown.shutdown().await;
    });

    // Wait until shutdown signals cancellation and the writer hits close().
    fs.wait_close_entered(1).await;
    assert!(
        !shutdown_task.is_finished(),
        "shutdown must wait for live writer"
    );
    assert!(
        fs.is_lock_held(),
        "root lock must remain held until writers join"
    );

    // Second open must still fail with Locked while shutdown waits on the writer.
    assert!(matches!(
        CacheStore::open(CacheConfig::new(dir.path().to_path_buf(), scope.clone())).await,
        Err(CacheError::Locked)
    ));

    gate.release();
    shutdown_task.await.unwrap();
    let put_err = put_task.await.unwrap().unwrap_err();
    assert_eq!(put_err, CacheError::ShuttingDown);
    assert!(
        !fs.is_lock_held(),
        "root lock released after writers joined"
    );
    assert_eq!(fs.open_handles(), 0);
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        0
    );

    // Now a new actor can open cleanly.
    let reopened = CacheStore::open(CacheConfig::new(dir.path().to_path_buf(), scope))
        .await
        .expect("reopen succeeds after shutdown releases lock");
    reopened.shutdown().await;
}

#[tokio::test]
async fn startup_recovers_rename_without_index_commit() {
    let dir = TempCacheDir::new();
    let scope = SessionScope::guest(1);

    // Seed one valid entry and shut down cleanly.
    let store = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(500)
            .unwrap(),
    )
    .await
    .unwrap();
    store
        .put(
            thumb_key("committed"),
            scope.clone(),
            Some(64),
            vec![0x11; 64],
        )
        .await
        .unwrap();
    store.shutdown().await;

    // Simulate crash after rename into objects/ and before index commit.
    let unindexed_path = dir.path().join("objects").join("orphan_renamed.bin");
    std::fs::write(&unindexed_path, vec![0x99; 300]).unwrap();

    // Case A: delete fails during startup reconciliation, and retained bytes fit cap (500 >= 364).
    let fs_fail_del = FaultyCacheFs::new();
    fs_fail_del.set_fail_remove_prefix(Some("orphan_renamed.bin"));
    let recovered = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(500)
            .unwrap(),
        fs_fail_del,
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .expect("reconcile retains unindexed file as charged tombstone");

    let stats = recovered.stats().await.unwrap();
    assert_eq!(stats.published_bytes, 64);
    assert_eq!(stats.tombstone_bytes, 300);
    assert_eq!(stats.total_charged_bytes, 364);
    assert_eq!(stats.pending_deletes, 1);
    recovered.shutdown().await;

    // Case B: delete succeeds on next startup -> unindexed orphan is removed, committed entry intact.
    let clean = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(500)
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(!unindexed_path.exists());
    let stats = clean.stats().await.unwrap();
    assert_eq!(stats.published_bytes, 64);
    assert_eq!(stats.tombstone_bytes, 0);
    assert_eq!(stats.pending_deletes, 0);
    assert!(matches!(
        clean
            .get(thumb_key("committed"), scope.clone())
            .await
            .unwrap(),
        CacheLookup::Hit { .. }
    ));
    clean.shutdown().await;

    // Case C: delete still fails and cap is too small (200 < 300 retained after evicting committed)
    // -> startup fails instead of admitting work, releasing the root lock cleanly.
    std::fs::write(&unindexed_path, vec![0x99; 300]).unwrap();
    let fs_fail_cap = FaultyCacheFs::new();
    fs_fail_cap.set_fail_remove_prefix(Some("orphan_renamed.bin"));
    let err = match CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope)
            .with_cap_bytes(200)
            .unwrap(),
        fs_fail_cap.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    {
        Ok(_) => panic!("startup must fail when retained bytes exceed cap"),
        Err(err) => err,
    };
    assert!(matches!(
        err,
        CacheError::StartupCapExceeded {
            charged: 300,
            cap: 200
        }
    ));
    assert!(
        !fs_fail_cap.is_lock_held(),
        "lock released on failed startup"
    );
}

#[tokio::test]
async fn failed_purge_tombstone_stays_hidden_across_restart() {
    let dir = TempCacheDir::new();
    let acct = AccountId::new("acct-purge-fail");
    let scope1 = SessionScope::account(acct.clone(), 1);
    let fs1 = FaultyCacheFs::new();

    let store1 = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope1.clone())
            .with_cap_bytes(1000)
            .unwrap(),
        fs1.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .unwrap();

    let key = thumb_key("secret-nsfw");
    store1
        .put(key.clone(), scope1.clone(), Some(150), vec![0xAB; 150])
        .await
        .unwrap();

    // Inject delete failure during logout purge.
    fs1.set_fail_remove_prefix(Some("/objects/"));
    store1
        .transition_scope(CacheScopeTransition {
            new_scope: SessionScope::guest(2),
            purge_account: Some(acct.clone()),
        })
        .await
        .unwrap();

    // Entry is immediately hidden/unreadable, while bytes remain charged as tombstone.
    assert_eq!(
        store1.get(key.clone(), scope1.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    let stats1 = store1.stats().await.unwrap();
    assert_eq!(stats1.entry_count, 0);
    assert_eq!(stats1.tombstone_bytes, 150);
    assert_eq!(stats1.pending_deletes, 1);
    store1.shutdown().await;

    // Restart with delete STILL failing and same account scope -> entry stays hidden and charged.
    let fs2 = FaultyCacheFs::new();
    fs2.set_fail_remove_prefix(Some("/objects/"));
    let restarted_scope = SessionScope::account(acct.clone(), 1);
    let store2 = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), restarted_scope.clone())
            .with_cap_bytes(1000)
            .unwrap(),
        fs2,
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .unwrap();

    assert_eq!(
        store2
            .get(key.clone(), restarted_scope.clone())
            .await
            .unwrap(),
        CacheLookup::Miss
    );
    let stats2 = store2.stats().await.unwrap();
    assert_eq!(stats2.entry_count, 0);
    assert_eq!(stats2.tombstone_bytes, 150);
    assert_eq!(stats2.pending_deletes, 1);
    store2.shutdown().await;

    // Restart with delete succeeding -> tombstone file is physically deleted and charge released.
    let store3 = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), restarted_scope.clone())
            .with_cap_bytes(1000)
            .unwrap(),
    )
    .await
    .unwrap();
    let stats3 = store3.stats().await.unwrap();
    assert_eq!(stats3.tombstone_bytes, 0);
    assert_eq!(stats3.pending_deletes, 0);
    assert_eq!(
        store3.get(key, restarted_scope).await.unwrap(),
        CacheLookup::Miss
    );
    store3.shutdown().await;
}

#[tokio::test]
async fn same_generation_different_account_cannot_publish() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let acct_a = AccountId::new("acct-alpha");
    let acct_b = AccountId::new("acct-beta");
    let scope_a = SessionScope::account(acct_a.clone(), 5);
    let scope_b = SessionScope::account(acct_b.clone(), 6);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope_a.clone())
            .with_cap_bytes(5000)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .unwrap();

    let (tx, rx) = mpsc::channel(2);
    let s1 = store.clone();
    let sc_a = scope_a.clone();
    let key = thumb_key("shared-id");
    let k1 = key.clone();
    let put_handle = tokio::spawn(async move { s1.put(k1, sc_a, Some(64), rx).await });
    fs.wait_writers_opened(1).await;

    // Reject a different account at the same generation, without cancelling A.
    assert_eq!(
        store
            .transition_scope(CacheScopeTransition {
                new_scope: SessionScope::account(acct_b, 5),
                purge_account: Some(acct_a.clone()),
            })
            .await,
        Err(CacheError::ScopeMismatch)
    );
    let stats = store.stats().await.unwrap();
    assert_eq!(stats.current_scope, scope_a);
    assert_eq!(stats.reserved_bytes, 64);

    // A valid transition supersedes the old writer.
    store
        .transition_scope(CacheScopeTransition {
            new_scope: scope_b.clone(),
            purge_account: Some(acct_a),
        })
        .await
        .unwrap();

    // Even if stream immediately sends EOF bytes, writer must not publish into acct_a or acct_b.
    let _ = tx.send(Ok(vec![0x12; 64])).await;
    drop(tx);
    let err = put_handle.await.unwrap().unwrap_err();
    assert_eq!(err, CacheError::ScopeMismatch);

    assert_eq!(
        store.get(key.clone(), scope_a.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    assert_eq!(
        store.get(key.clone(), scope_b.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    assert_eq!(
        store
            .put(key, scope_a, Some(32), vec![0x34; 32])
            .await
            .unwrap_err(),
        CacheError::ScopeMismatch
    );
    assert_eq!(store.stats().await.unwrap().entry_count, 0);
    store.shutdown().await;
}

#[tokio::test]
async fn scope_mismatch_misses_reads_and_rejects_stale_writes() {
    let dir = TempCacheDir::new();
    let acct_a = AccountId::new("acct-a");
    let scope_a1 = SessionScope::account(acct_a.clone(), 1);
    let scope_a2 = SessionScope::account(acct_a, 2);
    let guest1 = SessionScope::guest(1);

    let store = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), scope_a1.clone())
            .with_cap_bytes(5000)
            .unwrap(),
    )
    .await
    .unwrap();

    let key = thumb_key("wp-99");
    store
        .put(key.clone(), scope_a1.clone(), Some(16), vec![1; 16])
        .await
        .unwrap();

    // Wrong generation and guest scope miss on read and reject writes.
    assert_eq!(
        store.get(key.clone(), scope_a2.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    assert_eq!(
        store.get(key.clone(), guest1.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    assert_eq!(
        store
            .put(key.clone(), scope_a2, Some(16), vec![2; 16])
            .await
            .unwrap_err(),
        CacheError::ScopeMismatch
    );
    assert_eq!(
        store
            .put(key, guest1, Some(16), vec![3; 16])
            .await
            .unwrap_err(),
        CacheError::ScopeMismatch
    );
    store.shutdown().await;
}

#[tokio::test]
async fn logout_while_staging_and_while_handle_held_open_keeps_actor_responsive() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let acct = AccountId::new("acct-logout-staging");
    let scope1 = SessionScope::account(acct.clone(), 1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope1.clone())
            .with_cap_bytes(5000)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .unwrap();

    // Seed one published entry in acct before arming the close gate for the held writer.
    store
        .put(
            thumb_key("published"),
            scope1.clone(),
            Some(40),
            vec![0xAA; 40],
        )
        .await
        .unwrap();

    let gate = fs.arm_close_gate();
    let (_tx_held, rx_held) = mpsc::channel::<Result<Vec<u8>, CacheSourceError>>(2);
    let s1 = store.clone();
    let sc1 = scope1.clone();
    let held_put = tokio::spawn(async move {
        s1.put(thumb_key("held-staging"), sc1, Some(200), rx_held)
            .await
    });
    fs.wait_writers_opened(2).await;

    let (_tx_fast, rx_fast) = mpsc::channel::<Result<Vec<u8>, CacheSourceError>>(2);
    let s2 = store.clone();
    let sc2 = scope1.clone();
    let fast_put = tokio::spawn(async move {
        s2.put(thumb_key("fast-staging"), sc2, Some(100), rx_fast)
            .await
    });
    fs.wait_writers_opened(3).await;

    // Logout: published entry deleted immediately, fast staging cancelled & deleted,
    // held writer keeps file open until gate releases while actor keeps serving guest writes!
    let mut removed_rx = fs.subscribe_removed();
    let scope_guest = SessionScope::guest(2);
    bounded(
        "logout transition_scope",
        store.transition_scope(CacheScopeTransition {
            new_scope: scope_guest.clone(),
            purge_account: Some(acct),
        }),
    )
    .await
    .unwrap();

    // Both cancelled writers enter close(); held writer blocks inside close() while fast exits.
    fs.wait_close_entered(3).await;
    bounded(
        "wait fast+published removed",
        removed_rx.wait_for(|&c| c >= 2),
    )
    .await
    .unwrap();
    assert_eq!(
        bounded("wait fast_put", fast_put)
            .await
            .unwrap()
            .unwrap_err(),
        CacheError::ScopeMismatch
    );
    assert_eq!(fs.open_handles(), 1);
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        1
    );

    let mid_stats = bounded("mid stats", store.stats()).await.unwrap();
    assert_eq!(mid_stats.published_bytes, 0);
    assert_eq!(
        mid_stats.reserved_bytes, 200,
        "held writer stays charged until Finished"
    );

    // Actor remains responsive while held writer is still open.
    bounded(
        "guest put while held",
        store.put(
            thumb_key("guest-while-held"),
            scope_guest.clone(),
            Some(50),
            vec![0x55; 50],
        ),
    )
    .await
    .unwrap();
    assert!(matches!(
        bounded(
            "guest get while held",
            store.get(thumb_key("guest-while-held"), scope_guest),
        )
        .await
        .unwrap(),
        CacheLookup::Hit { .. }
    ));

    // Release held handle -> staging deleted and charge released.
    gate.release();
    fs.wait_open_handles(0).await;
    bounded(
        "wait held staging removed",
        removed_rx.wait_for(|&c| c >= 3),
    )
    .await
    .unwrap();
    assert_eq!(
        bounded("wait held_put", held_put)
            .await
            .unwrap()
            .unwrap_err(),
        CacheError::ScopeMismatch
    );

    let end_stats = store.stats().await.unwrap();
    assert_eq!(end_stats.reserved_bytes, 0);
    assert_eq!(end_stats.live_reservations, 0);
    assert_eq!(end_stats.published_bytes, 50);
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
    store.shutdown().await;
}

#[tokio::test]
async fn duplicate_completion_and_retired_id_after_failed_delete_ignored() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);

    let store = CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(5000)
            .unwrap(),
        fs.clone(),
        Arc::new(SqliteCacheIndex::new()),
        Arc::new(FakeClock::default()),
    )
    .await
    .unwrap();

    // 1. Normal completion (reservation_id = 1) followed by duplicate Finished injection.
    let key1 = thumb_key("dup");
    store
        .put(key1.clone(), scope.clone(), Some(32), vec![1; 32])
        .await
        .unwrap();
    store
        .inject_writer_finished_for_test(1, scope.clone(), key1.clone(), 32, 0)
        .await
        .unwrap();
    assert_eq!(store.stats().await.unwrap().entry_count, 1);

    assert_eq!(fs.renamed_targets().len(), 1);
    assert_eq!(
        store.get(key1, scope.clone()).await.unwrap(),
        CacheLookup::Hit {
            scope: scope.clone(),
            bytes: vec![1; 32],
            stale: false,
        }
    );

    // 2. Cancelled reservation whose staging cleanup fails -> retired as tombstone (reservation_id = 2).
    // Injecting a late Finished for that retired id must be completely ignored and publish nothing.
    fs.set_fail_remove_prefix(Some("/staging/"));
    let gate = fs.arm_close_gate();
    let s1 = store.clone();
    let sc1 = scope.clone();
    let key2 = thumb_key("retired-after-failed-delete");
    let k2 = key2.clone();
    let put2 = tokio::spawn(async move { s1.put(k2, sc1, Some(80), vec![2; 32]).await });
    fs.wait_close_entered(2).await;

    store
        .transition_scope(CacheScopeTransition {
            new_scope: SessionScope::guest(2),
            purge_account: None,
        })
        .await
        .unwrap();
    gate.release();
    assert_eq!(put2.await.unwrap().unwrap_err(), CacheError::ScopeMismatch);

    let stats_tomb = store.stats().await.unwrap();
    assert_eq!(stats_tomb.tombstone_bytes, 32);
    assert_eq!(stats_tomb.pending_deletes, 1);

    // Inject Finished for retired reservation_id = 2 -> ignored, nothing published.
    store
        .inject_writer_finished_for_test(2, SessionScope::guest(2), key2.clone(), 80, 1234)
        .await
        .unwrap();
    assert_eq!(
        store.get(key2, SessionScope::guest(2)).await.unwrap(),
        CacheLookup::Miss
    );
    assert_eq!(store.stats().await.unwrap().entry_count, 1);
    assert_eq!(fs.renamed_targets().len(), 1);
    assert_eq!(store.stats().await.unwrap().tombstone_bytes, 32);
    store.shutdown().await;
}

#[tokio::test]
async fn startup_reconciles_staging_missing_corrupt_and_pending_deletes() {
    let dir = TempCacheDir::new();
    let scope = SessionScope::guest(1);

    let store = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(10_000)
            .unwrap(),
    )
    .await
    .unwrap();

    store
        .put(thumb_key("keep"), scope.clone(), Some(40), vec![0x11; 40])
        .await
        .unwrap();
    store
        .put(
            thumb_key("missing"),
            scope.clone(),
            Some(40),
            vec![0x22; 40],
        )
        .await
        .unwrap();
    store
        .put(
            thumb_key("corrupt"),
            scope.clone(),
            Some(40),
            vec![0x33; 40],
        )
        .await
        .unwrap();
    store.shutdown().await;

    // Inspect SQLite index to find paths for missing and corrupt entries, plus plant a staging file.
    let conn = rusqlite::Connection::open(dir.path().join("cache_index.db")).unwrap();
    let missing_rel: String = conn
        .query_row(
            "SELECT rel_path FROM cache_entries WHERE cache_key = 'missing'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let corrupt_rel: String = conn
        .query_row(
            "SELECT rel_path FROM cache_entries WHERE cache_key = 'corrupt'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    drop(conn);

    std::fs::remove_file(dir.path().join(&missing_rel)).unwrap();
    std::fs::write(dir.path().join(&corrupt_rel), vec![0xFF; 41]).unwrap();
    let stale_staging = dir.path().join("staging").join("crashed.part");
    std::fs::write(&stale_staging, vec![0xAA; 128]).unwrap();

    let reopened = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), scope.clone())
            .with_cap_bytes(10_000)
            .unwrap(),
    )
    .await
    .unwrap();

    assert!(!stale_staging.exists());
    assert!(!dir.path().join(&corrupt_rel).exists());
    let stats = reopened.stats().await.unwrap();
    assert_eq!(stats.entry_count, 1);
    assert_eq!(stats.published_bytes, 40);
    assert_eq!(stats.tombstone_bytes, 0);
    assert!(matches!(
        reopened
            .get(thumb_key("keep"), scope.clone())
            .await
            .unwrap(),
        CacheLookup::Hit { .. }
    ));
    assert_eq!(
        reopened
            .get(thumb_key("missing"), scope.clone())
            .await
            .unwrap(),
        CacheLookup::Miss
    );
    assert_eq!(
        reopened.get(thumb_key("corrupt"), scope).await.unwrap(),
        CacheLookup::Miss
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn offline_scope_visibility_and_reconnect_purge() {
    let dir = TempCacheDir::new();
    let acct_a = AccountId::new("acct-offline-a");
    let acct_b = AccountId::new("acct-offline-b");
    let guest = SessionScope::guest(1);
    let scope_a = SessionScope::account(acct_a.clone(), 2);
    let scope_b = SessionScope::account(acct_b.clone(), 3);

    let store = CacheStore::open(
        CacheConfig::new(dir.path().to_path_buf(), guest.clone())
            .with_cap_bytes(10_000)
            .unwrap(),
    )
    .await
    .unwrap();

    let k_guest = thumb_key("guest-item");
    let k_acct_a = thumb_key("acct-a-item");
    store
        .put(k_guest.clone(), guest.clone(), Some(24), vec![0x01; 24])
        .await
        .unwrap();

    // Switch to Account A (without purging Guest) and cache an item for Account A.
    store
        .transition_scope(CacheScopeTransition {
            new_scope: scope_a.clone(),
            purge_account: None,
        })
        .await
        .unwrap();
    store
        .put(k_acct_a.clone(), scope_a.clone(), Some(32), vec![0x02; 32])
        .await
        .unwrap();

    // Offline while signed in as Account A: Account A entry visible & labelled stale; Guest/Account B miss.
    store
        .set_connectivity(CacheConnectivity::Offline)
        .await
        .unwrap();
    assert_eq!(
        store.get(k_acct_a.clone(), scope_a.clone()).await.unwrap(),
        CacheLookup::Hit {
            scope: scope_a.clone(),
            bytes: vec![0x02; 32],
            stale: true,
        }
    );
    assert_eq!(
        store.get(k_guest.clone(), guest.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    assert_eq!(
        store.get(k_acct_a.clone(), scope_b.clone()).await.unwrap(),
        CacheLookup::Miss
    );

    // Offline in Guest mode: Guest offline content shown labelled stale, Account A misses.
    store
        .transition_scope(CacheScopeTransition {
            new_scope: SessionScope::guest(4),
            purge_account: None,
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .get(k_guest.clone(), SessionScope::guest(4))
            .await
            .unwrap(),
        CacheLookup::Hit {
            scope: SessionScope::guest(4),
            bytes: vec![0x01; 24],
            stale: true,
        }
    );
    assert_eq!(
        store.get(k_acct_a.clone(), scope_a.clone()).await.unwrap(),
        CacheLookup::Miss
    );

    // Return to Account A (persisted on disk) and reconnect -> 403/401 purges Account A cache.
    store
        .transition_scope(CacheScopeTransition {
            new_scope: SessionScope::account(acct_a.clone(), 5),
            purge_account: None,
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .get(k_acct_a.clone(), SessionScope::account(acct_a.clone(), 5))
            .await
            .unwrap(),
        CacheLookup::Hit {
            scope: SessionScope::account(acct_a.clone(), 5),
            bytes: vec![0x02; 32],
            stale: true,
        }
    );

    store
        .set_connectivity(CacheConnectivity::Online)
        .await
        .unwrap();
    store.purge_account(acct_a.clone()).await.unwrap();
    assert_eq!(
        store
            .get(k_acct_a, SessionScope::account(acct_a, 5))
            .await
            .unwrap(),
        CacheLookup::Miss
    );
    store.shutdown().await;
}

fn test_probe(start_scope: SessionScope, username: &str, key: &str) -> SignInProbe {
    SignInProbe {
        start_scope,
        username: username.to_string(),
        api_key: SecretString::new(key),
        user_settings: None,
        own_collections: Some(Vec::<CollectionSummary>::new()),
        verification: UsernameVerification::UnverifiableEmpty,
    }
}

#[tokio::test]
async fn session_manager_wires_logout_switch_and_401_to_fake_and_real_cache() {
    let dir = TempCacheDir::new();
    let db_path = dir.path().join("wallora.db");
    let storage: Arc<dyn SessionStorage> =
        Arc::new(SqliteStore::open(&db_path).expect("open sqlite"));
    let creds: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());

    // Part 1: Fake sink verifies every transition emitted synchronously without blocking.
    let fake_sink = Arc::new(FakeSessionCacheSink::default());
    let mut session = SessionManager::restore_with_cache(
        Arc::clone(&storage),
        Arc::clone(&creds),
        fake_sink.clone(),
    );

    let probe_a = test_probe(session.scope(), "alice", "wh_fake_alpha_111");
    let acct_a = match session.commit_sign_in(probe_a).unwrap() {
        wallora_core::services::SignInCommitOutcome::Applied { scope, .. } => {
            scope.account_id().unwrap().clone()
        }
        other => panic!("unexpected sign in outcome: {other:?}"),
    };

    let probe_b = test_probe(session.scope(), "bob", "wh_fake_beta_222");
    let acct_b = match session.commit_sign_in(probe_b).unwrap() {
        wallora_core::services::SignInCommitOutcome::Applied { scope, .. } => {
            scope.account_id().unwrap().clone()
        }
        other => panic!("unexpected sign in outcome: {other:?}"),
    };

    // Switch back to A -> purges B.
    session.switch_account(&acct_a).unwrap();
    // Refresh 403 -> purges A via on_purge_account.
    session.apply_refresh(AccountRefreshPayload {
        request_scope: session.scope(),
        user_settings: Err(ProviderError::Forbidden),
        own_collections: Err(ProviderError::Forbidden),
        identity_outcome: None,
    });
    // 401 revocation -> bumps scope to guest and purges A.
    let _ =
        session.handle_provider_result::<()>(&session.scope(), Err(ProviderError::Unauthorized));
    // Switch to B and logout -> bumps scope to guest and purges B.
    session.switch_account(&acct_b).unwrap();
    let logout_out = session.logout();
    assert_eq!(logout_out.logged_out_account, Some(acct_b.clone()));

    let transitions = fake_sink.transitions();
    assert!(
        transitions
            .iter()
            .any(|t| t.purge_account.as_ref() == Some(&acct_a))
    );
    assert!(
        transitions
            .iter()
            .any(|t| t.purge_account.as_ref() == Some(&acct_b))
    );
    assert_eq!(fake_sink.purges(), vec![acct_a.clone()]);

    // Part 2: End-to-end SessionManager + real CacheStore actor.
    let cache_dir = dir.path().join("real-cache");
    let cache_store = CacheStore::open(
        CacheConfig::new(cache_dir, SessionScope::guest(1))
            .with_cap_bytes(50_000)
            .unwrap(),
    )
    .await
    .unwrap();
    let cache_sink: Arc<dyn SessionCacheSink> = Arc::new(cache_store.clone());

    let db_path2 = dir.path().join("wallora2.db");
    let storage2: Arc<dyn SessionStorage> =
        Arc::new(SqliteStore::open(&db_path2).expect("open sqlite 2"));
    let creds2: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());
    let mut live_session = SessionManager::restore_with_cache(storage2, creds2, cache_sink);

    let acct1 = match live_session
        .commit_sign_in(test_probe(live_session.scope(), "carol", "wh_live_carol_1"))
        .unwrap()
    {
        wallora_core::services::SignInCommitOutcome::Applied { scope, .. } => {
            scope.account_id().unwrap().clone()
        }
        other => panic!("unexpected outcome: {other:?}"),
    };
    let s_carol = live_session.scope();
    let key = thumb_key("carol-thumb");
    cache_store
        .put(key.clone(), s_carol.clone(), Some(48), vec![0xCC; 48])
        .await
        .unwrap();

    // Sync 401 revocation from SessionManager purges Carol's cached entries non-blockingly.
    let _ = live_session.handle_provider_result::<()>(&s_carol, Err(ProviderError::Unauthorized));
    assert_eq!(
        cache_store
            .get(key, SessionScope::account(acct1, s_carol.generation))
            .await
            .unwrap(),
        CacheLookup::Miss
    );
    let stats = cache_store.stats().await.unwrap();
    assert_eq!(stats.entry_count, 0);
    assert_eq!(stats.published_bytes, 0);
    assert_eq!(stats.current_scope, live_session.scope());
    cache_store.shutdown().await;
}

async fn open_test_cache(
    dir: &TempCacheDir,
    scope: SessionScope,
    fs: Arc<FaultyCacheFs>,
    index: Arc<FaultyCacheIndex>,
    cap: u64,
) -> Result<CacheStore, CacheError> {
    CacheStore::open_with_seams(
        CacheConfig::new(dir.path().to_path_buf(), scope)
            .with_cap_bytes(cap)
            .unwrap(),
        fs,
        index,
        Arc::new(FakeClock::default()),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_last_handle_drop_releases_root_lock() {
    let dir = TempCacheDir::new();
    let scope = SessionScope::guest(1);
    let store = CacheStore::open(CacheConfig::new(dir.path().into(), scope.clone()))
        .await
        .unwrap();
    drop(store);
    let reopened = bounded("root lock released", async {
        loop {
            match open_test_cache(
                &dir,
                scope.clone(),
                FaultyCacheFs::new(),
                FaultyCacheIndex::new(),
                100,
            )
            .await
            {
                Err(CacheError::Locked) => tokio::task::yield_now().await,
                result => break result.unwrap(),
            }
        }
    })
    .await;
    reopened.shutdown().await;
}

#[tokio::test]
async fn review_failed_sink_purge_hides_keeps_charge_and_retries() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let index = FaultyCacheIndex::new();
    let id = AccountId::new("purge-retry");
    let scope = SessionScope::account(id.clone(), 1);
    let store = open_test_cache(&dir, scope.clone(), fs, index.clone(), 100)
        .await
        .unwrap();
    let key = thumb_key("private");
    store
        .put(key.clone(), scope, Some(10), vec![1; 10])
        .await
        .unwrap();
    index.fail_hide.store(true, Ordering::SeqCst);
    store.on_scope_transition(CacheScopeTransition {
        new_scope: SessionScope::guest(2),
        purge_account: Some(id.clone()),
    });
    let next = SessionScope::account(id, 3);
    store
        .transition_scope(CacheScopeTransition {
            new_scope: next.clone(),
            purge_account: None,
        })
        .await
        .unwrap();
    assert_eq!(
        store.get(key, next.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    let stats = store.stats().await.unwrap();
    assert_eq!(
        (
            stats.published_bytes,
            stats.tombstone_bytes,
            stats.total_charged_bytes
        ),
        (0, 10, 10)
    );
    index.fail_hide.store(false, Ordering::SeqCst);
    store
        .put(thumb_key("new"), next, Some(5), vec![2; 5])
        .await
        .unwrap();
    assert_eq!(store.stats().await.unwrap().total_charged_bytes, 5);
    assert_eq!(
        std::fs::read_dir(dir.path().join("objects"))
            .unwrap()
            .count(),
        1
    );
    store.shutdown().await;
}

#[tokio::test]
async fn review_failed_purge_stays_hidden_on_reopen() {
    let dir = TempCacheDir::new();
    let index = FaultyCacheIndex::new();
    let id = AccountId::new("purge-restart");
    let scope = SessionScope::account(id.clone(), 1);
    let store = open_test_cache(
        &dir,
        scope.clone(),
        FaultyCacheFs::new(),
        index.clone(),
        100,
    )
    .await
    .unwrap();
    let key = thumb_key("private");
    store
        .put(key.clone(), scope.clone(), Some(10), vec![1; 10])
        .await
        .unwrap();
    index.fail_hide.store(true, Ordering::SeqCst);
    assert!(matches!(
        store.purge_account(id).await,
        Err(CacheError::Index(_))
    ));
    store.shutdown().await;
    drop(index);
    let reopened = CacheStore::open(CacheConfig::new(dir.path().into(), scope.clone()))
        .await
        .unwrap();
    assert_eq!(reopened.get(key, scope).await.unwrap(), CacheLookup::Miss);
    assert_eq!(reopened.stats().await.unwrap().total_charged_bytes, 0);
    reopened.shutdown().await;
}

#[tokio::test]
async fn review_purge_cancels_held_writer_until_finished() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let id = AccountId::new("purge-held");
    let scope = SessionScope::account(id.clone(), 1);
    let store = open_test_cache(
        &dir,
        scope.clone(),
        fs.clone(),
        FaultyCacheIndex::new(),
        100,
    )
    .await
    .unwrap();
    let gate = fs.arm_close_gate();
    let key = thumb_key("private");
    let put = tokio::spawn({
        let (store, scope, key) = (store.clone(), scope.clone(), key.clone());
        async move { store.put(key, scope, Some(10), vec![1; 10]).await }
    });
    fs.wait_close_entered(1).await;
    store.on_purge_account(&id);
    let stats = store.stats().await.unwrap();
    gate.release();
    assert_eq!(stats.reserved_bytes, 10);
    assert_eq!(
        bounded("writer finishes", put).await.unwrap(),
        Err(CacheError::Cancelled)
    );
    assert_eq!(store.get(key, scope).await.unwrap(), CacheLookup::Miss);
    assert!(fs.renamed_targets().is_empty());
    assert_eq!(store.stats().await.unwrap().total_charged_bytes, 0);
    store.shutdown().await;
}

#[tokio::test]
async fn review_scope_transition_requires_new_generation_unless_identical() {
    let dir = TempCacheDir::new();
    let scope = SessionScope::guest(5);
    let store = CacheStore::open(CacheConfig::new(dir.path().into(), scope.clone()))
        .await
        .unwrap();
    for new_scope in [
        SessionScope::guest(4),
        SessionScope::account(AccountId::new("same-gen"), 5),
    ] {
        assert_eq!(
            store
                .transition_scope(CacheScopeTransition {
                    new_scope,
                    purge_account: None
                })
                .await,
            Err(CacheError::ScopeMismatch)
        );
        assert_eq!(store.stats().await.unwrap().current_scope, scope);
    }
    store
        .transition_scope(CacheScopeTransition {
            new_scope: scope.clone(),
            purge_account: None,
        })
        .await
        .unwrap();
    assert_eq!(store.stats().await.unwrap().current_scope, scope);
    store.shutdown().await;
}

#[tokio::test]
async fn review_publish_panic_keeps_charge_until_cleanup() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);
    let store = open_test_cache(&dir, scope.clone(), fs.clone(), FaultyCacheIndex::new(), 20)
        .await
        .unwrap();
    fs.panic_after_rename.store(true, Ordering::SeqCst);
    fs.set_fail_remove_prefix(Some("/objects/"));
    let key = thumb_key("panic");
    assert!(matches!(
        store
            .put(key.clone(), scope.clone(), Some(20), vec![3; 20])
            .await,
        Err(CacheError::Io(_))
    ));
    assert_eq!(
        store.get(key.clone(), scope.clone()).await.unwrap(),
        CacheLookup::Miss
    );
    assert_eq!(store.stats().await.unwrap().tombstone_bytes, 20);
    assert!(fs.renamed_targets()[0].exists());
    fs.set_fail_remove_prefix(None);
    fs.panic_after_rename.store(false, Ordering::SeqCst);
    store.put(key, scope, Some(20), vec![4; 20]).await.unwrap();
    assert!(!fs.renamed_targets()[0].exists());
    assert_eq!(store.stats().await.unwrap().total_charged_bytes, 20);
    store.shutdown().await;
}

#[tokio::test]
async fn review_truncated_and_empty_payloads_do_not_publish() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);
    let store = open_test_cache(
        &dir,
        scope.clone(),
        fs.clone(),
        FaultyCacheIndex::new(),
        2_000_000,
    )
    .await
    .unwrap();
    for (expected, payload) in [(Some(10), vec![1; 9]), (Some(0), vec![]), (None, vec![])] {
        let key = thumb_key("bad-size");
        assert!(matches!(
            store
                .put(key.clone(), scope.clone(), expected, payload)
                .await,
            Err(CacheError::WriterFailed(_))
        ));
        assert_eq!(
            store.get(key, scope.clone()).await.unwrap(),
            CacheLookup::Miss
        );
        assert_eq!(store.stats().await.unwrap().total_charged_bytes, 0);
    }
    assert!(fs.renamed_targets().is_empty());
    store.shutdown().await;
}

#[tokio::test]
async fn review_rename_failure_preserves_previous_entry() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);
    let store = open_test_cache(
        &dir,
        scope.clone(),
        fs.clone(),
        FaultyCacheIndex::new(),
        100,
    )
    .await
    .unwrap();
    let key = thumb_key("replacement");
    store
        .put(key.clone(), scope.clone(), Some(10), vec![1; 10])
        .await
        .unwrap();
    fs.fail_rename.store(true, Ordering::SeqCst);
    assert!(matches!(
        store
            .put(key.clone(), scope.clone(), Some(20), vec![2; 20])
            .await,
        Err(CacheError::Io(_))
    ));
    assert_eq!(
        store.get(key, scope.clone()).await.unwrap(),
        CacheLookup::Hit {
            scope,
            bytes: vec![1; 10],
            stale: false
        }
    );
    assert_eq!(store.stats().await.unwrap().total_charged_bytes, 10);
    assert_eq!(fs.renamed_targets().len(), 1);
    assert_eq!(
        std::fs::read_dir(dir.path().join("staging"))
            .unwrap()
            .count(),
        0
    );
    store.shutdown().await;
}

#[tokio::test]
async fn review_startup_salvages_valid_rows_and_never_follows_invalid_paths() {
    let dir = TempCacheDir::new();
    let scope = SessionScope::guest(1);
    let config = CacheConfig::new(dir.path().into(), scope.clone());
    let store = CacheStore::open(config.clone()).await.unwrap();
    let good = thumb_key("good");
    store
        .put(good.clone(), scope.clone(), Some(3), vec![1; 3])
        .await
        .unwrap();
    store.shutdown().await;
    std::fs::write(dir.path().join("outside"), b"keep").unwrap();
    std::fs::write(dir.path().join("objects/bad.bin"), b"discard").unwrap();
    let db = rusqlite::Connection::open(dir.path().join("cache_index.db")).unwrap();
    db.execute_batch(
        "INSERT INTO cache_entries VALUES
        ('bad namespace','thumb','bad','objects/bad.bin',7,0,1,0),
        ('guest','thumb','bad-path','objects/../outside',4,0,1,0);
        INSERT INTO pending_deletes VALUES ('../outside',4), ('objects/../outside',4);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO pending_deletes VALUES (?1,4)",
        [dir.path().join("outside").to_str().unwrap()],
    )
    .unwrap();
    drop(db);
    let store = CacheStore::open(config).await.unwrap();
    assert_eq!(std::fs::read(dir.path().join("outside")).unwrap(), b"keep");
    assert!(!dir.path().join("objects/bad.bin").exists());
    assert_eq!(
        store.get(good, scope.clone()).await.unwrap(),
        CacheLookup::Hit {
            scope,
            bytes: vec![1; 3],
            stale: false
        }
    );
    let db = rusqlite::Connection::open(dir.path().join("cache_index.db")).unwrap();
    let count = |table: &str| {
        db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .unwrap()
    };
    assert_eq!(count("cache_entries"), 1);
    assert_eq!(count("pending_deletes"), 0);
    store.shutdown().await;
}

#[tokio::test]
async fn review_startup_rebuilds_corrupt_index_and_reconciles_payloads() {
    let dir = TempCacheDir::new();
    std::fs::create_dir_all(dir.path().join("objects")).unwrap();
    std::fs::create_dir_all(dir.path().join("staging")).unwrap();
    std::fs::write(dir.path().join("cache_index.db"), vec![0xBB; 4096]).unwrap();
    for path in ["objects/lost.bin", "staging/lost.part"] {
        std::fs::write(dir.path().join(path), b"unindexed").unwrap();
    }
    let store = CacheStore::open(CacheConfig::new(dir.path().into(), SessionScope::guest(1)))
        .await
        .unwrap();
    assert!(std::fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("cache_index.db.corrupt-")
    }));
    assert!(!dir.path().join("objects/lost.bin").exists());
    assert!(!dir.path().join("staging/lost.part").exists());
    assert_eq!(store.stats().await.unwrap().total_charged_bytes, 0);
    store.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn review_startup_removes_non_utf8_files() {
    use std::os::unix::ffi::OsStrExt;
    let dir = TempCacheDir::new();
    std::fs::create_dir_all(dir.path().join("objects")).unwrap();
    let path = dir
        .path()
        .join("objects")
        .join(std::ffi::OsStr::from_bytes(b"bad-\xff"));
    std::fs::write(&path, b"unindexed").unwrap();
    let store = CacheStore::open(CacheConfig::new(dir.path().into(), SessionScope::guest(1)))
        .await
        .unwrap();
    assert!(!path.exists());
    store.shutdown().await;
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(12))]
    #[test]
    fn review_random_puts_and_evictions_preserve_ledger(
        cap in 8u64..64,
        puts in proptest::collection::vec((0u8..5, 1u64..72), 1..16)
    ) {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let dir = TempCacheDir::new();
            let scope = SessionScope::guest(1);
            let store = CacheStore::open(CacheConfig::new(dir.path().into(), scope.clone()).with_cap_bytes(cap).unwrap()).await.unwrap();
            for (key, size) in puts {
                let result = store.put(thumb_key(&key.to_string()), scope.clone(), Some(size), vec![key; size as usize]).await;
                if size > cap {
                    assert_eq!(result, Err(CacheError::CapacityExceeded));
                } else {
                    assert_eq!(result.unwrap().byte_len, size);
                }
                let stats = store.stats().await.unwrap();
                let disk_bytes: u64 = std::fs::read_dir(dir.path().join("objects")).unwrap()
                    .map(|entry| entry.unwrap().metadata().unwrap().len()).sum();
                assert!(stats.total_charged_bytes <= cap);
                assert_eq!(stats.total_charged_bytes, stats.published_bytes + stats.reserved_bytes + stats.tombstone_bytes);
                assert_eq!(stats.published_bytes, disk_bytes);
            }
            store.shutdown().await;
        });
    }
}

#[tokio::test]
async fn startup_defers_same_length_corruption_to_first_get() {
    let dir = TempCacheDir::new();
    let fs = FaultyCacheFs::new();
    let scope = SessionScope::guest(1);
    let key = thumb_key("lazy-crc");
    let store = open_test_cache(
        &dir,
        scope.clone(),
        fs.clone(),
        FaultyCacheIndex::new(),
        100,
    )
    .await
    .unwrap();
    store
        .put(key.clone(), scope.clone(), Some(16), vec![1; 16])
        .await
        .unwrap();
    let path = fs.renamed_targets()[0].clone();
    store.shutdown().await;
    std::fs::write(&path, [2; 16]).unwrap();

    let reopened = open_test_cache(
        &dir,
        scope.clone(),
        fs.clone(),
        FaultyCacheIndex::new(),
        100,
    )
    .await
    .unwrap();
    let stats = reopened.stats().await.unwrap();
    assert_eq!(stats.entry_count, 1);
    assert_eq!(stats.published_bytes, 16);
    assert!(path.exists());
    assert_eq!(fs.read_bounded_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        reopened.get(key.clone(), scope.clone()).await.unwrap(),
        CacheLookup::CorruptPurged
    );
    assert_eq!(reopened.get(key, scope).await.unwrap(), CacheLookup::Miss);
    assert!(!path.exists());
    assert_eq!(fs.read_bounded_calls.load(Ordering::SeqCst), 1);
    assert_eq!(reopened.stats().await.unwrap().total_charged_bytes, 0);
    reopened.shutdown().await;
}
