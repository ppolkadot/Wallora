use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::provider::ImageClass;
use crate::services::cache::types::{CacheError, CacheKey, CacheNamespace};

/// Computes IEEE CRC-32 checksum incrementally for integrity checks on cache payloads.
#[derive(Debug, Clone)]
pub struct Crc32Hasher {
    state: u32,
}

impl Default for Crc32Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32Hasher {
    pub fn new() -> Self {
        Self { state: 0xFFFF_FFFF }
    }

    pub fn update(&mut self, data: &[u8]) {
        for &byte in data {
            self.state ^= u32::from(byte);
            for _ in 0..8 {
                let mask = (self.state & 1).wrapping_neg();
                self.state = (self.state >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }

    pub fn finalize(self) -> u32 {
        !self.state
    }
}

pub fn crc32_ieee(data: &[u8]) -> u32 {
    let mut hasher = Crc32Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

/// Exclusive lock guard keeping `<cache_root>/cache.lock` owned for the actor's lifetime.
pub trait CacheLockGuard: Send + Sync {}

/// Directory entry returned by [`CacheFs::list_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheDirEntry {
    pub file_name: OsString,
    pub byte_len: u64,
    pub is_file: bool,
}

/// Errors returned when acquiring the exclusive cache root lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheLockError {
    Locked,
    Io(String),
}

/// Errors returned by bounded reads of indexed cache content files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheIoReadError {
    NotFound,
    TooLarge,
    Io(String),
}

/// Exclusive handle to a staging file opened with `create_new`.
pub trait StagingWriter: Send {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    fn close(self: Box<Self>) -> io::Result<()>;
}

/// Injectable filesystem seam for cache locking, staging, publication, reads, and reconciliation.
pub trait CacheFs: Send + Sync + 'static {
    fn create_dir_all(&self, path: &Path) -> io::Result<()>;

    /// Ensures a recovery marker exists and returns whether it was already present.
    fn create_recovery_marker(&self, path: &Path) -> io::Result<bool>;

    fn try_lock_exclusive(
        &self,
        lock_path: &Path,
    ) -> Result<Box<dyn CacheLockGuard>, CacheLockError>;

    fn create_new_writer(&self, path: &Path) -> io::Result<Box<dyn StagingWriter>>;

    fn read_bounded(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, CacheIoReadError>;

    fn file_len(&self, path: &Path) -> Result<Option<u64>, io::Error>;

    /// Atomically publishes `from` to a fresh path `to`, failing if `to` already exists.
    fn rename_no_overwrite(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// Deletes `path`; confirmed absence (`NotFound`) must return `Ok(())`.
    fn remove_file(&self, path: &Path) -> io::Result<()>;

    fn list_dir(&self, path: &Path) -> io::Result<Vec<CacheDirEntry>>;
}

/// Production OS filesystem implementation of [`CacheFs`].
#[derive(Debug, Default)]
pub struct RealCacheFs;

struct RealLockGuard {
    _file: File,
}

impl CacheLockGuard for RealLockGuard {}

struct RealStagingWriter {
    file: Option<File>,
}

impl StagingWriter for RealStagingWriter {
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("staging writer already closed"))?;
        file.write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("staging writer already closed"))?;
        file.flush()
    }

    fn close(mut self: Box<Self>) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            drop(file);
        }
        Ok(())
    }
}

impl Drop for RealStagingWriter {
    fn drop(&mut self) {
        drop(self.file.take());
    }
}

impl CacheFs for RealCacheFs {
    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn create_recovery_marker(&self, path: &Path) -> io::Result<bool> {
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(file) => {
                file.sync_all()?;
                Ok(false)
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(true),
            Err(err) => Err(err),
        }
    }

