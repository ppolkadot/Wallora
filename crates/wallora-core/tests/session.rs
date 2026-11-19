use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tracing_subscriber::fmt::MakeWriter;
use url::Url;
use wallora_core::domain::{
    AccountId, AccountRecord, AspectRatio, BrowseDefaults, CategoryMask, CredentialError,
    CredentialStore, Dimensions, InMemoryCredentialStore, LocalPreferences, ProxyConfig,
    PurityMask, SearchQuery, SecretString, SessionKind, SessionScope, TopRange,
    UsernameVerification, WallpaperId,
};
use wallora_core::provider::{Provider, ProviderError, WallhavenClient, WallhavenClientConfig};
use wallora_core::services::{
    AccountRefreshPayload, NoticeLevel, RefreshOutcome, ScopedResponse, SessionManager,
    SignInCommitOutcome, SignInError, fetch_account_refresh, probe_sign_in,
};
use wallora_core::storage::{AccountCachedState, SessionStorage, SqliteStore, StorageError};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[allow(dead_code)]
mod support;

const FIXTURE_SETTINGS: &str = include_str!("fixtures/settings.json");
const FIXTURE_COLLECTIONS_PUBLIC: &str = include_str!("fixtures/collections.json");
const FIXTURE_ERROR_401: &str = include_str!("fixtures/error_unauthorized.json");
const FIXTURE_SEARCH_PAGE: &str = include_str!("fixtures/search_page.json");

const SYNTHETIC_KEY_A: &str = "wh_synthetic_task3_key_alpha_998877";
const SYNTHETIC_KEY_B: &str = "wh_synthetic_task3_key_beta_665544";

fn test_client(server: &MockServer, creds: Arc<dyn CredentialStore>) -> WallhavenClient {
    let api_base = Url::parse(&format!("{}/api/v1", server.uri())).expect("valid mock api base");
    let config = WallhavenClientConfig::for_test(api_base).expect("valid test config");
    WallhavenClient::with_config(creds, config).expect("client build")
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn contents(&self) -> String {
        let guard = self.0.lock().expect("log lock");
        String::from_utf8_lossy(&guard).into_owned()
    }
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.lock().expect("log lock");
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(Arc::clone(&self.0))
    }
}

/// Wrapper around `InMemoryCredentialStore` that can inject failures per operation.
struct FaultyCredentialStore {
    inner: InMemoryCredentialStore,
    fail_load: AtomicBool,
    fail_save: AtomicBool,
    fail_delete: AtomicBool,
}

impl FaultyCredentialStore {
    fn new() -> Self {
        Self {
            inner: InMemoryCredentialStore::new(),
            fail_load: AtomicBool::new(false),
            fail_save: AtomicBool::new(false),
            fail_delete: AtomicBool::new(false),
        }
    }
}

impl CredentialStore for FaultyCredentialStore {
    fn load(&self, account: &AccountId) -> Result<Option<SecretString>, CredentialError> {
        if self.fail_load.load(Ordering::SeqCst) {
            return Err(CredentialError::Unavailable(
                "simulated credential read error".to_string(),
            ));
        }
        self.inner.load(account)
    }

    fn save(&self, account: &AccountId, key: &SecretString) -> Result<(), CredentialError> {
        if self.fail_save.load(Ordering::SeqCst) {
            return Err(CredentialError::Io(
                "simulated credential write error".to_string(),
            ));
        }
        self.inner.save(account, key)
    }

    fn delete(&self, account: &AccountId) -> Result<(), CredentialError> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(CredentialError::Io(
                "simulated credential delete error".to_string(),
            ));
        }
        self.inner.delete(account)
    }
}

/// Wrapper around `SqliteStore` that can inject write failures on demand.
struct FaultyStorage {
    inner: SqliteStore,
    fail_save_account: AtomicBool,
    fail_save_prefs: AtomicBool,
    fail_remove_account: AtomicBool,
}

impl FaultyStorage {
    fn in_memory() -> Self {
        Self {
            inner: SqliteStore::open_in_memory().expect("in-memory sqlite"),
            fail_save_account: AtomicBool::new(false),
            fail_save_prefs: AtomicBool::new(false),
            fail_remove_account: AtomicBool::new(false),
        }
    }
}

impl SessionStorage for FaultyStorage {
    fn list_accounts(&self) -> Result<Vec<AccountRecord>, StorageError> {
        self.inner.list_accounts()
    }

    fn active_account(&self) -> Result<Option<AccountRecord>, StorageError> {
        self.inner.active_account()
    }

    fn save_and_activate_account(
        &self,
        account: &AccountRecord,
        initial_cache: Option<&AccountCachedState>,
        purge_outgoing_cache: bool,
    ) -> Result<(), StorageError> {
        if self.fail_save_account.load(Ordering::SeqCst) {
            return Err(StorageError::Query(
                "simulated SQLite write error".to_string(),
            ));
        }
        self.inner
            .save_and_activate_account(account, initial_cache, purge_outgoing_cache)
    }

