use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, mpsc, oneshot, watch};
use wallora_core::domain::SessionScope;
use wallora_core::provider::ImageClass;
use wallora_core::services::cache::{
    CacheByteStream, CacheChunkFuture, CacheClock, CacheConfig, CacheDirEntry, CacheEntryKey,
    CacheError, CacheFs, CacheIndex, CacheIndexSnapshot, CacheIoReadError, CacheKey,
    CacheLockError, CacheLockGuard, CacheLookup, CacheSourceError, CacheStore,
    DEFAULT_CACHE_CAP_BYTES, HARD_MAX_CACHE_CAP_BYTES, PendingDeleteRecord, PublishedEntryRecord,
    RealCacheFs, SqliteCacheIndex, StagingWriter,
};

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

struct ObservableWriter {
    inner: Box<dyn StagingWriter>,
    open_handles: Arc<AtomicUsize>,
    closed_notify: Arc<Notify>,
    hold_close_gate: Option<Arc<Notify>>,
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
        let res = {
            let dummy = Box::new(NoopWriter);
            let inner = std::mem::replace(&mut self.inner, dummy);
            inner.close()
        };
        if !self.drop_closed {
            self.drop_closed = true;
            self.open_handles.fetch_sub(1, Ordering::SeqCst);
            self.closed_notify.notify_waiters();
        }
        res
    }
}

impl Drop for ObservableWriter {
    fn drop(&mut self) {
        if !self.drop_closed {
            self.drop_closed = true;
            if let Some(gate) = &self.hold_close_gate {
                // Allow tests to observe synchronous handle closure only when released.
                let _ = gate;
            }
            self.open_handles.fetch_sub(1, Ordering::SeqCst);
            self.closed_notify.notify_waiters();
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
    open_writer_handles: Arc<AtomicUsize>,
    writer_closed_notify: Arc<Notify>,
    writer_opened_notify: Notify,
    file_removed_tx: watch::Sender<usize>,
    fail_remove_prefix: Mutex<Option<String>>,
    fail_rename: AtomicBool,
    removed_paths: Mutex<Vec<PathBuf>>,
    renamed_targets: Mutex<Vec<PathBuf>>,
}

impl Default for FaultyCacheFs {
    fn default() -> Self {
        let (file_removed_tx, _rx) = watch::channel(0usize);
        Self {
            real: RealCacheFs,
            writers_opened: AtomicUsize::new(0),
            open_writer_handles: Arc::new(AtomicUsize::new(0)),
            writer_closed_notify: Arc::new(Notify::new()),
            writer_opened_notify: Notify::new(),
            file_removed_tx,
            fail_remove_prefix: Mutex::new(None),
            fail_rename: AtomicBool::new(false),
            removed_paths: Mutex::new(Vec::new()),
            renamed_targets: Mutex::new(Vec::new()),
        }
    }
}

impl FaultyCacheFs {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
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

    fn try_lock_exclusive(
        &self,
        lock_path: &Path,
    ) -> Result<Box<dyn CacheLockGuard>, CacheLockError> {
        self.real.try_lock_exclusive(lock_path)
    }

    fn create_new_writer(&self, path: &Path) -> io::Result<Box<dyn StagingWriter>> {
        let inner = self.real.create_new_writer(path)?;
        self.writers_opened.fetch_add(1, Ordering::SeqCst);
        self.open_writer_handles.fetch_add(1, Ordering::SeqCst);
        self.writer_opened_notify.notify_waiters();
        Ok(Box::new(ObservableWriter {
            inner,
            open_handles: Arc::clone(&self.open_writer_handles),
            closed_notify: Arc::clone(&self.writer_closed_notify),
            hold_close_gate: None,
            drop_closed: false,
        }))
    }

    fn read_bounded(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, CacheIoReadError> {
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
}

impl FaultyCacheIndex {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: SqliteCacheIndex::new(),
            fail_commit_publish: AtomicBool::new(false),
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
    while fs.writers_opened() < 2 {
        fs.writer_opened_notify.notified().await;
    }

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
    while fs.writers_opened() < 2 {
        fs.writer_opened_notify.notified().await;
    }

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
    while fs.writers_opened() < 1 {
        fs.writer_opened_notify.notified().await;
    }

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
    while fs.writers_opened() < 1 {
        fs.writer_opened_notify.notified().await;
    }

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

    // Poll once to send BeginPut, then wait until writer opens staging.
    tokio::select! {
        _ = &mut put_task => panic!("put should wait on empty stream"),
        _ = fs.writer_opened_notify.notified() => {}
    }
    assert_eq!(fs.open_handles(), 1);

    let mut removed_rx = fs.subscribe_removed();
    // Drop caller future; CancelOnDrop cancels reservation through the actor.
    drop(put_task);

    while fs.open_handles() > 0 {
        fs.writer_closed_notify.notified().await;
    }
    removed_rx.wait_for(|&c| c >= 1).await.unwrap();

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