    fn try_lock_exclusive(
        &self,
        lock_path: &Path,
    ) -> Result<Box<dyn CacheLockGuard>, CacheLockError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|err| CacheLockError::Io(err.to_string()))?;
        match file.try_lock() {
            Ok(()) => Ok(Box::new(RealLockGuard { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Err(CacheLockError::Locked),
            Err(err) => Err(CacheLockError::Io(err.to_string())),
        }
    }

    fn create_new_writer(&self, path: &Path) -> io::Result<Box<dyn StagingWriter>> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        Ok(Box::new(RealStagingWriter { file: Some(file) }))
    }

    fn read_bounded(&self, path: &Path, max_bytes: usize) -> Result<Vec<u8>, CacheIoReadError> {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(CacheIoReadError::NotFound);
            }
            Err(err) => return Err(CacheIoReadError::Io(err.to_string())),
        };
        let meta = file
            .metadata()
            .map_err(|err| CacheIoReadError::Io(err.to_string()))?;
        if meta.len() > max_bytes as u64 {
            return Err(CacheIoReadError::TooLarge);
        }
        let mut buf = Vec::with_capacity(meta.len() as usize);
        let limit = (max_bytes as u64).saturating_add(1);
        file.take(limit)
            .read_to_end(&mut buf)
            .map_err(|err| CacheIoReadError::Io(err.to_string()))?;
        if buf.len() > max_bytes {
            return Err(CacheIoReadError::TooLarge);
        }
        Ok(buf)
    }

    fn file_len(&self, path: &Path) -> Result<Option<u64>, io::Error> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => Ok(meta.is_file().then_some(meta.len())),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    fn rename_no_overwrite(&self, from: &Path, to: &Path) -> io::Result<()> {
        match std::fs::hard_link(from, to) {
            Ok(()) => {
                if let Err(err) = std::fs::remove_file(from) {
                    let _ = std::fs::remove_file(to);
                    return Err(err);
                }
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Err(err),
            Err(_) => match std::fs::symlink_metadata(to) {
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination cache object already exists",
                )),
                Err(err) if err.kind() == io::ErrorKind::NotFound => {
                    // On filesystems without hard links, the root lock and single
                    // actor own the fresh UUID destination across this check and move.
                    std::fs::rename(from, to)
                }
                Err(err) => Err(err),
            },
        }
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<CacheDirEntry>> {
        let mut entries = Vec::new();
        for item in std::fs::read_dir(path)? {
            let entry = item?;
            let file_type = entry.file_type()?;
            let byte_len = if file_type.is_file() {
                entry.metadata()?.len()
            } else {
                0
            };
            entries.push(CacheDirEntry {
                file_name: entry.file_name(),
                byte_len,
                is_file: file_type.is_file(),
            });
        }
        Ok(entries)
    }
}

/// Injectable clock source for cache timestamps and deterministic tests.
pub trait CacheClock: Send + Sync + 'static {
    fn now_unix_secs(&self) -> i64;
}

/// Wall-clock implementation of [`CacheClock`].
#[derive(Debug, Default)]
pub struct SystemCacheClock;

impl CacheClock for SystemCacheClock {
    fn now_unix_secs(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

/// Canonical primary key for an entry in the SQLite ready index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheEntryKey {
    pub namespace: CacheNamespace,
    pub key: CacheKey,
}

impl CacheEntryKey {
    pub fn new(namespace: CacheNamespace, key: CacheKey) -> Self {
        Self { namespace, key }
    }
}

/// Committed ready entry row stored in the cache SQLite index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedEntryRecord {
    pub entry_key: CacheEntryKey,
    pub rel_path: String,
    pub byte_len: u64,
    pub crc32: u32,
    pub last_access: u64,
    pub created_at_unix: i64,
}

/// Persistent tombstone row for an unreadable file whose deletion is pending or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDeleteRecord {
    pub rel_path: String,
    pub byte_len: u64,
}

/// Snapshot loaded from the cache SQLite index during startup reconciliation.
#[derive(Debug, Clone, Default)]
pub struct CacheIndexSnapshot {
    pub ready_entries: Vec<PublishedEntryRecord>,
    pub pending_deletes: Vec<PendingDeleteRecord>,
}