    fn set_active_account(
        &self,
        account_id: Option<&AccountId>,
        purge_account_id: Option<&AccountId>,
    ) -> Result<(), StorageError> {
        self.inner.set_active_account(account_id, purge_account_id)
    }

    fn update_verification(
        &self,
        account_id: &AccountId,
        verification: UsernameVerification,
        updated_at_unix: i64,
    ) -> Result<(), StorageError> {
        self.inner
            .update_verification(account_id, verification, updated_at_unix)
    }

    fn remove_account(&self, account_id: &AccountId) -> Result<(), StorageError> {
        if self.fail_remove_account.load(Ordering::SeqCst) {
            return Err(StorageError::Query(
                "simulated SQLite remove error".to_string(),
            ));
        }
        self.inner.remove_account(account_id)
    }

    fn load_account_cache(
        &self,
        account_id: &AccountId,
    ) -> Result<Option<AccountCachedState>, StorageError> {
        self.inner.load_account_cache(account_id)
    }

    fn save_account_cache(
        &self,
        account_id: &AccountId,
        cache: &AccountCachedState,
    ) -> Result<(), StorageError> {
        self.inner.save_account_cache(account_id, cache)
    }

    fn purge_account_cache(&self, account_id: &AccountId) -> Result<(), StorageError> {
        self.inner.purge_account_cache(account_id)
    }

    fn load_local_preferences(&self) -> Result<LocalPreferences, StorageError> {
        self.inner.load_local_preferences()
    }

    fn save_local_preferences(&self, prefs: &LocalPreferences) -> Result<(), StorageError> {
        if self.fail_save_prefs.load(Ordering::SeqCst) {
            return Err(StorageError::Query(
                "simulated SQLite preference write error".to_string(),
            ));
        }
        self.inner.save_local_preferences(prefs)
    }
}

#[tokio::test]
async fn sign_in_verified_and_public_lookup_is_unkeyed() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":[
                {"id": 1, "label": "Default", "views": 0, "public": 1, "count": 4},
                {"id": 2, "label": "Hidden", "views": 0, "public": 0, "count": 2}
            ]}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    // Public identity request MUST remain unkeyed!
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/alice"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .expect(1)
        .mount(&server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let storage = Arc::new(SqliteStore::open_in_memory().expect("db"));
    let client = test_client(&server, creds.clone());
    let mut session = SessionManager::restore(storage.clone(), creds.clone());

    let probe = probe_sign_in(
        &client,
        "alice".to_string(),
        SecretString::new(SYNTHETIC_KEY_A),
        session.scope(),
    )
    .await
    .expect("valid key + matching public id must succeed");
    assert_eq!(probe.verification, UsernameVerification::Verified);

    let outcome = session.commit_sign_in(probe).expect("commit sign-in");
    let SignInCommitOutcome::Applied {
        scope,
        verification,
    } = outcome
    else {
        panic!("expected Applied");
    };
    assert_eq!(verification, UsernameVerification::Verified);
    assert_eq!(scope.generation, 2);
    let account_id = scope.account_id().expect("account scope");
    assert_eq!(
        creds.load(account_id).unwrap().unwrap().expose_secret(),
        SYNTHETIC_KEY_A
    );
    let stored = storage.active_account().unwrap().expect("stored account");
    assert_eq!(stored.username, "alice");
    assert_eq!(stored.verification, UsernameVerification::Verified);
    assert!(stored.verification.can_access_own_collection_items());
    assert_eq!(session.own_collections().unwrap().len(), 2);
}

#[tokio::test]
async fn sign_in_mismatch_refused_and_leaves_stores_untouched() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .expect(1)
        .mount(&server)
        .await;

    // Public list belongs to another user (id 999 not in own list [1]).
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/wronguser"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":[{"id": 999, "label": "Other", "views": 5, "public": 1, "count": 3}]}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let storage = Arc::new(SqliteStore::open_in_memory().expect("db"));
    let client = test_client(&server, creds.clone());
    let session = SessionManager::restore(storage.clone(), creds);

    let err = probe_sign_in(
        &client,
        "wronguser".to_string(),
        SecretString::new(SYNTHETIC_KEY_A),
        session.scope(),
    )
    .await
    .expect_err("mismatched username must refuse sign-in");

    assert_eq!(err, SignInError::UsernameMismatch);
    assert_eq!(err.to_string(), "username does not match this API key");
    assert!(storage.list_accounts().unwrap().is_empty());
    assert_eq!(session.scope().kind, SessionKind::Guest);
}

