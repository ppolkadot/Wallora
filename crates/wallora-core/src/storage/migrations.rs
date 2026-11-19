use rusqlite::Connection;

use crate::storage::db::StorageError;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

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

    if version < 1 {
        let tx = conn
            .transaction()
            .map_err(|err| StorageError::Migrate(err.to_string()))?;
        tx.execute_batch(MIGRATION_V1)
            .map_err(|err| StorageError::Migrate(err.to_string()))?;
        tx.pragma_update(None, "user_version", 1u32)
            .map_err(|err| StorageError::Migrate(err.to_string()))?;
        tx.commit()
            .map_err(|err| StorageError::Migrate(err.to_string()))?;
    }

    Ok(())
}
