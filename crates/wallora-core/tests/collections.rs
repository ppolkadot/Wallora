//! GUI-free collections service, gating, Favorites mapping, and hand-off tests.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wallora_core::domain::{
    AccountId, Category, CollectionSummary, CredentialStore, IdentityCheckOutcome,
    InMemoryCredentialStore, Purity, PurityMask, SearchMeta, SearchPage, SearchQuery, SecretString,
    SessionScope, Thumbs, UserSettings, UsernameVerification, Wallpaper, WallpaperId,
};
use wallora_core::provider::{Provider, ProviderError};
use wallora_core::services::{
    AccountRefreshPayload, ApplyOutcome, CollectionListOutcome, CollectionListStatus,
    CollectionSource, CollectionUnsupportedAction, CollectionsController, DiscoveryStatus,
    FavoritesMapping, OPEN_ON_WALLHAVEN_LABEL, OwnCollectionsListStatus, OwnItemsBlockedReason,
    SessionManager, SignInCommitOutcome, SignInProbe, display_collection_label,
    fetch_collection_items, fetch_user_collections,
};
use wallora_core::storage::{SessionStorage, SqliteStore};

type ListHandler =
    dyn Fn(&str, &SessionScope) -> Result<Vec<CollectionSummary>, ProviderError> + Send + Sync;
type ItemsHandler = dyn Fn(
        &str,
        u64,
        Option<PurityMask>,
        Option<u32>,
        &SessionScope,
    ) -> Result<SearchPage, ProviderError>
    + Send
    + Sync;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedItemsCall {
    username: String,
    collection_id: u64,
    purity: Option<PurityMask>,
    page: Option<u32>,
    scope: SessionScope,
}

#[derive(Clone)]
struct FakeProvider {
    list_handler: Arc<ListHandler>,
    items_handler: Arc<ItemsHandler>,
    list_calls: Arc<Mutex<Vec<(String, SessionScope)>>>,
    items_calls: Arc<Mutex<Vec<RecordedItemsCall>>>,
}

impl FakeProvider {
    fn new(
        list_handler: impl Fn(&str, &SessionScope) -> Result<Vec<CollectionSummary>, ProviderError>
        + Send
        + Sync
        + 'static,
        items_handler: impl Fn(
            &str,
            u64,
            Option<PurityMask>,
            Option<u32>,
            &SessionScope,
        ) -> Result<SearchPage, ProviderError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            list_handler: Arc::new(list_handler),
            items_handler: Arc::new(items_handler),
            list_calls: Arc::default(),
            items_calls: Arc::default(),
        }
    }

    fn list_calls(&self) -> Vec<(String, SessionScope)> {
        self.list_calls.lock().unwrap().clone()
    }

    fn items_calls(&self) -> Vec<RecordedItemsCall> {
        self.items_calls.lock().unwrap().clone()
    }
}

impl Provider for FakeProvider {
    fn search(
        &self,
        _q: SearchQuery,
        _scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<SearchPage, ProviderError>> + Send + 'static {
        std::future::ready(Err(ProviderError::NotFound))
    }

    fn wallpaper(
        &self,
        _id: WallpaperId,
        _scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<Wallpaper, ProviderError>> + Send + 'static {
        std::future::ready(Err(ProviderError::NotFound))
    }