#[tokio::test]
async fn sign_in_unverifiable_private_accepted_with_own_list_available() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":[{"id": 42, "label": "PrivateOnly", "views": 0, "public": 0, "count": 7}]}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/collections/privateuser"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .expect(1)
        .mount(&server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let storage = Arc::new(SqliteStore::open_in_memory().expect("db"));
    let client = test_client(&server, creds.clone());
    let mut session = SessionManager::restore(storage, creds);

    let probe = probe_sign_in(
        &client,
        "privateuser".to_string(),
        SecretString::new(SYNTHETIC_KEY_A),
        session.scope(),
    )
    .await
    .expect("private-only account must be accepted");

    assert_eq!(
        probe.verification,
        UsernameVerification::UnverifiablePrivate
    );
    let _ = session.commit_sign_in(probe).expect("commit");
    assert!(
        !session
            .active_account()
            .unwrap()
            .verification
            .can_access_own_collection_items()
    );
    let own = session.own_collections().expect("own list stays available");
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].label, "PrivateOnly");
}

#[tokio::test]
async fn sign_in_unverifiable_empty_accepted() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/collections/emptyuser"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
        .expect(1)
        .mount(&server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let client = test_client(&server, creds);
    let probe = probe_sign_in(
        &client,
        "emptyuser".to_string(),
        SecretString::new(SYNTHETIC_KEY_A),
        SessionScope::guest(1),
    )
    .await
    .expect("empty collections must be accepted as unverifiable-empty");

    assert_eq!(probe.verification, UsernameVerification::UnverifiableEmpty);
    assert!(probe.own_collections.unwrap().is_empty());
}

#[tokio::test]
async fn sign_in_inconclusive_on_ambiguous_404_with_private_collection_and_other_ambiguities() {
    // Subcase 1: Matches live 2026-11-19 follow-up observation: /settings 200, /collections 200
    // with one private collection, /collections/{username} 404 -> Inconclusive (NOT Mismatch!).
    {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/settings"))
            .and(header("x-api-key", SYNTHETIC_KEY_A))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .and(header("x-api-key", SYNTHETIC_KEY_A))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"data":[{"id": 77, "label": "Default", "views": 0, "public": 0, "count": 5}]}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/private404"))
            .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
            .respond_with(ResponseTemplate::new(404).set_body_string(r#"{"error":"Not found"}"#))
            .expect(1)
            .mount(&server)
            .await;

        let client = test_client(&server, Arc::new(InMemoryCredentialStore::new()));
        let probe = probe_sign_in(
            &client,
            "private404".to_string(),
            SecretString::new(SYNTHETIC_KEY_A),
            SessionScope::guest(1),
        )
        .await
        .expect("ambiguous 404 must be inconclusive, not mismatch");
        assert_eq!(probe.verification, UsernameVerification::Inconclusive);
        assert_eq!(probe.own_collections.as_ref().unwrap().len(), 1);
    }

    // Subcase 2: Public list is empty despite public entries in the own list -> Inconclusive.
    {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/settings"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/empty_pub"))
            .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[]}"#))
            .mount(&server)
            .await;

        let client = test_client(&server, Arc::new(InMemoryCredentialStore::new()));
        let probe = probe_sign_in(
            &client,
            "empty_pub".to_string(),
            SecretString::new(SYNTHETIC_KEY_A),
            SessionScope::guest(1),
        )
        .await
        .expect("empty public despite public own must be inconclusive");
        assert_eq!(probe.verification, UsernameVerification::Inconclusive);
    }

    // Subcase 3: Malformed JSON in public list -> Inconclusive.
    {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/settings"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/collections/malformed"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data": "not-an-array"}"#))
            .mount(&server)
            .await;

        let client = test_client(&server, Arc::new(InMemoryCredentialStore::new()));
        let probe = probe_sign_in(
            &client,
            "malformed".to_string(),
            SecretString::new(SYNTHETIC_KEY_A),
            SessionScope::guest(1),
        )
        .await
        .expect("malformed public list must be inconclusive");
        assert_eq!(probe.verification, UsernameVerification::Inconclusive);
    }
}

#[tokio::test]
async fn sign_in_invalid_key_attempts_no_identity_check() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .respond_with(ResponseTemplate::new(401).set_body_string(FIXTURE_ERROR_401))
        .expect(1)
        .mount(&server)
        .await;

    // Neither keyed /collections nor unkeyed /collections/{username} may be called!
    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .expect(0)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/alice"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .expect(0)
        .mount(&server)
        .await;

    let client = test_client(&server, Arc::new(InMemoryCredentialStore::new()));
    let err = probe_sign_in(
        &client,
        "alice".to_string(),
        SecretString::new("wh_invalid_key_0000"),
        SessionScope::guest(1),
    )
    .await
    .expect_err("401 on /settings must refuse sign-in");

    assert_eq!(err, SignInError::InvalidApiKey);
    assert!(!err.to_string().contains("wh_invalid_key_0000"));
}