/// Injectable metadata index seam backed by SQLite in production.
pub trait CacheIndex: Send + Sync + 'static {
    fn open_and_load(&self, db_path: &Path) -> Result<CacheIndexSnapshot, CacheError>;

    fn commit_publish(
        &self,
        entry: &PublishedEntryRecord,
        previous_to_tombstone: Option<&PendingDeleteRecord>,
    ) -> Result<(), CacheError>;

    fn touch_last_access(
        &self,
        entry_key: &CacheEntryKey,
        last_access: u64,
    ) -> Result<(), CacheError>;

    fn hide_and_queue_deletes(
        &self,
        entries: &[CacheEntryKey],
        deletes: &[PendingDeleteRecord],
    ) -> Result<(), CacheError>;

    fn record_pending_deletes(&self, deletes: &[PendingDeleteRecord]) -> Result<(), CacheError>;

    fn mark_delete_completed(&self, rel_path: &str) -> Result<(), CacheError>;
}

fn class_to_db_str(class: ImageClass) -> &'static str {
    match class {
        ImageClass::Thumbnail => "thumb",
        ImageClass::Preview => "preview",
    }
}

fn class_from_db_str(raw: &str) -> Option<ImageClass> {
    match raw {
        "thumb" => Some(ImageClass::Thumbnail),
        "preview" => Some(ImageClass::Preview),
        _ => None,
    }
}

const CACHE_SCHEMA_VERSION: u32 = 1;

const CACHE_MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS cache_entries (
    namespace TEXT NOT NULL,
    image_class TEXT NOT NULL,
    cache_key TEXT NOT NULL,
    rel_path TEXT NOT NULL UNIQUE,
    byte_len INTEGER NOT NULL,
    crc32 INTEGER NOT NULL,
    last_access INTEGER NOT NULL,
    created_at_unix INTEGER NOT NULL,
    PRIMARY KEY (namespace, image_class, cache_key)
);

CREATE INDEX IF NOT EXISTS idx_cache_entries_lru
    ON cache_entries (last_access ASC, created_at_unix ASC);

CREATE TABLE IF NOT EXISTS pending_deletes (
    rel_path TEXT PRIMARY KEY NOT NULL,
    byte_len INTEGER NOT NULL
);
"#;

/// SQLite-backed implementation of [`CacheIndex`], stored inside `<cache_root>/cache_index.db`.
pub struct SqliteCacheIndex {
    conn: Mutex<Option<Connection>>,
}

impl Default for SqliteCacheIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl SqliteCacheIndex {
    pub fn new() -> Self {
        Self {
            conn: Mutex::new(None),
        }
    }

    fn with_conn<F, R>(&self, f: F) -> Result<R, CacheError>
    where
        F: FnOnce(&mut Connection) -> Result<R, CacheError>,
    {
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| CacheError::Index("cache index mutex poisoned".to_string()))?;
        let conn = guard
            .as_mut()
            .ok_or_else(|| CacheError::Index("cache index not opened".to_string()))?;
        f(conn)
    }
}

fn configure_and_migrate_cache_db(conn: &mut Connection) -> Result<(), CacheError> {
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| CacheError::Index(e.to_string()))?;
    conn.pragma_update(None, "busy_timeout", 5_000i64)
        .map_err(|e| CacheError::Index(e.to_string()))?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|e| CacheError::Index(e.to_string()))?;
    let journal_mode: String = conn
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .map_err(|e| CacheError::Index(e.to_string()))?;
    if journal_mode != "wal" {
        return Err(CacheError::Index(format!(
            "cache index requires WAL durability (got {journal_mode})"
        )));
    }

    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|e| CacheError::Index(e.to_string()))?;
    if version > CACHE_SCHEMA_VERSION {
        return Err(CacheError::Index(format!(
            "cache schema version {version} is newer than supported version {CACHE_SCHEMA_VERSION}"
        )));
    }
    if version < CACHE_SCHEMA_VERSION {
        let tx = conn
            .transaction()
            .map_err(|e| CacheError::Index(e.to_string()))?;
        tx.execute_batch(CACHE_MIGRATION_V1)
            .map_err(|e| CacheError::Index(e.to_string()))?;
        tx.pragma_update(None, "user_version", CACHE_SCHEMA_VERSION)
            .map_err(|e| CacheError::Index(e.to_string()))?;
        tx.commit().map_err(|e| CacheError::Index(e.to_string()))?;
    }
    Ok(())
}

