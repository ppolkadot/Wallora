use rusqlite::Connection;

use crate::storage::db::StorageError;

pub const CURRENT_SCHEMA_VERSION: u32 = 2;

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
        tx.execute_batch(
            "CREATE TABLE pending_credential_cleanup (account_id TEXT PRIMARY KEY NOT NULL);
             DELETE FROM account_cache
             WHERE account_id IN (SELECT account_id FROM accounts WHERE is_active = 0);",
        )
        .map_err(|err| StorageError::Migrate(err.to_string()))?;
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
}