#[tokio::test]
async fn mid_session_key_revocation_bumps_generation_purges_caches_and_notifies_user() {
    let server = MockServer::start().await;

    // Initial sign-in mocks.
    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/alice"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let storage = Arc::new(SqliteStore::open_in_memory().expect("db"));
    let client = test_client(&server, creds.clone());
    let mut session = SessionManager::restore(storage.clone(), creds.clone());

    let probe = probe_sign_in(
        &client,
        "alice".to_string(),
        SecretString::new(SYNTHETIC_KEY_A),
        session.scope(),
    )
    .await
    .expect("sign in");
    let _ = session.commit_sign_in(probe).expect("commit");
    let active_scope = session.scope();
    let account_id = active_scope.account_id().cloned().expect("active account");
    assert!(storage.load_account_cache(&account_id).unwrap().is_some());

    // Now upstream revokes the key: /w/nsfw123 returns 401.
    Mock::given(method("GET"))
        .and(path("/api/v1/w/nsfw123"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(401).set_body_string(FIXTURE_ERROR_401))
        .expect(1)
        .mount(&server)
        .await;

    let res = client
        .wallpaper(WallpaperId::new("nsfw123"), active_scope.clone())
        .await;
    let handled = session.handle_provider_result(&active_scope, res);

    let ScopedResponse::Revoked(notice) = handled else {
        panic!("expected ScopedResponse::Revoked, got {handled:?}");
    };
    assert_eq!(notice.level, NoticeLevel::Error);
    assert!(notice.message.contains("401"));
    assert!(!notice.message.contains(SYNTHETIC_KEY_A));

    assert_eq!(session.scope().kind, SessionKind::Guest);
    assert_eq!(session.scope().generation, active_scope.generation + 1);
    assert!(session.active_account().is_none());
    assert!(session.user_settings().is_none());
    assert!(session.own_collections().is_none());
    assert!(storage.load_account_cache(&account_id).unwrap().is_none());
    assert!(storage.active_account().unwrap().is_none());
    assert!(creds.load(&account_id).unwrap().is_none());
    assert!(session.purged_accounts().contains(&account_id));
}

#[tokio::test]
async fn stale_async_completions_and_stale_401_are_discarded() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/alice"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/bob"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let storage = Arc::new(SqliteStore::open_in_memory().expect("db"));
    let client = test_client(&server, creds.clone());
    let mut session = SessionManager::restore(storage.clone(), creds.clone());

    let guest_scope = session.scope();
    let probe_a = probe_sign_in(
        &client,
        "alice".to_string(),
        SecretString::new(SYNTHETIC_KEY_A),
        guest_scope.clone(),
    )
    .await
    .expect("probe alice");
    let probe_stale = probe_sign_in(
        &client,
        "bob".to_string(),
        SecretString::new(SYNTHETIC_KEY_B),
        guest_scope,
    )
    .await
    .expect("probe bob");

    // Commit Alice first (bumps generation 1 -> 2).
    let _ = session.commit_sign_in(probe_a).expect("commit alice");
    let alice_scope = session.scope();

    // Committing Bob's probe that started at generation 1 MUST be discarded with zero side effects!
    let stale_outcome = session
        .commit_sign_in(probe_stale)
        .expect("stale commit returns Ok(DiscardedStale)");
    assert_eq!(stale_outcome, SignInCommitOutcome::DiscardedStale);
    assert_eq!(session.scope(), alice_scope);
    assert_eq!(storage.list_accounts().unwrap().len(), 1);
    assert_eq!(session.active_account().unwrap().username, "alice");

    // A stale 401 refresh tagged with an older generation must NOT revoke Alice.
    let stale_refresh = session.apply_refresh(AccountRefreshPayload {
        request_scope: SessionScope::account(
            alice_scope.account_id().unwrap().clone(),
            alice_scope.generation - 1,
        ),
        user_settings: Err(ProviderError::Unauthorized),
        own_collections: Err(ProviderError::Unauthorized),
        identity_outcome: None,
    });
    assert_eq!(stale_refresh, RefreshOutcome::DiscardedStale);
    assert_eq!(session.scope(), alice_scope);

    // A stale 401 provider result from another account/generation must NOT revoke Alice.
    let stale_resp: ScopedResponse<()> = session.handle_provider_result(
        &SessionScope::account(AccountId::new("other-account"), alice_scope.generation),
        Err(ProviderError::Unauthorized),
    );
    assert_eq!(stale_resp, ScopedResponse::Stale);
    assert_eq!(session.scope(), alice_scope);
}

