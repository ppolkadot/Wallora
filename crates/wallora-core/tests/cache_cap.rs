//! Persisted image cache cap preference: validation, SQLite persistence, live application to the
//! cache actor, and startup wiring of `SessionManager::restore_with_cache`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use wallora_core::domain::{
    CollectionSummary, CredentialStore, DEFAULT_CACHE_CAP_BYTES, HARD_MAX_CACHE_CAP_BYTES,
    InMemoryCredentialStore, ProxyConfig, SecretString, SessionScope, UsernameVerification,
};
use wallora_core::provider::{ImageClass, WallhavenClient};
use wallora_core::services::cache::{CacheConfig, CacheError, CacheKey, CacheLookup, CacheStore};
use wallora_core::services::{
    CacheCapUpdateError, SessionManager, SignInCommitOutcome, SignInProbe,
};
use wallora_core::storage::{SessionStorage, SqliteStore};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("wallora-cap-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open_storage(path: &Path) -> Arc<dyn SessionStorage> {
    Arc::new(SqliteStore::open(path).expect("open sqlite"))
}

fn thumb_key(id: &str) -> CacheKey {
    CacheKey::new(ImageClass::Thumbnail, id).expect("valid key")
}

fn probe(start_scope: SessionScope, username: &str, key: &str) -> SignInProbe {
    SignInProbe {
        start_scope,
        username: username.to_string(),
        api_key: SecretString::new(key),
        user_settings: None,
        own_collections: Some(Vec::<CollectionSummary>::new()),
        verification: UsernameVerification::UnverifiableEmpty,
    }
}

async fn open_cache(root: PathBuf, cap: u64) -> CacheStore {
    CacheStore::open(
        CacheConfig::new(root, SessionScope::guest(0))
            .with_cap_bytes(cap)
            .expect("valid cap"),
    )
    .await
    .expect("open cache")
}

#[tokio::test]
async fn cap_validates_persists_and_applies_to_the_live_cache() {
    let dir = TempDir::new();
    let db_path = dir.path().join("wallora.db");
    let creds: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());
    let store = open_cache(dir.path().join("cache"), DEFAULT_CACHE_CAP_BYTES).await;

    {
        let mut session = SessionManager::restore_with_cache(
            open_storage(&db_path),
            Arc::clone(&creds),
            Arc::new(store.clone()),
        );
        assert_eq!(
            session.local_preferences().cache_cap_bytes,
            DEFAULT_CACHE_CAP_BYTES
        );

        // A proxy preference saved first must survive cap changes, and vice versa.
        let client = WallhavenClient::new(Arc::clone(&creds)).unwrap();
        session.update_proxy(&client, ProxyConfig::Direct).unwrap();

        assert_eq!(
            session.apply_cache_cap(&store, 1_500_000_000).await,
            Ok(1_500_000_000)
        );
        assert_eq!(store.stats().await.unwrap().cap_bytes, 1_500_000_000);
        assert_eq!(session.local_preferences().proxy, ProxyConfig::Direct);

        // Rejected values change neither the live actor nor the persisted preference.
        for invalid in [0, HARD_MAX_CACHE_CAP_BYTES + 1, u64::MAX] {
            let err = session.apply_cache_cap(&store, invalid).await.unwrap_err();
            assert!(
                matches!(err, CacheCapUpdateError::Invalid(_)),
                "{invalid}: {err:?}"
            );
            assert_eq!(store.stats().await.unwrap().cap_bytes, 1_500_000_000);
            assert_eq!(session.local_preferences().cache_cap_bytes, 1_500_000_000);
        }

        session.update_proxy(&client, ProxyConfig::System).unwrap();
        assert_eq!(session.local_preferences().cache_cap_bytes, 1_500_000_000);
    }

    // The preference survives a restart and still carries the proxy preference.
    let restarted = SessionManager::restore(open_storage(&db_path), Arc::clone(&creds));
    assert_eq!(restarted.local_preferences().cache_cap_bytes, 1_500_000_000);
    assert_eq!(restarted.local_preferences().proxy, ProxyConfig::System);
    store.shutdown().await;
}

#[tokio::test]
async fn hard_max_is_accepted_and_one_byte_more_is_rejected_everywhere() {
    let dir = TempDir::new();
    let store = open_cache(dir.path().join("cache"), DEFAULT_CACHE_CAP_BYTES).await;
    let mut session = SessionManager::restore_with_cache(
        open_storage(&dir.path().join("wallora.db")),
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(store.clone()),
    );

    assert_eq!(
        session
            .apply_cache_cap(&store, HARD_MAX_CACHE_CAP_BYTES)
            .await,
        Ok(HARD_MAX_CACHE_CAP_BYTES)
    );
    assert_eq!(
        store.set_cap(HARD_MAX_CACHE_CAP_BYTES + 1).await,
        Err(CacheError::CapExceedsHardMax {
            requested: HARD_MAX_CACHE_CAP_BYTES + 1,
            hard_max: HARD_MAX_CACHE_CAP_BYTES,
        })
    );
    assert_eq!(
        SessionManager::validate_cache_cap_request(HARD_MAX_CACHE_CAP_BYTES + 1)
            .unwrap_err()
            .to_string(),
        "cache cap 10000000001 bytes exceeds the 10000000000 byte hard maximum"
    );
    assert_eq!(
        store.stats().await.unwrap().cap_bytes,
        HARD_MAX_CACHE_CAP_BYTES
    );
    store.shutdown().await;
}

