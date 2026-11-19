use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    AccountId, AccountRecord, CollectionSummary, LocalPreferences, ProxyConfig, UserSettings,
    UsernameVerification,
};
use crate::storage::migrations;

/// Non-secret persisted account cache introduced in Task 3.
///
/// Purged immediately on logout, account switch, key revocation, or missing-credential startup
/// recovery. Never stores API keys.
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

    /// Atomically switches active account (or to Guest when `account_id` is `None`) and purges
    /// `purge_account_id`'s cached rows when provided.
    fn set_active_account(
        &self,
        account_id: Option<&AccountId>,
        purge_account_id: Option<&AccountId>,
    ) -> Result<(), StorageError>;

    fn update_verification(
        &self,
        account_id: &AccountId,
        verification: UsernameVerification,
        updated_at_unix: i64,
    ) -> Result<(), StorageError>;

    /// Purges the account's cache and deletes the account row.
    fn remove_account(&self, account_id: &AccountId) -> Result<(), StorageError>;

    fn load_account_cache(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<AccountCachedState>, StorageError>;

    fn save_account_cache(
        &self,
        account_id: &AccountId,
        cache: &AccountCachedState,
    ) -> Result<(), StorageError>;

    fn purge_account_cache(&self, account_id: &AccountId) -> Result<(), StorageError>;

    fn load_local_preferences(&self) -> Result<LocalPreferences, StorageError>;

    fn save_local_preferences(&self, prefs: &LocalPreferences) -> Result<(), StorageError>;
}

/// SQLite-backed implementation of [`SessionStorage`].
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|err| StorageError::Open(err.to_string()))?;
        }
        let mut conn = Connection::open(path).map_err(|err| StorageError::Open(err.to_string()))?;
        configure_connection(&mut conn, true)?;
        migrations::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        let mut conn =
            Connection::open_in_memory().map_err(|err| StorageError::Open(err.to_string()))?;
        configure_connection(&mut conn, false)?;
        migrations::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
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
        let _: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(|err| StorageError::Open(err.to_string()))?;
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
            tx.execute(
                "INSERT INTO account_cache (account_id, user_settings_json, own_collections_json, updated_at_unix)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(account_id) DO UPDATE SET
                     user_settings_json = excluded.user_settings_json,
                     own_collections_json = excluded.own_collections_json,
                     updated_at_unix = excluded.updated_at_unix",
                params![
                    account.id.as_str(),
                    settings_json,
                    collections_json,
                    cache.updated_at_unix,
                ],
            )
            .map_err(|err| StorageError::Query(err.to_string()))?;
        }

        tx.commit()
            .map_err(|err| StorageError::Query(err.to_string()))?;
        Ok(())
    }

    fn set_active_account(
        &self,
        account_id: Option<&AccountId>,
        purge_account_id: Option<&AccountId>,
    ) -> Result<(), StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction()
            .map_err(|err| StorageError::Query(err.to_string()))?;

        if let Some(purge_id) = purge_account_id {
            tx.execute(
                "DELETE FROM account_cache WHERE account_id = ?1",
                params![purge_id.as_str()],
            )
            .map_err(|err| StorageError::Query(err.to_string()))?;
        }

        tx.execute("UPDATE accounts SET is_active = 0 WHERE is_active = 1", [])
            .map_err(|err| StorageError::Query(err.to_string()))?;

        if let Some(target) = account_id {
            let updated = tx
                .execute(
                    "UPDATE accounts SET is_active = 1 WHERE account_id = ?1",
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

    fn update_verification(
        &self,
        account_id: &AccountId,
        verification: UsernameVerification,
        updated_at_unix: i64,
    ) -> Result<(), StorageError> {
        let conn = self.lock_conn()?;
        conn.execute(
            "UPDATE accounts SET verification_state = ?1, updated_at_unix = ?2 WHERE account_id = ?3",
            params![verification.as_db_str(), updated_at_unix, account_id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        Ok(())
    }

    fn remove_account(&self, account_id: &AccountId) -> Result<(), StorageError> {
        let mut conn = self.lock_conn()?;
        let tx = conn
            .transaction()
            .map_err(|err| StorageError::Query(err.to_string()))?;
        tx.execute(
            "DELETE FROM account_cache WHERE account_id = ?1",
            params![account_id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        tx.execute(
            "DELETE FROM accounts WHERE account_id = ?1",
            params![account_id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        tx.commit()
            .map_err(|err| StorageError::Query(err.to_string()))?;
        Ok(())
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
                 WHERE account_id = ?1",
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

    fn save_account_cache(
        &self,
        account_id: &AccountId,
        cache: &AccountCachedState,
    ) -> Result<(), StorageError> {
        let conn = self.lock_conn()?;
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

    fn purge_account_cache(&self, account_id: &AccountId) -> Result<(), StorageError> {
        let conn = self.lock_conn()?;
        conn.execute(
            "DELETE FROM account_cache WHERE account_id = ?1",
            params![account_id.as_str()],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        Ok(())
    }

    fn load_local_preferences(&self) -> Result<LocalPreferences, StorageError> {
        let conn = self.lock_conn()?;
        let row: Option<(String, Option<String>)> = conn
            .query_row(
                "SELECT proxy_mode, proxy_url FROM local_preferences WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|err| StorageError::Query(err.to_string()))?;

        let Some((mode, url)) = row else {
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
        Ok(LocalPreferences { proxy })
    }

    fn save_local_preferences(&self, prefs: &LocalPreferences) -> Result<(), StorageError> {
        let normalized = prefs
            .proxy
            .normalized()
            .map_err(|err| StorageError::Serialization(err.to_string()))?;
        let (mode, url) = match &normalized {
            ProxyConfig::System => ("system", None),
            ProxyConfig::Direct => ("direct", None),
            ProxyConfig::Manual(raw) => ("manual", Some(raw.as_str())),
        };
        let conn = self.lock_conn()?;
        conn.execute(
            "INSERT INTO local_preferences (id, proxy_mode, proxy_url)
             VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET
                 proxy_mode = excluded.proxy_mode,
                 proxy_url = excluded.proxy_url",
            params![mode, url],
        )
        .map_err(|err| StorageError::Query(err.to_string()))?;
        Ok(())
    }
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
        store.remove_account(&acct2.id).expect("remove acct2");
        assert!(store.active_account().unwrap().is_none());
        assert_eq!(store.load_local_preferences().unwrap(), prefs);
    }
}