#[tokio::test]
async fn logout_and_account_switch_purge_caches_bump_generation_and_keep_local_preferences() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .and(header("x-api-key", SYNTHETIC_KEY_B))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{
                "thumb_size":"large",
                "per_page":"64",
                "purity":["sfw","sketchy","nsfw"],
                "categories":["anime"],
                "resolutions":["3840x2160"],
                "aspect_ratios":["21x9"],
                "toplist_range":"1y",
                "tag_blacklist":[],
                "user_blacklist":[]
            }}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/alice"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/bob"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let storage = Arc::new(SqliteStore::open_in_memory().expect("db"));
    let client = test_client(&server, creds.clone());
    let mut session = SessionManager::restore(storage.clone(), creds.clone());

    // Save a local proxy preference (must stay separate from upstream settings and survive logout/switch).
    let local_prefs = LocalPreferences {
        proxy: ProxyConfig::Direct,
    };
    session
        .update_proxy(&client, local_prefs.proxy.clone())
        .expect("set direct proxy");

    // Sign in as Alice (defaults: per_page=24, purity=sfw+sketchy, res=1920x1080, toplist=1M).
    let probe_a = probe_sign_in(
        &client,
        "alice".to_string(),
        SecretString::new(SYNTHETIC_KEY_A),
        session.scope(),
    )
    .await
    .expect("alice probe");
    let _ = session.commit_sign_in(probe_a).expect("commit alice");
    let alice_scope = session.scope();
    let alice_id = alice_scope.account_id().cloned().unwrap();
    let alice_defaults = session.browse_defaults();
    assert_eq!(alice_defaults.per_page, 24);
    assert_eq!(alice_defaults.purity, PurityMask::SFW_AND_SKETCHY);
    assert_eq!(alice_defaults.categories, CategoryMask::ALL);
    assert_eq!(
        alice_defaults.resolutions,
        vec![Dimensions::new(1920, 1080)]
    );
    assert_eq!(alice_defaults.aspect_ratios, vec![AspectRatio::new("16x9")]);
    assert_eq!(alice_defaults.toplist_range, TopRange::OneMonth);

    // Sign in as Bob while Alice is active -> purges Alice's SQLite cache and bumps generation.
    let probe_b = probe_sign_in(
        &client,
        "bob".to_string(),
        SecretString::new(SYNTHETIC_KEY_B),
        session.scope(),
    )
    .await
    .expect("bob probe");
    let _ = session.commit_sign_in(probe_b).expect("commit bob");
    let bob_scope = session.scope();
    let bob_id = bob_scope.account_id().cloned().unwrap();
    assert_ne!(alice_id, bob_id);
    assert_eq!(bob_scope.generation, alice_scope.generation + 1);
    assert!(storage.load_account_cache(&alice_id).unwrap().is_none());
    let bob_defaults = session.browse_defaults();
    assert_eq!(bob_defaults.per_page, 64);
    assert_eq!(bob_defaults.purity, PurityMask::ALL);
    assert_eq!(
        bob_defaults.categories,
        CategoryMask {
            general: false,
            anime: true,
            people: false
        }
    );
    assert_eq!(bob_defaults.toplist_range, TopRange::OneYear);

    // Switch back to Alice: bumps generation, clears Bob's cache & in-memory views, keeps Bob's key.
    let switch = session.switch_account(&alice_id).expect("switch to alice");
    assert_eq!(switch.scope.generation, bob_scope.generation + 1);
    assert_eq!(switch.previous_account, Some(bob_id.clone()));
    assert!(storage.load_account_cache(&bob_id).unwrap().is_none());
    assert!(session.user_settings().is_none());
    assert!(creds.load(&bob_id).unwrap().is_some());

    // Refresh Alice under new scope and verify Alice's upstream defaults return.
    let refresh = fetch_account_refresh(&client, session.scope(), "alice".to_string(), false).await;
    assert!(matches!(
        session.apply_refresh(refresh),
        RefreshOutcome::Applied { warning: None }
    ));
    assert_eq!(session.browse_defaults().per_page, 24);
    assert_eq!(session.local_preferences().proxy, ProxyConfig::Direct);

    // Logout Alice -> Guest, generation bumped, Alice removed + cache purged + key deleted,
    // browse defaults reset to Guest, local proxy preference preserved.
    let gen_before_logout = session.scope().generation;
    let logout = session.logout();
    assert!(logout.warning.is_none());
    assert_eq!(session.scope().kind, SessionKind::Guest);
    assert_eq!(session.scope().generation, gen_before_logout + 1);
    assert_eq!(session.browse_defaults(), BrowseDefaults::guest());
    assert!(storage.load_account_cache(&alice_id).unwrap().is_none());
    assert!(creds.load(&alice_id).unwrap().is_none());
    assert_eq!(session.local_preferences().proxy, ProxyConfig::Direct);
}