#[tokio::test]
async fn live_rejection_while_a_reservation_is_charged_keeps_the_persisted_cap() {
    let dir = TempDir::new();
    let db_path = dir.path().join("wallora.db");
    let store = open_cache(dir.path().join("cache"), 1000).await;
    // The cache opened at guest(0), and the session starts at guest(1); use the actor's scope.
    store
        .transition_scope(wallora_core::services::cache::CacheScopeTransition {
            new_scope: SessionScope::guest(1),
            purge_account: None,
        })
        .await
        .unwrap();
    let mut session = SessionManager::restore(
        open_storage(&db_path),
        Arc::new(InMemoryCredentialStore::new()),
    );
    session.commit_cache_cap(1000).unwrap();

    let (tx, rx) = mpsc::channel(2);
    let writer_store = store.clone();
    let handle = tokio::spawn(async move {
        writer_store
            .put(
                thumb_key("reserved-700"),
                SessionScope::guest(1),
                Some(700),
                rx,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while store.stats().await.unwrap().live_reservations == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("reservation admitted");

    let err = session.apply_cache_cap(&store, 500).await.unwrap_err();
    assert_eq!(
        err,
        CacheCapUpdateError::Rejected(CacheError::CapReductionRejected {
            requested: 500,
            charged: 700,
        })
    );
    assert_eq!(session.local_preferences().cache_cap_bytes, 1000);
    assert_eq!(store.stats().await.unwrap().cap_bytes, 1000);
    drop(session);
    let reloaded = SessionManager::restore(
        open_storage(&db_path),
        Arc::new(InMemoryCredentialStore::new()),
    );
    assert_eq!(reloaded.local_preferences().cache_cap_bytes, 1000);

    tx.send(Ok(vec![0x55; 700])).await.unwrap();
    drop(tx);
    handle.await.unwrap().unwrap();
    store.shutdown().await;
}

#[tokio::test]
async fn failed_preference_write_rolls_the_live_cap_back() {
    let dir = TempDir::new();
    let db_path = dir.path().join("wallora.db");
    let store = open_cache(dir.path().join("cache"), 4_000_000).await;
    let mut session = SessionManager::restore(
        open_storage(&db_path),
        Arc::new(InMemoryCredentialStore::new()),
    );
    session.commit_cache_cap(4_000_000).unwrap();

    let raw = rusqlite::Connection::open(&db_path).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER block_cap BEFORE UPDATE ON local_preferences
         BEGIN SELECT RAISE(ABORT, 'synthetic cap write failure'); END;",
    )
    .unwrap();

    let err = session
        .apply_cache_cap(&store, 1_000_000)
        .await
        .unwrap_err();
    assert_eq!(err, CacheCapUpdateError::Storage);
    assert_eq!(session.local_preferences().cache_cap_bytes, 4_000_000);
    assert_eq!(store.stats().await.unwrap().cap_bytes, 4_000_000);
    store.shutdown().await;
}

#[tokio::test]
async fn restored_account_session_reaches_the_cache_when_it_opens_before_restore() {
    let dir = TempDir::new();
    let db_path = dir.path().join("wallora.db");
    let creds: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());

    let account = {
        let mut first = SessionManager::restore(open_storage(&db_path), Arc::clone(&creds));
        match first
            .commit_sign_in(probe(first.scope(), "dave", "wh_restart_dave_1"))
            .unwrap()
        {
            SignInCommitOutcome::Applied { scope, .. } => scope.account_id().unwrap().clone(),
            other => panic!("unexpected outcome: {other:?}"),
        }
    };

    // The app opens the cache before restore, so it must not start at generation 1: a restored
    // account session is also generation 1 and the actor ignores non-increasing transitions.
    let store = open_cache(dir.path().join("cache"), DEFAULT_CACHE_CAP_BYTES).await;
    let restored = SessionManager::restore_with_cache(
        open_storage(&db_path),
        Arc::clone(&creds),
        Arc::new(store.clone()),
    );
    assert_eq!(restored.scope(), SessionScope::account(account, 1));
    assert_eq!(store.stats().await.unwrap().current_scope, restored.scope());

    let key = thumb_key("restored-thumb");
    store
        .put(key.clone(), restored.scope(), Some(16), vec![0x42; 16])
        .await
        .expect("restored scope may write");
    assert!(matches!(
        store.get(key, restored.scope()).await.unwrap(),
        CacheLookup::Hit { .. }
    ));
    store.shutdown().await;
}

#[tokio::test]
async fn restored_guest_session_reaches_the_cache_when_it_opens_before_restore() {
    let dir = TempDir::new();
    let store = open_cache(dir.path().join("cache"), DEFAULT_CACHE_CAP_BYTES).await;
    let restored = SessionManager::restore_with_cache(
        open_storage(&dir.path().join("wallora.db")),
        Arc::new(InMemoryCredentialStore::new()),
        Arc::new(store.clone()),
    );
    assert_eq!(restored.scope(), SessionScope::guest(1));
    assert_eq!(store.stats().await.unwrap().current_scope, restored.scope());
    store.shutdown().await;
}
