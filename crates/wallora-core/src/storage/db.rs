use std::fs::{File, OpenOptions};
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    AccountId, AccountRecord, CollectionSummary, DEFAULT_CACHE_CAP_BYTES, LocalPreferences,
    ProxyConfig, UserSettings, UsernameVerification, validate_cache_cap_preference,
};
use crate::storage::migrations;

/// Non-secret persisted account cache introduced in Task 3.
///
/// Purged on account transitions. Pending cleanup hides this data until SQLite can delete it.
/// Never stores API keys.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountCachedState {
    pub user_settings: Option<UserSettings>,
    pub own_collections: Option<Vec<CollectionSummary>>,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StorageError {
    #[error("failed to open SQLite database: {0}")]
    Open(String),
    #[error("another Wallora instance owns this database")]
    SessionOwned,
    #[error("durable SQLite storage is required")]
    NotDurable,
    #[error("failed to migrate SQLite database: {0}")]
    Migrate(String),
    #[error("SQLite operation failed: {0}")]
    Query(String),
    #[error("failed to encode or decode stored metadata: {0}")]
    Serialization(String),
    #[error("storage lock poisoned")]
    LockPoisoned,
}

/// Persistence contract for account metadata, Task 3 account caches, and local preferences.
pub trait SessionStorage: Send + Sync + 'static {
    fn list_accounts(&self) -> Result<Vec<AccountRecord>, StorageError>;

    fn active_account(&self) -> Result<Option<AccountRecord>, StorageError>;

    /// Atomically saves `account`, marks it active (deactivating all others), optionally stores
    /// its initial non-secret cache, and purges any outgoing active account's cache if requested.
    fn save_and_activate_account(
        &self,
        account: &AccountRecord,
        initial_cache: Option<&AccountCachedState>,
        purge_outgoing_cache: bool,
    ) -> Result<(), StorageError>;

    /// Atomically switches active account (or to Guest) and purges the outgoing cache.
    fn set_active_account(&self, account_id: Option<&AccountId>) -> Result<(), StorageError>;

    fn is_durable(&self) -> bool;

    /// Records an intent before any vault write or delete. Existing accounts with this ID
    /// are tombstoned immediately, even if subsequent cache or vault cleanup fails.
    fn queue_credential_cleanup(&self, account_id: &AccountId) -> Result<(), StorageError>;

    fn pending_credential_cleanup(&self) -> Result<Vec<AccountId>, StorageError>;

    fn deactivate_and_purge_account(&self, account_id: &AccountId) -> Result<(), StorageError>;

    /// Removes account metadata and any cleanup intent after idempotent vault deletion succeeds.
    fn finish_credential_cleanup(&self, account_id: &AccountId) -> Result<(), StorageError>;

    fn load_account_cache(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<AccountCachedState>, StorageError>;

    fn save_refresh(
        &self,
        account_id: &AccountId,
        verification: Option<UsernameVerification>,
        cache: &AccountCachedState,
    ) -> Result<(), StorageError>;

    fn load_local_preferences(&self) -> Result<LocalPreferences, StorageError>;

    fn save_local_preferences(&self, prefs: &LocalPreferences) -> Result<(), StorageError>;
}

/// SQLite-backed implementation of [`SessionStorage`].
pub struct SqliteStore {
    conn: Mutex<Connection>,
    // Held until after SQLite closes. Never unlink the lock file.
    owner: Option<File>,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| StorageError::Open(err.to_string()))?;
        }
        // The app uses one fixed database path. The OS releases the lock on process exit,
        // including crashes, so restart can reconcile outstanding vault work.
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("session.lock"))
            .map_err(|err| StorageError::Open(err.to_string()))?;
        match owner.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(StorageError::SessionOwned),
            Err(err) => return Err(StorageError::Open(err.to_string())),
        }
        let mut conn = Connection::open(path).map_err(|err| StorageError::Open(err.to_string()))?;
        configure_connection(&mut conn, true)?;
        migrations::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            owner: Some(owner),
        })
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        let mut conn =
            Connection::open_in_memory().map_err(|err| StorageError::Open(err.to_string()))?;
        configure_connection(&mut conn, false)?;
        migrations::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            owner: None,
        })
    }

    fn lock_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StorageError> {
        self.conn.lock().map_err(|_| StorageError::LockPoisoned)
    }
}