#[tokio::test]
async fn guest_requesting_nsfw_surfaces_upstream_restriction_without_client_filtering() {
    let server = MockServer::start().await;

    // Guest requesting NSFW wallpaper detail -> upstream returns 401.
    Mock::given(method("GET"))
        .and(path("/api/v1/w/nsfw99"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(401).set_body_string(FIXTURE_ERROR_401))
        .expect(1)
        .mount(&server)
        .await;

    // Guest requesting search with purity=111 -> Wallora sends purity=111 as requested without
    // imposing its own filter; upstream enforces what the guest session may receive.
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("purity", "111"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SEARCH_PAGE))
        .expect(1)
        .mount(&server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let storage = Arc::new(SqliteStore::open_in_memory().expect("db"));
    let client = test_client(&server, creds.clone());
    let mut session = SessionManager::restore(storage, creds);
    let guest_scope = session.scope();
    assert_eq!(guest_scope.kind, SessionKind::Guest);

    // 1. Direct NSFW wallpaper detail -> upstream 401 surfaced as Ready(Err(Unauthorized)),
    // NOT treated as account revocation and does NOT bump generation.
    let detail_res = client
        .wallpaper(WallpaperId::new("nsfw99"), guest_scope.clone())
        .await;
    let handled = session.handle_provider_result(&guest_scope, detail_res);
    assert_eq!(
        handled,
        ScopedResponse::Ready(Err(ProviderError::Unauthorized))
    );
    assert_eq!(session.scope(), guest_scope);

    // 2. Guest search requesting purity=111 is forwarded unmodified to upstream.
    let search_res = client
        .search(
            SearchQuery {
                purity: Some(PurityMask::ALL),
                ..SearchQuery::default()
            },
            guest_scope.clone(),
        )
        .await;
    let handled_search = session.handle_provider_result(&guest_scope, search_res);
    assert!(matches!(handled_search, ScopedResponse::Ready(Ok(_))));
}

#[tokio::test]
async fn manual_proxy_routes_requests_and_preserves_single_api_pacer() {
    // Direct upstream server that triggers a 429 cooldown first.
    let direct_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "2"))
        .expect(1)
        .mount(&direct_server)
        .await;

    // Loopback HTTP proxy that handles absolute-URI GET requests and returns 200 settings JSON.
    let (listener, proxy_addr) = support::bind().await;
    let proxy_hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let hits_clone = Arc::clone(&proxy_hits);
    let _proxy_server = support::serve(listener, move |mut stream| {
        let hits = Arc::clone(&hits_clone);
        async move {
            let head = support::read_request_head(&mut stream)
                .await
                .unwrap_or_default();
            let first_line = head.lines().next().unwrap_or_default().to_string();
            hits.lock().expect("hits lock").push(first_line);
            let body = FIXTURE_SETTINGS.as_bytes();
            let len = body.len().to_string();
            let _ = support::write_head(
                &mut stream,
                &[
                    ("Content-Type", "application/json"),
                    ("Content-Length", &len),
                    ("Connection", "close"),
                ],
            )
            .await;
            let _ = stream.write_all(body).await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        }
    });

    let creds = Arc::new(InMemoryCredentialStore::new());
    let storage = Arc::new(SqliteStore::open_in_memory().expect("db"));
    let client = test_client(&direct_server, creds.clone());
    let mut session = SessionManager::restore(storage.clone(), creds);

    // Trigger a 429 on the direct server (spawned in background so we can observe cooldown while
    // updating the proxy on the SAME client).
    let client_for_429 = client.clone();
    let req_429 = tokio::spawn(async move {
        client_for_429
            .user_settings_with_key(SecretString::new(SYNTHETIC_KEY_A))
            .await
    });

    // Wait until the single ApiPacer registers the 429 cooldown.
    let cooldown = tokio::time::timeout(
        Duration::from_secs(5),
        client.wait_for_api_cooldown_for_test(),
    )
    .await
    .expect("429 cooldown should activate");
    assert!(cooldown > Duration::ZERO);

    // Switch the SAME client to the manual loopback proxy while cooldown is active!
    let proxy_url = format!("http://{proxy_addr}");
    session
        .update_proxy(&client, ProxyConfig::Manual(proxy_url.clone()))
        .expect("update proxy");

    // The in-flight retry after cooldown now goes through the NEW manual proxy on the SAME pacer!
    let settings = tokio::time::timeout(Duration::from_secs(10), req_429)
        .await
        .expect("retried request finishes after cooldown")
        .expect("task join")
        .expect("proxied retry succeeds");
    assert_eq!(settings.per_page, 24);

    let recorded = proxy_hits.lock().expect("hits lock");
    assert!(
        !recorded.is_empty(),
        "request after set_proxy must route through the manual loopback proxy"
    );
    assert!(
        recorded[0].contains("/api/v1/settings"),
        "unexpected proxy request line: {}",
        recorded[0]
    );
    assert_eq!(
        storage.load_local_preferences().unwrap().proxy,
        ProxyConfig::Manual(format!("{proxy_url}/"))
    );
}