    fn user_settings(
        &self,
        _scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<UserSettings, ProviderError>> + Send + 'static
    {
        std::future::ready(Err(ProviderError::NotFound))
    }

    fn own_collections(
        &self,
        _scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static
    {
        std::future::ready(Err(ProviderError::NotFound))
    }

    fn public_collections(
        &self,
        _username: String,
        _generation: u64,
    ) -> impl std::future::Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static
    {
        std::future::ready(Err(ProviderError::NotFound))
    }

    fn user_settings_with_key(
        &self,
        _api_key: SecretString,
    ) -> impl std::future::Future<Output = Result<Option<UserSettings>, ProviderError>> + Send + 'static
    {
        std::future::ready(Err(ProviderError::NotFound))
    }

    fn own_collections_with_key(
        &self,
        _api_key: SecretString,
    ) -> impl std::future::Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static
    {
        std::future::ready(Err(ProviderError::NotFound))
    }

    fn collections(
        &self,
        username: String,
        scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static
    {
        self.list_calls
            .lock()
            .unwrap()
            .push((username.clone(), scope.clone()));
        let result = (self.list_handler)(&username, &scope);
        async move { result }
    }

    fn collection_items(
        &self,
        username: String,
        collection_id: u64,
        purity: Option<PurityMask>,
        page: Option<u32>,
        scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<SearchPage, ProviderError>> + Send + 'static {
        self.items_calls.lock().unwrap().push(RecordedItemsCall {
            username: username.clone(),
            collection_id,
            purity,
            page,
            scope: scope.clone(),
        });
        let result = (self.items_handler)(&username, collection_id, purity, page, &scope);
        async move { result }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("wallora-collections-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn guest_session() -> SessionManager {
    SessionManager::restore(
        Arc::new(SqliteStore::open_in_memory().unwrap()),
        Arc::new(InMemoryCredentialStore::new()),
    )
}

fn durable_session() -> (TempDir, SessionManager) {
    let dir = TempDir::new();
    let storage: Arc<dyn SessionStorage> =
        Arc::new(SqliteStore::open(&dir.0.join("wallora.db")).unwrap());
    let creds: Arc<dyn CredentialStore> = Arc::new(InMemoryCredentialStore::new());
    (dir, SessionManager::restore(storage, creds))
}

fn sign_in_with(
    session: &mut SessionManager,
    username: &str,
    verification: UsernameVerification,
    own_collections: Vec<CollectionSummary>,
) -> SessionScope {
    let probe = SignInProbe {
        start_scope: session.scope(),
        username: username.to_string(),
        api_key: SecretString::new("wh_synthetic_collections_key"),
        user_settings: None,
        own_collections: Some(own_collections),
        verification,
    };
    match session.commit_sign_in(probe).unwrap() {
        SignInCommitOutcome::Applied { scope, .. } => scope,
        other => panic!("unexpected sign-in outcome: {other:?}"),
    }
}

fn summary(id: u64, label: &str, public: bool, count: u64) -> CollectionSummary {
    CollectionSummary {
        id,
        label: label.to_string(),
        views: 3,
        public,
        count,
    }
}

fn wallpaper(id: &str, purity: Purity) -> Wallpaper {
    Wallpaper {
        id: WallpaperId::new(id),
        url: format!("https://wallhaven.cc/w/{id}"),
        short_url: None,
        uploader: None,
        views: 1,
        favorites: 1,
        source: String::new(),
        purity,
        category: Category::General,
        dimension_x: 1920,
        dimension_y: 1080,
        resolution: "1920x1080".to_string(),
        ratio: "1.78".to_string(),
        file_size: 100,
        file_type: "image/jpeg".to_string(),
        created_at: "2026-01-01 00:00:00".to_string(),
        colors: Vec::new(),
        path: format!("https://w.wallhaven.cc/full/{id}.jpg"),
        thumbs: Thumbs {
            large: format!("https://th.wallhaven.cc/lg/{id}.jpg"),
            original: format!("https://th.wallhaven.cc/orig/{id}.jpg"),
            small: format!("https://th.wallhaven.cc/small/{id}.jpg"),
        },
        tags: Vec::new(),
    }
}

fn page(ids: &[&str], current: u32, last: u32) -> SearchPage {
    SearchPage {
        data: ids.iter().map(|id| wallpaper(id, Purity::Sfw)).collect(),
        meta: SearchMeta {
            current_page: current,
            last_page: last,
            per_page: 24,
            total: ids.len() as u64,
            query: None,
            seed: None,
        },
    }
}

#[tokio::test]
async fn every_non_verified_identity_state_blocks_own_items_while_keeping_list_accessible() {
    let provider = FakeProvider::new(
        |username, _| match username {
            "other_public" => Ok(vec![summary(55, "Scenery", true, 1)]),
            _ => Ok(Vec::new()),
        },
        |username, id, _, _, _| {
            assert_eq!(username, "other_public");
            assert_eq!(id, 55);
            Ok(page(&["pub1"], 1, 1))
        },
    );

    for verification in [
        UsernameVerification::UnverifiablePrivate,
        UsernameVerification::UnverifiableEmpty,
        UsernameVerification::Inconclusive,
    ] {
        let (_dir, mut session) = durable_session();
        let own = if verification == UsernameVerification::UnverifiableEmpty {
            Vec::new()
        } else {
            vec![summary(10, "Default", false, 2)]
        };
        sign_in_with(&mut session, "alice", verification, own.clone());
        let mut controller = CollectionsController::from_session(&session);

        assert!(!controller.can_access_own_collection_items());
        assert_eq!(
            controller.own_items_blocked_reason(),
            Some(OwnItemsBlockedReason::Unverified(verification))
        );
        if verification == UsernameVerification::UnverifiableEmpty {
            assert_eq!(
                controller.own_collections_status(),
                OwnCollectionsListStatus::Empty
            );
        } else {
            assert_eq!(
                controller.own_collections_status(),
                OwnCollectionsListStatus::Ready
            );
            assert_eq!(controller.own_collections().unwrap(), own.as_slice());
        }

        // Direct own-collection selection is blocked with an explanation and sends zero requests.
        let err = controller
            .select_own_collection(10)
            .expect_err("non-verified state must block own items");
        assert_eq!(err, OwnItemsBlockedReason::Unverified(verification));
        assert!(!err.explanation().is_empty());
        assert_eq!(controller.items_blocked(), Some(err));
        assert!(controller.selected_collection().is_none());
        assert!(controller.items().is_empty());

        // Attempting to bypass via the public user browser with own username is also blocked.
        let bypass_err = controller
            .load_public_collection_items("ALICE", 10)
            .expect_err("own username via public browser must remain blocked when unverified");
        assert!(!bypass_err.is_empty());
        assert_eq!(
            controller.items_blocked(),
            Some(OwnItemsBlockedReason::Unverified(verification))
        );

        // Other users' public collections remain browsable.
        let list_req = controller.load_user_collections("other_public").unwrap();
        let list_resp = fetch_user_collections(&provider, list_req).await;
        assert_eq!(
            controller.apply_list_response(&mut session, list_resp),
            CollectionListOutcome::Applied { count: 1 }
        );
        let items_req = controller.select_user_collection(55).unwrap();
        let items_resp = fetch_collection_items(&provider, items_req).await;
        assert_eq!(
            controller.apply_items_response(&mut session, items_resp),
            ApplyOutcome::Applied { added: 1 }
        );
        assert_eq!(controller.items().len(), 1);
    }

    // Only the three non-bypass public-user requests hit the provider.
    assert_eq!(provider.list_calls().len(), 3);
    assert_eq!(provider.items_calls().len(), 3);
}

#[tokio::test]
async fn verified_account_loads_own_collection_items_with_purity_and_pagination() {
    let (_dir, mut session) = durable_session();
    let scope = sign_in_with(
        &mut session,
        "alice",
        UsernameVerification::Verified,
        vec![
            summary(10, "Default", true, 3),
            summary(11, "PrivateVault", false, 4),
        ],
    );

    let provider = FakeProvider::new(
        |_, _| Ok(Vec::new()),
        |username, id, purity, page_num, req_scope| {
            assert_eq!(username, "alice");
            assert_eq!(id, 11);
            assert_eq!(req_scope, &session_scope_placeholder(req_scope));
            match page_num.unwrap_or(1) {
                1 => Ok(page(&["own1", "own2"], 1, 2)),
                2 => Ok(page(&["own3"], 2, 2)),
                _ => panic!("unexpected page"),
            }
            .map(|mut p| {
                if purity == Some(PurityMask::ALL) {
                    p.data[0].purity = Purity::Nsfw;
                }
                p
            })
        },
    );

    fn session_scope_placeholder(s: &SessionScope) -> SessionScope {
        s.clone()
    }

    let mut controller = CollectionsController::from_session(&session);
    assert!(controller.can_access_own_collection_items());
    assert_eq!(controller.own_items_blocked_reason(), None);

    // Unresolved Favorites mapping must show exact upstream label ("Default").
    let default_col = &controller.own_collections().unwrap()[0];
    assert_eq!(
        controller.display_own_collection_label(default_col),
        "Default"
    );

    let req1 = controller
        .select_own_collection(11)
        .expect("verified account can view own collection items");
    assert_eq!(req1.scope, scope);
    assert_eq!(req1.collection_id, 11);
    assert_eq!(req1.purity, PurityMask::SFW_ONLY);
    assert_eq!(req1.page, 1);

    let resp1 = fetch_collection_items(&provider, req1).await;
    assert_eq!(
        controller.apply_items_response(&mut session, resp1),
        ApplyOutcome::Applied { added: 2 }
    );
    assert!(controller.can_load_more());

    // Changing purity reloads page 1 with the new purity parameter.
    let purity_req = controller
        .set_purity(PurityMask::ALL)
        .expect("purity change triggers reload");
    assert_eq!(purity_req.purity, PurityMask::ALL);
    assert_eq!(purity_req.page, 1);
    let resp1_all = fetch_collection_items(&provider, purity_req).await;
    assert_eq!(
        controller.apply_items_response(&mut session, resp1_all),
        ApplyOutcome::Applied { added: 2 }
    );
    assert_eq!(controller.items()[0].purity, Purity::Nsfw);

    let req2 = controller.request_next_page().expect("page 2 available");
    assert_eq!(req2.page, 2);
    let resp2 = fetch_collection_items(&provider, req2).await;
    assert_eq!(
        controller.apply_items_response(&mut session, resp2),
        ApplyOutcome::Applied { added: 1 }
    );
    assert!(!controller.can_load_more());
    assert!(controller.request_next_page().is_none());
    assert_eq!(controller.items().len(), 3);

    let calls = provider.items_calls();
    assert_eq!(calls[0].page, None);
    assert_eq!(calls[1].purity, Some(PurityMask::ALL));
    assert_eq!(calls[2].page, Some(2));
}

#[tokio::test]
async fn guest_private_collection_surfaces_upstream_denial_without_revocation_or_filtering() {
    let provider = FakeProvider::new(
        |_, _| Ok(vec![summary(404, "MissingOrPrivate", true, 1)]),
        |_, id, _, _, _| match id {
            901 => Err(ProviderError::Unauthorized),
            903 => Err(ProviderError::Forbidden),
            904 => Err(ProviderError::NotFound),
            500 => Ok(SearchPage {
                data: vec![
                    wallpaper("w1", Purity::Sfw),
                    wallpaper("w2", Purity::Sketchy),
                    wallpaper("w3", Purity::Nsfw),
                ],
                meta: SearchMeta {
                    current_page: 1,
                    last_page: 1,
                    per_page: 3,
                    total: 3,
                    query: None,
                    seed: None,
                },
            }),
            _ => Err(ProviderError::NotFound),
        },
    );

    let mut session = guest_session();
    let guest_scope = session.scope();
    let mut controller = CollectionsController::from_session(&session);

    for (id, expected) in [
        (901, ProviderError::Unauthorized),
        (903, ProviderError::Forbidden),
        (904, ProviderError::NotFound),
    ] {
        let req = controller
            .load_public_collection_items("alice", id)
            .unwrap();
        let resp = fetch_collection_items(&provider, req).await;
        assert_eq!(
            controller.apply_items_response(&mut session, resp),
            ApplyOutcome::Failed(expected.clone())
        );
        assert_eq!(
            controller.items_status(),
            &DiscoveryStatus::Failed {
                page: 1,
                error: expected
            }
        );
        assert_eq!(
            session.scope(),
            guest_scope,
            "guest denial never revokes guest"
        );
        assert!(controller.retry_items().is_some());
    }

    // No client-side purity filtering: whatever upstream returns is preserved.
    let req = controller
        .load_public_collection_items("alice", 500)
        .unwrap();
    let resp = fetch_collection_items(&provider, req).await;
    controller.apply_items_response(&mut session, resp);
    let purities: Vec<Purity> = controller.items().iter().map(|w| w.purity).collect();
    assert_eq!(purities, vec![Purity::Sfw, Purity::Sketchy, Purity::Nsfw]);
}

#[tokio::test]
async fn empty_list_empty_collection_pagination_dedup_and_stale_dropping_work() {
    let fail_page_two = Arc::new(Mutex::new(true));
    let provider = {
        let fail_page_two = Arc::clone(&fail_page_two);
        FakeProvider::new(
            |username, _| match username {
                "emptyuser" => Ok(Vec::new()),
                "stale_a" => Ok(vec![summary(1, "A", true, 1)]),
                "erruser" => Err(ProviderError::Network),
                _ => Ok(vec![
                    summary(10, "EmptyItems", true, 0),
                    summary(20, "ThreePages", true, 5),
                ]),
            },
            move |_, id, _, page_num, _| {
                if id == 10 {
                    return Ok(page(&[], 1, 1));
                }
                match page_num.unwrap_or(1) {
                    1 => Ok(page(&["a", "b"], 1, 3)),
                    2 if *fail_page_two.lock().unwrap() => Err(ProviderError::Timeout),
                    // Page 2 intentionally repeats "b" from page 1 to verify deduplication.
                    2 => Ok(page(&["b", "c"], 2, 3)),
                    3 => Ok(page(&["d"], 3, 3)),
                    _ => panic!("unexpected page beyond last_page"),
                }
            },
        )
    };

    let mut session = guest_session();
    let mut controller = CollectionsController::from_session(&session);

    // 1. Empty collection list.
    let req = controller.load_user_collections("emptyuser").unwrap();
    assert_eq!(
        controller.user_collections_status(),
        &CollectionListStatus::Loading
    );
    let resp = fetch_user_collections(&provider, req).await;
    assert_eq!(
        controller.apply_list_response(&mut session, resp),
        CollectionListOutcome::Applied { count: 0 }
    );
    assert_eq!(
        controller.user_collections_status(),
        &CollectionListStatus::Empty
    );

    // 2. Stale list response from an older generation is dropped.
    let old_req = controller.load_user_collections("stale_a").unwrap();
    let new_req = controller.load_user_collections("normaluser").unwrap();
    assert!(new_req.generation > old_req.generation);
    let old_resp = fetch_user_collections(&provider, old_req).await;
    let new_resp = fetch_user_collections(&provider, new_req).await;
    assert_eq!(
        controller.apply_list_response(&mut session, old_resp),
        CollectionListOutcome::Stale
    );
    assert_eq!(
        controller.apply_list_response(&mut session, new_resp),
        CollectionListOutcome::Applied { count: 2 }
    );

    // 3. Empty collection (0 items) -> DiscoveryStatus::Empty.
    let empty_req = controller.select_user_collection(10).unwrap();
    let empty_resp = fetch_collection_items(&provider, empty_req).await;
    controller.apply_items_response(&mut session, empty_resp);
    assert_eq!(controller.items_status(), &DiscoveryStatus::Empty);
    assert!(!controller.can_load_more());

    // 4. Stale items generation dropped, dedup across pages, error retry, and stop at last_page.
    let stale_req = controller.select_user_collection(20).unwrap();
    let active_req = controller
        .set_purity(PurityMask::SFW_AND_SKETCHY)
        .expect("purity change supersedes stale page 1");
    let stale_resp = fetch_collection_items(&provider, stale_req).await;
    assert_eq!(
        controller.apply_items_response(&mut session, stale_resp),
        ApplyOutcome::Stale
    );
    let active_resp = fetch_collection_items(&provider, active_req).await;
    assert_eq!(
        controller.apply_items_response(&mut session, active_resp.clone()),
        ApplyOutcome::Applied { added: 2 }
    );
    // Duplicate delivery of same page is dropped as Stale.
    assert_eq!(
        controller.apply_items_response(&mut session, active_resp),
        ApplyOutcome::Stale
    );

    let p2_fail_req = controller.request_next_page().unwrap();
    assert!(
        controller.request_page(2).is_none(),
        "page 2 is already in flight"
    );
    let p2_fail_resp = fetch_collection_items(&provider, p2_fail_req).await;
    assert_eq!(
        controller.apply_items_response(&mut session, p2_fail_resp),
        ApplyOutcome::Failed(ProviderError::Timeout)
    );
    assert!(controller.request_next_page().is_none());

    *fail_page_two.lock().unwrap() = false;
    let p2_retry_req = controller.retry_items().unwrap();
    let p2_retry_resp = fetch_collection_items(&provider, p2_retry_req).await;
    assert_eq!(
        controller.apply_items_response(&mut session, p2_retry_resp),
        ApplyOutcome::Applied { added: 1 }
    );

    let p3_req = controller.request_next_page().unwrap();
    let p3_resp = fetch_collection_items(&provider, p3_req).await;
    assert_eq!(
        controller.apply_items_response(&mut session, p3_resp),
        ApplyOutcome::Applied { added: 1 }
    );
    assert!(!controller.can_load_more());
    assert!(controller.request_next_page().is_none());
    assert!(controller.request_page(4).is_none());
    let ids: Vec<&str> = controller.items().iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", "c", "d"]);

    // 5. List error + retry.
    let err_req = controller.load_user_collections("erruser").unwrap();
    let err_resp = fetch_user_collections(&provider, err_req).await;
    assert_eq!(
        controller.apply_list_response(&mut session, err_resp),
        CollectionListOutcome::Failed(ProviderError::Network)
    );
    assert!(controller.retry_user_collections().is_some());
}

#[test]
fn favorites_label_policy_is_per_account_and_requires_verified_username() {
    let default_col = summary(1, "Default", false, 2);
    let other_col = summary(2, "Landscapes", true, 5);
    let account_a = AccountId::new("account-a");
    let account_b = AccountId::new("account-b");

    // 1. Unresolved mapping never guesses "Favorites", even for "Default".
    assert_eq!(
        display_collection_label(
            &default_col,
            Some(&account_a),
            Some(UsernameVerification::Verified),
            None,
        ),
        "Default"
    );

    // 2. Confirmed mapping applies ONLY when Account A is Verified and ID matches.
    let mut mapping = FavoritesMapping::new();
    mapping.record_confirmed(account_a.clone(), 1);
    assert_eq!(
        mapping.display_label(
            &default_col,
            Some(&account_a),
            Some(UsernameVerification::Verified)
        ),
        "Favorites"
    );
    assert_eq!(
        mapping.display_label(
            &other_col,
            Some(&account_a),
            Some(UsernameVerification::Verified)
        ),
        "Landscapes"
    );

    // 3. Account A's confirmed ID never applies when unverified, to Account B, or to Guest.
    for ver in [
        UsernameVerification::UnverifiablePrivate,
        UsernameVerification::UnverifiableEmpty,
        UsernameVerification::Inconclusive,
    ] {
        assert_eq!(
            mapping.display_label(&default_col, Some(&account_a), Some(ver)),
            "Default"
        );
    }
    assert_eq!(
        mapping.display_label(
            &default_col,
            Some(&account_b),
            Some(UsernameVerification::Verified)
        ),
        "Default"
    );
    assert_eq!(mapping.display_label(&default_col, None, None), "Default");
}

#[test]
fn unsupported_write_handoffs_validate_urls_and_labels() {
    for action in CollectionUnsupportedAction::ALL {
        assert_eq!(action.label(), OPEN_ON_WALLHAVEN_LABEL);
        assert!(!action.title().is_empty());
        assert!(!action.explanation().is_empty());
    }
    assert_eq!(
        CollectionUnsupportedAction::favorite_url(&WallpaperId::new("abc123")),
        Ok("https://wallhaven.cc/w/abc123".to_string())
    );
    for bad in ["", ".", "..", "a/b", "a\\b", "a?x=1", "a#frag", "a b"] {
        assert!(CollectionUnsupportedAction::favorite_url(&WallpaperId::new(bad)).is_err());
    }
    assert_eq!(
        CollectionUnsupportedAction::manage_collections_url(Some("alice")),
        "https://wallhaven.cc/user/alice/favorites"
    );
    assert_eq!(
        CollectionUnsupportedAction::manage_collections_url(None),
        "https://wallhaven.cc/"
    );
    assert_eq!(
        CollectionUnsupportedAction::manage_collections_url(Some("../bad")),
        "https://wallhaven.cc/"
    );
}

#[tokio::test]
async fn account_401_and_verification_downgrade_clear_private_collection_state() {
    let (_dir, mut session) = durable_session();
    let scope = sign_in_with(
        &mut session,
        "alice",
        UsernameVerification::Verified,
        vec![summary(10, "Private", false, 3)],
    );
    let provider = FakeProvider::new(
        |_, _| Ok(Vec::new()),
        |_, _, _, page_num, _| match page_num.unwrap_or(1) {
            1 => Ok(page(&["p1"], 1, 2)),
            _ => Err(ProviderError::Unauthorized),
        },
    );

    let mut controller = CollectionsController::from_session(&session);
    let req1 = controller.select_own_collection(10).unwrap();
    let resp1 = fetch_collection_items(&provider, req1).await;
    controller.apply_items_response(&mut session, resp1);
    assert_eq!(controller.items().len(), 1);
    assert!(controller.selected_collection().is_some());

    // Mid-session 401 revokes session and clears private collection state.
    let req2 = controller.request_next_page().unwrap();
    let resp2 = fetch_collection_items(&provider, req2).await;
    let outcome = controller.apply_items_response(&mut session, resp2);
    assert!(matches!(outcome, ApplyOutcome::Revoked(_)));
    assert_eq!(session.scope(), SessionScope::guest(scope.generation + 1));
    assert_eq!(controller.scope(), &session.scope());
    assert!(controller.own_account().is_none());
    assert!(controller.own_collections().is_none());
    assert!(controller.selected_collection().is_none());
    assert!(controller.items().is_empty());
    assert_eq!(controller.items_status(), &DiscoveryStatus::Idle);
    assert_eq!(
        controller.selected_collection().map(|s| &s.source),
        None::<&CollectionSource>
    );

    // Verification downgrade via AccountRefreshPayload clears active own items while keeping list.
    sign_in_with(
        &mut session,
        "bob",
        UsernameVerification::Verified,
        vec![summary(20, "Vault", false, 2)],
    );
    let mut controller = CollectionsController::from_session(&session);
    let req = controller.select_own_collection(20).unwrap();
    let resp = fetch_collection_items(
        &FakeProvider::new(
            |_, _| Ok(Vec::new()),
            |_, _, _, _, _| Ok(page(&["b1"], 1, 1)),
        ),
        req,
    )
    .await;
    controller.apply_items_response(&mut session, resp);
    assert_eq!(controller.items().len(), 1);

    let payload = AccountRefreshPayload {
        request_scope: session.scope(),
        user_settings: Ok(UserSettings {
            thumb_size: "large".into(),
            per_page: 24,
            purity: vec!["sfw".into()],
            categories: vec!["general".into()],
            resolutions: Vec::new(),
            aspect_ratios: Vec::new(),
            toplist_range: None,
            tag_blacklist: Vec::new(),
            user_blacklist: Vec::new(),
            ai_art_filter: None,
        }),
        own_collections: Ok(vec![summary(20, "Vault", false, 2)]),
        identity_outcome: Some(IdentityCheckOutcome::UnverifiablePrivate),
    };
    session.apply_refresh(payload);
    assert!(controller.sync_session(&session));
    assert!(controller.selected_collection().is_none());
    assert!(controller.items().is_empty());
    assert_eq!(
        controller.items_blocked(),
        Some(OwnItemsBlockedReason::Unverified(
            UsernameVerification::UnverifiablePrivate
        ))
    );
    assert_eq!(controller.own_collections().unwrap().len(), 1);
}