fn configure_connection(conn: &mut Connection, enable_wal: bool) -> Result<(), StorageError> {
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|err| StorageError::Open(err.to_string()))?;
    conn.pragma_update(None, "busy_timeout", 5_000i64)
        .map_err(|err| StorageError::Open(err.to_string()))?;
    if enable_wal {
        conn.pragma_update(None, "synchronous", "FULL")
            .map_err(|err| StorageError::Open(err.to_string()))?;
        let journal_mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(|err| StorageError::Open(err.to_string()))?;
        if journal_mode != "wal" {
            return Err(StorageError::NotDurable);
        }
    }
    Ok(())
}

fn row_to_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRecord> {
    let id: String = row.get(0)?;
    let username: String = row.get(1)?;
    let verification_raw: String = row.get(2)?;
    let is_active: i64 = row.get(3)?;
    let created_at_unix: i64 = row.get(4)?;
    let updated_at_unix: i64 = row.get(5)?;
    let verification = UsernameVerification::from_db_str(&verification_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid verification_state value",
            )),
        )
    })?;
    Ok(AccountRecord {
        id: AccountId::new(id),
        username,
        verification,
        is_active: is_active != 0,
        created_at_unix,
        updated_at_unix,
    })
}

impl SessionStorage for SqliteStore {
    fn list_accounts(&self) -> Result<Vec<AccountRecord>, StorageError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT account_id, username, verification_state, is_active, created_at_unix, updated_at_unix
                 FROM accounts
                 WHERE account_id NOT IN (SELECT account_id FROM pending_credential_cleanup)
                 ORDER BY is_active DESC, updated_at_unix DESC, account_id ASC",
            )
            .map_err(|err| StorageError::Query(err.to_string()))?;
        let rows = stmt
            .query_map([], row_to_account)
            .map_err(|err| StorageError::Query(err.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|err| StorageError::Query(err.to_string()))?);
        }
        Ok(out)
    }

    fn active_account(&self) -> Result<Option<AccountRecord>, StorageError> {
        let conn = self.lock_conn()?;
        conn.query_row(
            "SELECT account_id, username, verification_state, is_active, created_at_unix, updated_at_unix
             FROM accounts
             WHERE is_active = 1
               AND account_id NOT IN (SELECT account_id FROM pending_credential_cleanup)
             ORDER BY updated_at_unix DESC
             LIMIT 1",
            [],
            row_to_account,
        )
        .optional()
        .map_err(|err| StorageError::Query(err.to_string()))
    }

    fn save_and_activate_account(
        &self,
        account: &AccountRecord,
        initial_cache: Option<&AccountCachedState>,
        purge_outgoing_cache: bool,
    ) -> Result<(), StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction()
            .map_err(|err| StorageError::Query(err.to_string()))?;

        if purge_outgoing_cache {
            tx.execute(
                "DELETE FROM account_cache WHERE account_id IN (SELECT account_id FROM accounts WHERE is_active = 1 AND account_id != ?1)",
                params![account.id.as_str()],
            )
            .map_err(|err| StorageError::Query(err.to_string()))?;
        }

        tx.execute("UPDATE accounts SET is_active = 0 WHERE is_active = 1", [])
            .map_err(|err| StorageError::Query(err.to_string()))?;

        tx.execute(
            "INSERT INTO accounts (account_id, username, verification_state, is_active, created_at_unix, updated_at_unix)
             VALUES (?1, ?2, ?3, 1, ?4, ?5)
             ON CONFLICT(account_id) DO UPDATE SET
                 username = excluded.username,
                 verification_state = excluded.verification_state,
                 is_active = 1,
                 updated_at_unix = excluded.updated_at_unix",
            params![
                account.id.as_str(),
                account.username,
                account.verification.as_db_str(),
                account.created_at_unix,
                account.updated_at_unix,
            ],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;

        if let Some(cache) = initial_cache {
            write_account_cache(&tx, &account.id, cache)?;
        }
        tx.execute(
            "DELETE FROM pending_credential_cleanup WHERE account_id = ?1",
            [account.id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;

        tx.commit()
            .map_err(|err| StorageError::Query(err.to_string()))?;
        Ok(())
    }

    fn set_active_account(&self, account_id: Option<&AccountId>) -> Result<(), StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction()
            .map_err(|err| StorageError::Query(err.to_string()))?;

        tx.execute(
            "DELETE FROM account_cache WHERE account_id IN (
                SELECT account_id FROM accounts WHERE is_active = 1
                AND (?1 IS NULL OR account_id != ?1)
            )",
            [account_id.map(AccountId::as_str)],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;

        tx.execute("UPDATE accounts SET is_active = 0 WHERE is_active = 1", [])
            .map_err(|err| StorageError::Query(err.to_string()))?;

        if let Some(target) = account_id {
            let updated = tx
                .execute(
                    "UPDATE accounts SET is_active = 1 WHERE account_id = ?1
                     AND account_id NOT IN (SELECT account_id FROM pending_credential_cleanup)",
                    params![target.as_str()],
                )
                .map_err(|err| StorageError::Query(err.to_string()))?;
            if updated == 0 {
                return Err(StorageError::Query("target account not found".to_string()));
            }
        }

        tx.commit()
            .map_err(|err| StorageError::Query(err.to_string()))?;
        Ok(())
    }

    fn is_durable(&self) -> bool {
        self.owner.is_some()
    }

    fn queue_credential_cleanup(&self, account_id: &AccountId) -> Result<(), StorageError> {
        if !self.is_durable() {
            return Err(StorageError::NotDurable);
        }
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO pending_credential_cleanup (account_id) VALUES (?1)",
            [account_id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        Ok(())
    }

    fn pending_credential_cleanup(&self) -> Result<Vec<AccountId>, StorageError> {
        let conn = self.lock_conn()?;
        let mut stmt = conn
            .prepare("SELECT account_id FROM pending_credential_cleanup ORDER BY account_id")
            .map_err(|err| StorageError::Query(err.to_string()))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0).map(AccountId::new))
            .map_err(|err| StorageError::Query(err.to_string()))?;
        rows.collect::<Result<_, _>>()
            .map_err(|err| StorageError::Query(err.to_string()))
    }

    fn deactivate_and_purge_account(&self, account_id: &AccountId) -> Result<(), StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction()
            .map_err(|err| StorageError::Query(err.to_string()))?;
        tx.execute(
            "UPDATE accounts SET is_active = 0 WHERE account_id = ?1",
            [account_id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        tx.execute(
            "DELETE FROM account_cache WHERE account_id = ?1",
            [account_id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        tx.commit()
            .map_err(|err| StorageError::Query(err.to_string()))
    }

    fn finish_credential_cleanup(&self, account_id: &AccountId) -> Result<(), StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction()
            .map_err(|err| StorageError::Query(err.to_string()))?;
        tx.execute(
            "DELETE FROM accounts WHERE account_id = ?1",
            [account_id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        tx.execute(
            "DELETE FROM pending_credential_cleanup WHERE account_id = ?1",
            [account_id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        tx.commit()
            .map_err(|err| StorageError::Query(err.to_string()))
    }

    fn load_account_cache(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<AccountCachedState>, StorageError> {
        let conn = self.lock_conn()?;
        let row: Option<(Option<String>, Option<String>, i64)> = conn
            .query_row(
                "SELECT user_settings_json, own_collections_json, updated_at_unix
                 FROM account_cache
                 WHERE account_id = ?1
                   AND account_id NOT IN (SELECT account_id FROM pending_credential_cleanup)",
                params![account_id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|err| StorageError::Query(err.to_string()))?;

        let Some((settings_raw, collections_raw, updated_at_unix)) = row else {
            return Ok(None);
        };

        let user_settings = settings_raw
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|err| StorageError::Serialization(err.to_string()))?;
        let own_collections = collections_raw
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|err| StorageError::Serialization(err.to_string()))?;

        Ok(Some(AccountCachedState {
            user_settings,
            own_collections,
            updated_at_unix,
        }))
    }

    fn save_refresh(
        &self,
        account_id: &AccountId,
        verification: Option<UsernameVerification>,
        cache: &AccountCachedState,
    ) -> Result<(), StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction()
            .map_err(|err| StorageError::Query(err.to_string()))?;
        let updated = tx
            .execute(
                "UPDATE accounts SET verification_state = COALESCE(?1, verification_state),
                                 updated_at_unix = ?2
             WHERE account_id = ?3 AND is_active = 1
               AND account_id NOT IN (SELECT account_id FROM pending_credential_cleanup)",
                params![
                    verification.map(UsernameVerification::as_db_str),
                    cache.updated_at_unix,
                    account_id.as_str()
                ],
            )
            .map_err(|err| StorageError::Query(err.to_string()))?;
        if updated == 0 {
            return Err(StorageError::Query(
                "refresh account is no longer active".into(),
            ));
        }
        write_account_cache(&tx, account_id, cache)?;
        tx.commit()
            .map_err(|err| StorageError::Query(err.to_string()))
    }

    fn load_local_preferences(&self) -> Result<LocalPreferences, StorageError> {
        let conn = self.lock_conn()?;
        let row: Option<(String, Option<String>, Option<i64>)> = conn
            .query_row(
                "SELECT proxy_mode, proxy_url, cache_cap_bytes FROM local_preferences WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|err| StorageError::Query(err.to_string()))?;

        let Some((mode, url, cap)) = row else {
            return Ok(LocalPreferences::default());
        };

        let proxy = match mode.as_str() {
            "system" => ProxyConfig::System,
            "direct" => ProxyConfig::Direct,
            "manual" => match url {
                Some(raw) if ProxyConfig::validate_manual_url(&raw).is_ok() => {
                    ProxyConfig::Manual(raw)
                }
                _ => ProxyConfig::System,
            },
            _ => ProxyConfig::System,
        };
        // A missing, non-positive, or out-of-range stored cap falls back to the default instead of
        // failing startup or silently exceeding the 10 GB hard maximum.
        let cache_cap_bytes = cap
            .and_then(|raw| u64::try_from(raw).ok())
            .and_then(|raw| validate_cache_cap_preference(raw).ok())
            .unwrap_or(DEFAULT_CACHE_CAP_BYTES);
        Ok(LocalPreferences {
            proxy,
            cache_cap_bytes,
        })
    }

    fn save_local_preferences(&self, prefs: &LocalPreferences) -> Result<(), StorageError> {
        let normalized = prefs
            .proxy
            .normalized()
            .map_err(|err| StorageError::Serialization(err.to_string()))?;
        let cache_cap = validate_cache_cap_preference(prefs.cache_cap_bytes)
            .map_err(|err| StorageError::Serialization(err.to_string()))?;
        let cache_cap =
            i64::try_from(cache_cap).map_err(|err| StorageError::Serialization(err.to_string()))?;
        let (mode, url) = match &normalized {
            ProxyConfig::System => ("system", None),
            ProxyConfig::Direct => ("direct", None),
            ProxyConfig::Manual(raw) => ("manual", Some(raw.as_str())),
        };
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO local_preferences (id, proxy_mode, proxy_url, cache_cap_bytes)
             VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 proxy_mode = excluded.proxy_mode,
                 proxy_url = excluded.proxy_url,
                 cache_cap_bytes = excluded.cache_cap_bytes",
            params![mode, url, cache_cap],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        Ok(())
    }
}

fn write_account_cache(
    conn: &Connection,
    account_id: &AccountId,
    cache: &AccountCachedState,
) -> Result<(), StorageError> {
    let settings_json = cache
        .user_settings
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|err| StorageError::Serialization(err.to_string()))?;
    let collections_json = cache
        .own_collections
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|err| StorageError::Serialization(err.to_string()))?;

    conn.execute(
        "INSERT INTO account_cache (account_id, user_settings_json, own_collections_json, updated_at_unix)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(account_id) DO UPDATE SET
             user_settings_json = excluded.user_settings_json,
             own_collections_json = excluded.own_collections_json,
             updated_at_unix = excluded.updated_at_unix",
        params![
            account_id.as_str(),
            settings_json,
            collections_json,
            cache.updated_at_unix,
        ],
    )
    .map_err(|err| StorageError::Query(err.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_round_trips_accounts_caches_and_local_preferences() {
        let store = SqliteStore::open_in_memory().expect("open in-memory db");
        assert!(store.active_account().unwrap().is_none());
        assert_eq!(
            store.load_local_preferences().unwrap(),
            LocalPreferences::default()
        );

        let acct1 = AccountRecord {
            id: AccountId::generate(),
            username: "alice".to_string(),
            verification: UsernameVerification::Verified,
            is_active: true,
            created_at_unix: 100,
            updated_at_unix: 100,
        };
        let cache1 = AccountCachedState {
            user_settings: None,
            own_collections: Some(vec![CollectionSummary {
                id: 7,
                label: "Nature".to_string(),
                views: 12,
                public: true,
                count: 4,
            }]),
            updated_at_unix: 100,
        };
        store
            .save_and_activate_account(&acct1, Some(&cache1), true)
            .expect("save acct1");

        let prefs = LocalPreferences {
            proxy: ProxyConfig::Manual("http://127.0.0.1:8888/".to_string()),
            cache_cap_bytes: 3_500_000_000,
        };
        store.save_local_preferences(&prefs).expect("save prefs");
        assert_eq!(store.load_local_preferences().unwrap(), prefs);

        assert_eq!(store.active_account().unwrap(), Some(acct1.clone()));
        assert_eq!(
            store.load_account_cache(&acct1.id).unwrap(),
            Some(cache1.clone())
        );

        // Switching to acct2 purges acct1 cache while keeping local preferences intact.
        let acct2 = AccountRecord {
            id: AccountId::generate(),
            username: "bob".to_string(),
            verification: UsernameVerification::UnverifiablePrivate,
            is_active: true,
            created_at_unix: 200,
            updated_at_unix: 200,
        };
        store
            .save_and_activate_account(&acct2, None, true)
            .expect("save acct2");
        assert_eq!(store.active_account().unwrap(), Some(acct2.clone()));
        assert!(store.load_account_cache(&acct1.id).unwrap().is_none());
        assert_eq!(store.load_local_preferences().unwrap(), prefs);

        // Removing acct2 removes row and preserves local preferences.
        store
            .lock_conn()
            .unwrap()
            .execute(
                "INSERT INTO pending_credential_cleanup VALUES (?1)",
                [acct2.id.as_str()],
            )
            .unwrap();
        store
            .finish_credential_cleanup(&acct2.id)
            .expect("remove acct2");
        assert!(store.active_account().unwrap().is_none());
        assert_eq!(store.load_local_preferences().unwrap(), prefs);
    }

    #[test]
    fn cache_cap_round_trips_and_rejects_invalid_values_without_touching_stored_prefs() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert_eq!(
            store.load_local_preferences().unwrap().cache_cap_bytes,
            DEFAULT_CACHE_CAP_BYTES
        );

        let mut prefs = LocalPreferences {
            proxy: ProxyConfig::Direct,
            cache_cap_bytes: 10_000_000_000,
        };
        store.save_local_preferences(&prefs).unwrap();
        assert_eq!(store.load_local_preferences().unwrap(), prefs);

        for invalid in [0, 10_000_000_001, u64::MAX] {
            let rejected = LocalPreferences {
                cache_cap_bytes: invalid,
                ..prefs.clone()
            };
            assert!(store.save_local_preferences(&rejected).is_err());
            assert_eq!(store.load_local_preferences().unwrap(), prefs);
        }

        prefs.cache_cap_bytes = 1;
        store.save_local_preferences(&prefs).unwrap();
        assert_eq!(store.load_local_preferences().unwrap().cache_cap_bytes, 1);
    }

    #[test]
    fn out_of_range_stored_cap_falls_back_to_the_default_and_keeps_proxy() {
        let store = SqliteStore::open_in_memory().unwrap();
        for stored in [-5i64, 0, 10_000_000_001, i64::MAX] {
            store
                .lock_conn()
                .unwrap()
                .execute(
                    "INSERT INTO local_preferences (id, proxy_mode, proxy_url, cache_cap_bytes)
                     VALUES (1, 'direct', NULL, ?1)
                     ON CONFLICT(id) DO UPDATE SET cache_cap_bytes = excluded.cache_cap_bytes",
                    [stored],
                )
                .unwrap();
            let loaded = store.load_local_preferences().unwrap();
            assert_eq!(loaded.cache_cap_bytes, DEFAULT_CACHE_CAP_BYTES, "{stored}");
            assert_eq!(loaded.proxy, ProxyConfig::Direct);
        }
    }

    #[test]
    fn file_backed_v2_database_upgrades_in_place_preserving_proxy_preference() {
        let dir = std::env::temp_dir().join(format!("wallora-db-v2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wallora.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE accounts (
                     account_id TEXT PRIMARY KEY NOT NULL, username TEXT NOT NULL,
                     verification_state TEXT NOT NULL,
                     is_active INTEGER NOT NULL DEFAULT 0 CHECK (is_active IN (0, 1)),
                     created_at_unix INTEGER NOT NULL, updated_at_unix INTEGER NOT NULL);
                 CREATE TABLE account_cache (
                     account_id TEXT PRIMARY KEY NOT NULL
                         REFERENCES accounts(account_id) ON DELETE CASCADE,
                     user_settings_json TEXT, own_collections_json TEXT,
                     updated_at_unix INTEGER NOT NULL);
                 CREATE TABLE local_preferences (
                     id INTEGER PRIMARY KEY CHECK (id = 1), proxy_mode TEXT NOT NULL,
                     proxy_url TEXT);
                 CREATE TABLE pending_credential_cleanup (account_id TEXT PRIMARY KEY NOT NULL);
                 INSERT INTO local_preferences VALUES (1, 'manual', 'http://127.0.0.1:9000/');
                 PRAGMA user_version = 2;",
            )
            .unwrap();
        }
        {
            let store = SqliteStore::open(&path).unwrap();
            let loaded = store.load_local_preferences().unwrap();
            assert_eq!(
                loaded.proxy,
                ProxyConfig::Manual("http://127.0.0.1:9000/".to_string())
            );
            assert_eq!(loaded.cache_cap_bytes, DEFAULT_CACHE_CAP_BYTES);
            store
                .save_local_preferences(&LocalPreferences {
                    proxy: loaded.proxy.clone(),
                    cache_cap_bytes: 1_000_000_000,
                })
                .unwrap();
        }
        {
            let store = SqliteStore::open(&path).unwrap();
            let loaded = store.load_local_preferences().unwrap();
            assert_eq!(loaded.cache_cap_bytes, 1_000_000_000);
            assert_eq!(
                loaded.proxy,
                ProxyConfig::Manual("http://127.0.0.1:9000/".to_string())
            );
        }
        for name in [
            "wallora.db",
            "wallora.db-wal",
            "wallora.db-shm",
            "wallora.session.lock",
        ] {
            let _ = std::fs::remove_file(dir.join(name));
        }
        let _ = std::fs::remove_dir(&dir);
    }
}