#[tokio::test]
async fn partial_failures_rollback_safely_and_never_expose_secrets() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/alice"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;

    // 1. Credential save fails -> SQLite untouched, no secret in error.
    {
        let creds = Arc::new(FaultyCredentialStore::new());
        creds.fail_save.store(true, Ordering::SeqCst);
        let storage = Arc::new(FaultyStorage::in_memory());
        let client = test_client(&server, creds.clone());
        let mut session = SessionManager::restore(storage.clone(), creds.clone());

        let probe = probe_sign_in(
            &client,
            "alice".to_string(),
            SecretString::new(SYNTHETIC_KEY_A),
            session.scope(),
        )
        .await
        .expect("probe");
        let err = session
            .commit_sign_in(probe)
            .expect_err("credential save failure must fail sign-in");
        assert_eq!(err, SignInError::CredentialStore);
        assert!(!err.to_string().contains(SYNTHETIC_KEY_A));
        assert!(storage.list_accounts().unwrap().is_empty());
        assert_eq!(session.scope().kind, SessionKind::Guest);
    }

    // 2. Credential save succeeds, SQLite save fails -> compensating credential delete runs,
    // no orphan left, session stays Guest, no secret exposed.
    {
        let creds = Arc::new(FaultyCredentialStore::new());
        let storage = Arc::new(FaultyStorage::in_memory());
        storage.fail_save_account.store(true, Ordering::SeqCst);
        let client = test_client(&server, creds.clone());
        let mut session = SessionManager::restore(storage.clone(), creds.clone());

        let probe = probe_sign_in(
            &client,
            "alice".to_string(),
            SecretString::new(SYNTHETIC_KEY_A),
            session.scope(),
        )
        .await
        .expect("probe");
        let err = session
            .commit_sign_in(probe)
            .expect_err("SQLite failure must fail sign-in");
        assert_eq!(err, SignInError::Storage);
        assert!(!err.to_string().contains(SYNTHETIC_KEY_A));
        assert!(storage.list_accounts().unwrap().is_empty());
        assert_eq!(session.scope().kind, SessionKind::Guest);
    }

    // 3. Logout when CredentialStore::delete fails -> still transitions to Guest, bumps
    // generation, clears SQLite/in-memory views, surfaces warning without secret.
    {
        let creds = Arc::new(FaultyCredentialStore::new());
        let storage = Arc::new(FaultyStorage::in_memory());
        let client = test_client(&server, creds.clone());
        let mut session = SessionManager::restore(storage.clone(), creds.clone());

        let probe = probe_sign_in(
            &client,
            "alice".to_string(),
            SecretString::new(SYNTHETIC_KEY_A),
            session.scope(),
        )
        .await
        .expect("probe");
        let _ = session.commit_sign_in(probe).expect("commit");
        let signed_in_gen = session.scope().generation;

        creds.fail_delete.store(true, Ordering::SeqCst);
        let logout = session.logout();
        assert_eq!(session.scope().kind, SessionKind::Guest);
        assert_eq!(session.scope().generation, signed_in_gen + 1);
        assert!(storage.list_accounts().unwrap().is_empty());
        let warning = logout.warning.expect("partial cleanup warning expected");
        assert!(!warning.message.contains(SYNTHETIC_KEY_A));
    }

    // 4. Startup restore when SQLite has active account but Credential Manager is missing the key
    // -> falls back cleanly to Guest and purges persisted account cache.
    {
        let creds = Arc::new(FaultyCredentialStore::new());
        let storage = Arc::new(FaultyStorage::in_memory());
        let client = test_client(&server, creds.clone());
        let mut session = SessionManager::restore(storage.clone(), creds.clone());

        let probe = probe_sign_in(
            &client,
            "alice".to_string(),
            SecretString::new(SYNTHETIC_KEY_A),
            session.scope(),
        )
        .await
        .expect("probe");
        let _ = session.commit_sign_in(probe).expect("commit");
        let account_id = session.scope().account_id().cloned().unwrap();
        assert!(storage.load_account_cache(&account_id).unwrap().is_some());

        // Simulate key externally deleted from Credential Manager before next launch.
        creds.delete(&account_id).expect("delete key");
        let restored = SessionManager::restore(storage.clone(), creds.clone());
        assert_eq!(restored.scope().kind, SessionKind::Guest);
        assert!(restored.active_account().is_none());
        assert!(storage.load_account_cache(&account_id).unwrap().is_none());
        let notice = restored.notice().expect("missing credential notice");
        assert!(!notice.message.contains(SYNTHETIC_KEY_A));

        // Also verify unreadable Credential Manager error falls back safely to Guest.
        storage
            .set_active_account(Some(&account_id), None)
            .expect("reactivate in db");
        creds.fail_load.store(true, Ordering::SeqCst);
        let restored_err = SessionManager::restore(storage.clone(), creds);
        assert_eq!(restored_err.scope().kind, SessionKind::Guest);
        assert!(restored_err.active_account().is_none());
    }

    // 5. Proxy persistence failure rolls back transport on the single WallhavenClient.
    {
        let creds = Arc::new(FaultyCredentialStore::new());
        let storage = Arc::new(FaultyStorage::in_memory());
        let client = test_client(&server, creds.clone());
        let mut session = SessionManager::restore(storage.clone(), creds);
        storage.fail_save_prefs.store(true, Ordering::SeqCst);

        let err = session
            .update_proxy(&client, ProxyConfig::Direct)
            .expect_err("storage failure must fail proxy update");
        assert_eq!(err.to_string(), "failed to save local proxy preference");
        assert_eq!(client.proxy(), ProxyConfig::System);
        assert_eq!(session.local_preferences().proxy, ProxyConfig::System);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_database_file_and_logs_never_contain_api_key() {
    let log_buf = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log_buf.clone())
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SETTINGS))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/alice"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .mount(&server)
        .await;

    let temp_dir = std::env::temp_dir().join(format!(
        "wallora-task3-test-{}",
        AccountId::generate().as_str()
    ));
    let db_path = temp_dir.join("wallora.db");

    {
        let storage = Arc::new(SqliteStore::open(&db_path).expect("open file sqlite"));
        let creds = Arc::new(InMemoryCredentialStore::new());
        let client = test_client(&server, creds.clone());
        let mut session = SessionManager::restore(storage, creds);

        let probe = probe_sign_in(
            &client,
            "alice".to_string(),
            SecretString::new(SYNTHETIC_KEY_A),
            session.scope(),
        )
        .await
        .expect("probe");
        let _ = session.commit_sign_in(probe).expect("commit");
    }

    // Inspect raw bytes of the SQLite database file (and WAL if present) on disk.
    let db_bytes = std::fs::read(&db_path).expect("read sqlite file");
    let db_text = String::from_utf8_lossy(&db_bytes);
    assert!(
        !db_text.contains(SYNTHETIC_KEY_A),
        "API key must NEVER be stored in SQLite!"
    );

    let wal_path = temp_dir.join("wallora.db-wal");
    if wal_path.exists() {
        let wal_bytes = std::fs::read(&wal_path).expect("read wal");
        assert!(
            !String::from_utf8_lossy(&wal_bytes).contains(SYNTHETIC_KEY_A),
            "API key must NEVER appear in SQLite WAL!"
        );
    }

    let logs = log_buf.contents();
    assert!(
        !logs.contains(SYNTHETIC_KEY_A),
        "API key must NEVER appear in tracing logs!"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}

