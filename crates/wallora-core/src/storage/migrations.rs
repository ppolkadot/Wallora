use rusqlite::Connection;

use crate::storage::db::StorageError;

pub const CURRENT_SCHEMA_VERSION: u32 = 3;

const MIGRATION_V1: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    account_id TEXT PRIMARY KEY NOT NULL,
    username TEXT NOT NULL,
    verification_state TEXT NOT NULL,
    is_active INTEGER NOT NULL DEFAULT 0 CHECK (is_active IN (0, 1)),
    created_at_unix INTEGER NOT NULL,
    updated_at_unix INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS account_cache (
    account_id TEXT PRIMARY KEY NOT NULL
        REFERENCES accounts(account_id) ON DELETE CASCADE,
    user_settings_json TEXT,
    own_collections_json TEXT,
    updated_at_unix INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS local_preferences (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    proxy_mode TEXT NOT NULL,
    proxy_url TEXT
);
"#;

const MIGRATION_V2: &str = r#"
CREATE TABLE pending_credential_cleanup (account_id TEXT PRIMARY KEY NOT NULL);
DELETE FROM account_cache
WHERE account_id IN (SELECT account_id FROM accounts WHERE is_active = 0);
"#;

// Nullable so existing v1/v2 rows keep their proxy preference and fall back to the default cap.
const MIGRATION_V3: &str = "ALTER TABLE local_preferences ADD COLUMN cache_cap_bytes INTEGER;";

pub fn migrate(conn: &mut Connection) -> Result<(), StorageError> {
    let version: u32 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|err| StorageError::Migrate(err.to_string()))?;

    if version > CURRENT_SCHEMA_VERSION {
        return Err(StorageError::Migrate(format!(
            "database schema version {version} is newer than supported version {CURRENT_SCHEMA_VERSION}"
        )));
    }

    if version < CURRENT_SCHEMA_VERSION {
        let tx = conn
            .transaction()
            .map_err(|err| StorageError::Migrate(err.to_string()))?;
        if version < 1 {
            tx.execute_batch(MIGRATION_V1)
                .map_err(|err| StorageError::Migrate(err.to_string()))?;
        }
        if version < 2 {
            tx.execute_batch(MIGRATION_V2)
                .map_err(|err| StorageError::Migrate(err.to_string()))?;
        }
        if version < 3 {
            tx.execute_batch(MIGRATION_V3)
                .map_err(|err| StorageError::Migrate(err.to_string()))?;
        }
        tx.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)
            .map_err(|err| StorageError::Migrate(err.to_string()))?;
        tx.commit()
            .map_err(|err| StorageError::Migrate(err.to_string()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_migration_preserves_account_metadata() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 1;
             INSERT INTO accounts VALUES ('opaque-id', 'alice', 'verified', 1, 100, 100);
             INSERT INTO accounts VALUES ('inactive-id', 'bob', 'verified', 0, 100, 100);
             INSERT INTO account_cache VALUES ('opaque-id', NULL, NULL, 100);
             INSERT INTO account_cache VALUES ('inactive-id', NULL, NULL, 100);",
        )
        .unwrap();

        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();

        let user_version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let active_username: String = conn
            .query_row(
                "SELECT username FROM accounts WHERE is_active = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let pending: u32 = conn
            .query_row(
                "SELECT count(*) FROM pending_credential_cleanup",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(user_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(active_username, "alice");
        assert_eq!(pending, 0);
        let cached_id: String = conn
            .query_row("SELECT account_id FROM account_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cached_id, "opaque-id");
        let cache_count: u32 = conn
            .query_row("SELECT count(*) FROM account_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cache_count, 1);
    }
    fn user_version(conn: &Connection) -> u32 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn build_v2_database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute_batch("PRAGMA user_version = 2;").unwrap();
        conn
    }

    #[test]
    fn v2_to_v3_preserves_accounts_pending_cleanup_and_proxy_preferences() {
        let mut conn = build_v2_database();
        conn.execute_batch(
            "INSERT INTO accounts VALUES ('opaque-id', 'alice', 'verified', 1, 100, 100);
             INSERT INTO account_cache VALUES ('opaque-id', '{}', NULL, 100);
             INSERT INTO pending_credential_cleanup VALUES ('retired-id');
             INSERT INTO local_preferences (id, proxy_mode, proxy_url)
                 VALUES (1, 'manual', 'http://127.0.0.1:8888/');",
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        assert_eq!(user_version(&conn), CURRENT_SCHEMA_VERSION);
        let (mode, url, cap): (String, Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT proxy_mode, proxy_url, cache_cap_bytes FROM local_preferences WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(mode, "manual");
        assert_eq!(url.as_deref(), Some("http://127.0.0.1:8888/"));
        assert_eq!(cap, None, "existing rows keep the default cap");
        let username: String = conn
            .query_row("SELECT username FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(username, "alice");
        let cached: u32 = conn
            .query_row("SELECT count(*) FROM account_cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cached, 1, "the v2 purge of inactive caches must not re-run");
        let pending: String = conn
            .query_row(
                "SELECT account_id FROM pending_credential_cleanup",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending, "retired-id");
    }

    #[test]
    fn v3_migration_is_idempotent_and_runs_from_every_older_version() {
        let mut v2 = build_v2_database();
        migrate(&mut v2).unwrap();
        migrate(&mut v2).unwrap();
        assert_eq!(user_version(&v2), CURRENT_SCHEMA_VERSION);

        let mut fresh = Connection::open_in_memory().unwrap();
        migrate(&mut fresh).unwrap();
        assert_eq!(user_version(&fresh), CURRENT_SCHEMA_VERSION);
        fresh
            .execute(
                "INSERT INTO local_preferences (id, proxy_mode, cache_cap_bytes)
                 VALUES (1, 'system', 5000000000)",
                [],
            )
            .unwrap();
    }

    #[test]
    fn newer_schema_versions_are_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "PRAGMA user_version = {};",
            CURRENT_SCHEMA_VERSION + 1
        ))
        .unwrap();
        assert!(migrate(&mut conn).is_err());
    }
}