fn valid_cache_rel_path(path: &str) -> bool {
    let Some((dir, name)) = path.split_once('/') else {
        return false;
    };
    matches!(dir, "objects" | "staging")
        && !name.is_empty()
        && !name.ends_with(['.', ' '])
        && !name.contains(['/', '\\', ':', '\0'])
}

fn load_ready_row(row: &rusqlite::Row<'_>) -> Option<PublishedEntryRecord> {
    let namespace: String = row.get(1).ok()?;
    let class: String = row.get(2).ok()?;
    let key: String = row.get(3).ok()?;
    let rel_path: String = row.get(4).ok()?;
    let byte_len = u64::try_from(row.get::<_, i64>(5).ok()?).ok()?;
    if !valid_cache_rel_path(&rel_path) || byte_len == 0 {
        return None;
    }
    Some(PublishedEntryRecord {
        entry_key: CacheEntryKey::new(
            CacheNamespace::from_db_str(&namespace)?,
            CacheKey::new(class_from_db_str(&class)?, key).ok()?,
        ),
        rel_path,
        byte_len,
        crc32: u32::try_from(row.get::<_, i64>(6).ok()?).ok()?,
        last_access: u64::try_from(row.get::<_, i64>(7).ok()?).ok()?,
        created_at_unix: row.get(8).ok()?,
    })
}

fn load_cache_db(db_path: &Path) -> Result<(Connection, CacheIndexSnapshot), CacheError> {
    let mut conn = Connection::open(db_path).map_err(cache_index_error)?;
    configure_and_migrate_cache_db(&mut conn)?;
    let tx = conn.transaction().map_err(cache_index_error)?;
    let mut snapshot = CacheIndexSnapshot::default();
    let mut bad_ready = Vec::new();
    let mut bad_pending = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT rowid, namespace, image_class, cache_key, rel_path, byte_len, crc32, last_access, created_at_unix
             FROM cache_entries ORDER BY last_access ASC, created_at_unix ASC",
        ).map_err(cache_index_error)?;
        let mut rows = stmt.query([]).map_err(cache_index_error)?;
        while let Some(row) = rows.next().map_err(cache_index_error)? {
            if let Some(record) = load_ready_row(row) {
                snapshot.ready_entries.push(record);
            } else {
                bad_ready.push(row.get::<_, i64>(0).map_err(cache_index_error)?);
            }
        }
    }
    {
        let mut stmt = tx
            .prepare("SELECT rowid, rel_path, byte_len FROM pending_deletes ORDER BY rel_path ASC")
            .map_err(cache_index_error)?;
        let mut rows = stmt.query([]).map_err(cache_index_error)?;
        while let Some(row) = rows.next().map_err(cache_index_error)? {
            if let (Ok(rel_path), Ok(byte_len)) = (row.get::<_, String>(1), row.get::<_, i64>(2))
                && valid_cache_rel_path(&rel_path)
                && let Ok(byte_len) = u64::try_from(byte_len)
            {
                snapshot
                    .pending_deletes
                    .push(PendingDeleteRecord { rel_path, byte_len });
            } else {
                bad_pending.push(row.get::<_, i64>(0).map_err(cache_index_error)?);
            }
        }
    }
    for rowid in bad_ready {
        tx.execute("DELETE FROM cache_entries WHERE rowid = ?1", [rowid])
            .map_err(cache_index_error)?;
    }
    for rowid in bad_pending {
        tx.execute("DELETE FROM pending_deletes WHERE rowid = ?1", [rowid])
            .map_err(cache_index_error)?;
    }
    tx.commit().map_err(cache_index_error)?;
    Ok((conn, snapshot))
}