#[tokio::test]
async fn sign_in_inconclusive_on_malformed_own_list_and_reverify_keeps_public_unkeyed() {
    let server = MockServer::start().await;

    // Settings with unknown extra field and omitted optional blacklists must still decode cleanly.
    Mock::given(method("GET"))
        .and(path("/api/v1/settings"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"data":{
                "thumb_size":"orig",
                "per_page":"24",
                "purity":["sfw"],
                "categories":["general"],
                "unknown_future_field":"ignored"
            }}"#,
        ))
        .mount(&server)
        .await;

    // First pass: malformed /collections JSON -> sign-in succeeds as Inconclusive (own_collections None).
    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":"corrupt"}"#))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let storage = Arc::new(SqliteStore::open_in_memory().expect("db"));
    let client = test_client(&server, creds.clone());
    let mut session = SessionManager::restore(storage.clone(), creds);

    let probe = probe_sign_in(
        &client,
        "alice".to_string(),
        SecretString::new(SYNTHETIC_KEY_A),
        session.scope(),
    )
    .await
    .expect("valid key with malformed own collections must yield Inconclusive");
    assert_eq!(probe.verification, UsernameVerification::Inconclusive);
    assert!(probe.own_collections.is_none());

    let _ = session.commit_sign_in(probe).expect("commit inconclusive");
    assert_eq!(
        session.active_account().unwrap().verification,
        UsernameVerification::Inconclusive
    );

    // Second pass (signed-in re-verification): /collections now succeeds (keyed),
    // and /collections/alice MUST still be unkeyed even though the session is authenticated!
    Mock::given(method("GET"))
        .and(path("/api/v1/collections"))
        .and(header("x-api-key", SYNTHETIC_KEY_A))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/collections/alice"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_COLLECTIONS_PUBLIC))
        .expect(1)
        .mount(&server)
        .await;

    let refresh = fetch_account_refresh(&client, session.scope(), "alice".to_string(), true).await;
    assert!(matches!(
        session.apply_refresh(refresh),
        RefreshOutcome::Applied { warning: None }
    ));
    assert_eq!(
        session.active_account().unwrap().verification,
        UsernameVerification::Verified
    );
    assert_eq!(
        storage.active_account().unwrap().unwrap().verification,
        UsernameVerification::Verified
    );
    assert_eq!(session.own_collections().unwrap().len(), 1);
}