fn cache_index_error(err: rusqlite::Error) -> CacheError {
    CacheError::Index(err.to_string())
}

impl CacheIndex for SqliteCacheIndex {
    fn open_and_load(&self, db_path: &Path) -> Result<CacheIndexSnapshot, CacheError> {
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| CacheError::Index("cache index mutex poisoned".to_string()))?;
        // Drop any prior connection before moving the DB and its SQLite sidecars.
        guard.take();
        let (conn, snapshot) = match load_cache_db(db_path) {
            Ok(loaded) => loaded,
            Err(err) => {
                tracing::warn!(%err, "rebuilding unusable cache index");
                let mut quarantine = db_path.as_os_str().to_os_string();
                quarantine.push(format!(".corrupt-{}", uuid::Uuid::new_v4()));
                for suffix in ["-wal", "-shm", "-journal", ""] {
                    let mut source = db_path.as_os_str().to_os_string();
                    source.push(suffix);
                    let mut destination = quarantine.clone();
                    destination.push(suffix);
                    match std::fs::rename(source, destination) {
                        Ok(()) => {}
                        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                        Err(err) => {
                            return Err(CacheError::Index(format!(
                                "could not quarantine cache index: {err}"
                            )));
                        }
                    }
                }
                load_cache_db(db_path)?
            }
        };
        *guard = Some(conn);
        Ok(snapshot)
    }

    fn commit_publish(
        &self,
        entry: &PublishedEntryRecord,
        previous_to_tombstone: Option<&PendingDeleteRecord>,
    ) -> Result<(), CacheError> {
        self.with_conn(|conn| {
            let tx = conn
                .transaction()
                .map_err(|e| CacheError::Index(e.to_string()))?;
            let ns = entry.entry_key.namespace.as_db_str();
            let class = class_to_db_str(entry.entry_key.key.class);
            let existing_rel: Option<(String, i64)> = tx
                .query_row(
                    "SELECT rel_path, byte_len FROM cache_entries
                     WHERE namespace = ?1 AND image_class = ?2 AND cache_key = ?3",
                    params![&ns, class, &entry.entry_key.key.key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| CacheError::Index(e.to_string()))?;

            if let Some(prev) = previous_to_tombstone {
                tx.execute(
                    "INSERT OR REPLACE INTO pending_deletes (rel_path, byte_len) VALUES (?1, ?2)",
                    params![&prev.rel_path, i64::try_from(prev.byte_len).unwrap_or(i64::MAX)],
                )
                .map_err(|e| CacheError::Index(e.to_string()))?;
            } else if let Some((prev_path, prev_len)) = existing_rel
                && prev_path != entry.rel_path
            {
                tx.execute(
                    "INSERT OR REPLACE INTO pending_deletes (rel_path, byte_len) VALUES (?1, ?2)",
                    params![prev_path, prev_len],
                )
                .map_err(|e| CacheError::Index(e.to_string()))?;
            }

            tx.execute(
                "INSERT INTO cache_entries
                    (namespace, image_class, cache_key, rel_path, byte_len, crc32, last_access, created_at_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(namespace, image_class, cache_key) DO UPDATE SET
                    rel_path = excluded.rel_path,
                    byte_len = excluded.byte_len,
                    crc32 = excluded.crc32,
                    last_access = excluded.last_access,
                    created_at_unix = excluded.created_at_unix",
                params![
                    ns,
                    class,
                    &entry.entry_key.key.key,
                    &entry.rel_path,
                    i64::try_from(entry.byte_len).unwrap_or(i64::MAX),
                    i64::from(entry.crc32),
                    i64::try_from(entry.last_access).unwrap_or(i64::MAX),
                    entry.created_at_unix,
                ],
            )
            .map_err(|e| CacheError::Index(e.to_string()))?;

            tx.commit().map_err(|e| CacheError::Index(e.to_string()))?;
            Ok(())
        })
    }

    fn touch_last_access(
        &self,
        entry_key: &CacheEntryKey,
        last_access: u64,
    ) -> Result<(), CacheError> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE cache_entries SET last_access = ?1
                 WHERE namespace = ?2 AND image_class = ?3 AND cache_key = ?4",
                params![
                    i64::try_from(last_access).unwrap_or(i64::MAX),
                    entry_key.namespace.as_db_str(),
                    class_to_db_str(entry_key.key.class),
                    &entry_key.key.key,
                ],
            )
            .map_err(|e| CacheError::Index(e.to_string()))?;
            Ok(())
        })
    }

    fn hide_and_queue_deletes(
        &self,
        entries: &[CacheEntryKey],
        deletes: &[PendingDeleteRecord],
    ) -> Result<(), CacheError> {
        if entries.is_empty() && deletes.is_empty() {
            return Ok(());
        }
        self.with_conn(|conn| {
            let tx = conn
                .transaction()
                .map_err(|e| CacheError::Index(e.to_string()))?;
            for entry_key in entries {
                tx.execute(
                    "DELETE FROM cache_entries
                     WHERE namespace = ?1 AND image_class = ?2 AND cache_key = ?3",
                    params![
                        entry_key.namespace.as_db_str(),
                        class_to_db_str(entry_key.key.class),
                        &entry_key.key.key,
                    ],
                )
                .map_err(|e| CacheError::Index(e.to_string()))?;
            }
            for del in deletes {
                tx.execute(
                    "INSERT OR REPLACE INTO pending_deletes (rel_path, byte_len) VALUES (?1, ?2)",
                    params![
                        &del.rel_path,
                        i64::try_from(del.byte_len).unwrap_or(i64::MAX)
                    ],
                )
                .map_err(|e| CacheError::Index(e.to_string()))?;
            }
            tx.commit().map_err(|e| CacheError::Index(e.to_string()))?;
            Ok(())
        })
    }

    fn record_pending_deletes(&self, deletes: &[PendingDeleteRecord]) -> Result<(), CacheError> {
        if deletes.is_empty() {
            return Ok(());
        }
        self.with_conn(|conn| {
            let tx = conn
                .transaction()
                .map_err(|e| CacheError::Index(e.to_string()))?;
            for del in deletes {
                tx.execute(
                    "INSERT OR REPLACE INTO pending_deletes (rel_path, byte_len) VALUES (?1, ?2)",
                    params![
                        &del.rel_path,
                        i64::try_from(del.byte_len).unwrap_or(i64::MAX)
                    ],
                )
                .map_err(|e| CacheError::Index(e.to_string()))?;
            }
            tx.commit().map_err(|e| CacheError::Index(e.to_string()))?;
            Ok(())
        })
    }

    fn mark_delete_completed(&self, rel_path: &str) -> Result<(), CacheError> {
        self.with_conn(|conn| {
            conn.execute(
                "DELETE FROM pending_deletes WHERE rel_path = ?1",
                params![rel_path],
            )
            .map_err(|e| CacheError::Index(e.to_string()))?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("wallora-rename-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rename_no_overwrite_preserves_existing_destination_and_source() {
        let dir = TempDir::new();
        let from = dir.0.join("staging");
        let to = dir.0.join("object");
        std::fs::write(&from, b"source").unwrap();
        std::fs::write(&to, b"existing").unwrap();
        assert_eq!(
            RealCacheFs
                .rename_no_overwrite(&from, &to)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(std::fs::read(&from).unwrap(), b"source");
        assert_eq!(std::fs::read(&to).unwrap(), b"existing");
    }

    #[test]
    fn rename_no_overwrite_moves_file_to_fresh_destination() {
        let dir = TempDir::new();
        let from = dir.0.join("staging");
        let to = dir.0.join("object");
        std::fs::write(&from, b"payload").unwrap();
        RealCacheFs.rename_no_overwrite(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).unwrap(), b"payload");
    }
}
