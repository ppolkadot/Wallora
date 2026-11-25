//! GUI-free profiles and tags service tests (Task 6b).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wallora_core::domain::{
    Category, CollectionSummary, CredentialStore, InMemoryCredentialStore, Purity, PurityMask,
    QueryEcho, SearchMeta, SearchPage, SearchQuery, SecretString, SessionKind, SessionScope,
    SortOrder, Sorting, Tag, Thumbs, TopRange, UserSettings, UsernameVerification, Wallpaper,
    WallpaperId,
};
use wallora_core::provider::{Provider, ProviderError};
use wallora_core::services::{
    ApplyOutcome, DiscoveryStatus, OPEN_ON_WALLHAVEN_LABEL, ProfileTagUnsupportedAction,
    SessionManager, SignInCommitOutcome, SignInProbe, TagMetadataOutcome, TagMetadataStatus,
    TagPageController, UploaderController, WALLHAVEN_TAGS_URL, fetch_page, fetch_tag_metadata,
};
use wallora_core::storage::{SessionStorage, SqliteStore};

type TagHandler = dyn Fn(u64, &SessionScope) -> Result<Tag, ProviderError> + Send + Sync;
type SearchHandler =
    dyn Fn(&SearchQuery, &SessionScope) -> Result<SearchPage, ProviderError> + Send + Sync;

#[derive(Clone)]
struct FakeProvider {
    tag_handler: Arc<TagHandler>,
    search_handler: Arc<SearchHandler>,
    tag_calls: Arc<Mutex<Vec<(u64, SessionScope)>>>,
    search_calls: Arc<Mutex<Vec<(SearchQuery, SessionScope)>>>,
}

impl FakeProvider {
    fn new(
        tag_handler: impl Fn(u64, &SessionScope) -> Result<Tag, ProviderError> + Send + Sync + 'static,
        search_handler: impl Fn(&SearchQuery, &SessionScope) -> Result<SearchPage, ProviderError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            tag_handler: Arc::new(tag_handler),
            search_handler: Arc::new(search_handler),
            tag_calls: Arc::default(),
            search_calls: Arc::default(),
        }
    }

    fn tag_calls(&self) -> Vec<(u64, SessionScope)> {
        self.tag_calls.lock().unwrap().clone()
    }

    fn search_calls(&self) -> Vec<(SearchQuery, SessionScope)> {
        self.search_calls.lock().unwrap().clone()
    }
}

impl Provider for FakeProvider {
    fn search(
        &self,
        q: SearchQuery,
        scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<SearchPage, ProviderError>> + Send + 'static {
        self.search_calls
            .lock()
            .unwrap()
            .push((q.clone(), scope.clone()));
        let result = (self.search_handler)(&q, &scope);
        async move { result }
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
        _username: String,
        _scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static
    {
        std::future::ready(Err(ProviderError::NotFound))
    }

    fn collection_items(
        &self,
        _username: String,
        _collection_id: u64,
        _purity: Option<PurityMask>,
        _page: Option<u32>,
        _scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<SearchPage, ProviderError>> + Send + 'static {
        std::future::ready(Err(ProviderError::NotFound))
    }

    fn tag(
        &self,
        id: u64,
        scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<Tag, ProviderError>> + Send + 'static {
        self.tag_calls.lock().unwrap().push((id, scope.clone()));
        let result = (self.tag_handler)(id, &scope);
        async move { result }
    }
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir =
            std::env::temp_dir().join(format!("wallora-profiles-tags-{}", uuid::Uuid::new_v4()));
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

fn sign_in(session: &mut SessionManager, username: &str) -> SessionScope {
    let probe = SignInProbe {
        start_scope: session.scope(),
        username: username.to_string(),
        api_key: SecretString::new("wh_synthetic_profiles_tags_key"),
        user_settings: None,
        own_collections: Some(Vec::new()),
        verification: UsernameVerification::Verified,
    };
    match session.commit_sign_in(probe).unwrap() {
        SignInCommitOutcome::Applied { scope, .. } => scope,
        other => panic!("unexpected sign-in outcome: {other:?}"),
    }
}

fn sample_tag(id: u64, name: &str) -> Tag {
    Tag {
        id,
        name: name.to_string(),
        alias: "Chinese cartoons".to_string(),
        category_id: 1,
        category: "Anime & Manga".to_string(),
        purity: Purity::Sfw,
        created_at: "2015-01-16 02:06:45".to_string(),
    }
}

fn wallpaper(id: &str, purity: Purity) -> Wallpaper {
    Wallpaper {
        id: WallpaperId::new(id),
        url: format!("https://wallhaven.cc/w/{id}"),
        short_url: None,
        uploader: None,
        views: 10,
        favorites: 5,
        source: String::new(),
        purity,
        category: Category::General,
        dimension_x: 1920,
        dimension_y: 1080,
        resolution: "1920x1080".to_string(),
        ratio: "1.78".to_string(),
        file_size: 2048,
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

fn page(
    ids: &[&str],
    current: u32,
    last: u32,
    query: Option<QueryEcho>,
    seed: Option<&str>,
) -> SearchPage {
    SearchPage {
        data: ids.iter().map(|id| wallpaper(id, Purity::Sfw)).collect(),
        meta: SearchMeta {
            current_page: current,
            last_page: last,
            per_page: 24,
            total: u64::from(last) * 24,
            query,
            seed: seed.map(str::to_string),
        },
    }
}

#[tokio::test]
async fn tag_page_loads_metadata_and_id_search_paginates_dedups_retries_and_drops_stale() {
    let fail_meta = Arc::new(Mutex::new(true));
    let fail_p2 = Arc::new(Mutex::new(true));
    let seed_counter = Arc::new(Mutex::new(0u32));

    let provider = {
        let fail_meta = Arc::clone(&fail_meta);
        let fail_p2 = Arc::clone(&fail_p2);
        let seed_counter = Arc::clone(&seed_counter);
        FakeProvider::new(
            move |id, _| {
                if id == 1 && *fail_meta.lock().unwrap() {
                    return Err(ProviderError::Timeout);
                }
                Ok(sample_tag(id, if id == 1 { "anime" } else { "scenery" }))
            },
            move |q, _| {
                if q.sorting == Some(Sorting::Random) {
                    let mut guard = seed_counter.lock().unwrap();
                    *guard += 1;
                    let seed = q.seed.clone().unwrap_or_else(|| format!("rseed{}", *guard));
                    let p = q.page.unwrap_or(1);
                    return Ok(page(
                        &[format!("rnd{p}").as_str()],
                        p,
                        3,
                        Some(QueryEcho::Tag {
                            id: 1,
                            tag: "anime".to_string(),
                        }),
                        Some(&seed),
                    ));
                }
                match q.page.unwrap_or(1) {
                    1 => Ok(page(
                        &["w1", "w2"],
                        1,
                        3,
                        Some(QueryEcho::Tag {
                            id: 1,
                            tag: "anime".to_string(),
                        }),
                        None,
                    )),
                    2 if *fail_p2.lock().unwrap() => Err(ProviderError::Network),
                    // Repeats "w2" to verify cross-page deduplication.
                    2 => Ok(page(&["w2", "w3"], 2, 3, None, None)),
                    3 => Ok(page(&["w4"], 3, 3, None, None)),
                    other => panic!("unexpected page beyond last_page: {other}"),
                }
            },
        )
    };

    let mut session = guest_session();
    let mut controller = TagPageController::from_session(&session);

    // Invalid tag ID 0 is rejected locally without touching the provider.
    assert!(controller.open_tag(0, None).is_err());
    assert!(provider.tag_calls().is_empty());
    assert!(provider.search_calls().is_empty());

    // Open tag 99, then immediately supersede with tag 1.
    let stale_reqs = controller.open_tag(99, Some("old".to_string())).unwrap();
    let reqs = controller.open_tag(1, None).unwrap();
    assert_eq!(reqs.metadata.tag_id, 1);
    assert_eq!(reqs.page.query.q.as_deref(), Some("id:1"));
    assert_eq!(reqs.page.query.categories, None);
    assert_eq!(reqs.page.query.purity, Some(PurityMask::SFW_ONLY));
    assert_eq!(reqs.page.query.sorting, Some(Sorting::DateAdded));
    assert_eq!(reqs.page.query.page, None);

    // Stale metadata & page responses from superseded tag 99 are dropped.
    let stale_meta_resp = fetch_tag_metadata(&provider, stale_reqs.metadata).await;
    let stale_page_resp = fetch_page(&provider, stale_reqs.page).await;
    assert_eq!(
        controller.apply_metadata_response(&mut session, stale_meta_resp),
        TagMetadataOutcome::Stale
    );
    assert_eq!(
        controller.apply_page_response(&mut session, stale_page_resp),
        ApplyOutcome::Stale
    );

    // Metadata fails first, then retry_metadata recovers cleanly.
    let meta_fail_resp = fetch_tag_metadata(&provider, reqs.metadata).await;
    assert_eq!(
        controller.apply_metadata_response(&mut session, meta_fail_resp),
        TagMetadataOutcome::Failed(ProviderError::Timeout)
    );
    assert_eq!(
        controller.metadata_status(),
        &TagMetadataStatus::Failed(ProviderError::Timeout)
    );
    *fail_meta.lock().unwrap() = false;
    let meta_retry = controller.retry_metadata().expect("metadata retry");
    let meta_ok_resp = fetch_tag_metadata(&provider, meta_retry).await;
    assert_eq!(
        controller.apply_metadata_response(&mut session, meta_ok_resp),
        TagMetadataOutcome::Applied
    );
    assert_eq!(controller.tag().map(|t| t.name.as_str()), Some("anime"));
    assert_eq!(controller.hint_name(), Some("anime"));

    // Page 1 applies; duplicate delivery of page 1 is dropped as Stale.
    let p1_resp = fetch_page(&provider, reqs.page).await;
    assert_eq!(
        controller.apply_page_response(&mut session, p1_resp.clone()),
        ApplyOutcome::Applied { added: 2 }
    );
    assert_eq!(
        controller.apply_page_response(&mut session, p1_resp),
        ApplyOutcome::Stale
    );
    assert!(controller.can_load_more());

    // Page 2 fails, blocks next page until retry_items, then deduplicates w2 and loads w3.
    let p2_req = controller.request_next_page().unwrap();
    assert!(
        controller.request_page(2).is_none(),
        "page 2 is already in flight"
    );
    let p2_fail = fetch_page(&provider, p2_req).await;
    assert_eq!(
        controller.apply_page_response(&mut session, p2_fail),
        ApplyOutcome::Failed(ProviderError::Network)
    );
    assert!(controller.request_next_page().is_none());

    *fail_p2.lock().unwrap() = false;
    let p2_retry = controller.retry_items().unwrap();
    let p2_ok = fetch_page(&provider, p2_retry).await;
    assert_eq!(
        controller.apply_page_response(&mut session, p2_ok),
        ApplyOutcome::Applied { added: 1 }
    );

    // Page 3 hits last_page = 3 and stops.
    let p3_req = controller.request_next_page().unwrap();
    let p3_ok = fetch_page(&provider, p3_req).await;
    assert_eq!(
        controller.apply_page_response(&mut session, p3_ok),
        ApplyOutcome::Applied { added: 1 }
    );
    assert!(!controller.can_load_more());
    assert!(controller.request_next_page().is_none());
    assert!(controller.request_page(4).is_none());
    let ids: Vec<&str> = controller.items().iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, vec!["w1", "w2", "w3", "w4"]);

    // Changing purity/sorting/order/top_range reloads ONLY items, keeping loaded metadata intact.
    let tag_calls_before_filter = provider.tag_calls().len();
    let purity_req = controller
        .set_purity(PurityMask::SFW_AND_SKETCHY)
        .expect("purity change reloads items");
    assert_eq!(purity_req.query.purity, Some(PurityMask::SFW_AND_SKETCHY));
    assert!(controller.set_purity(PurityMask::SFW_AND_SKETCHY).is_none());
    let _ = controller.apply_page_response(&mut session, fetch_page(&provider, purity_req).await);

    let top_req = controller
        .set_top_range(TopRange::OneWeek)
        .expect("top range activates Toplist sorting");
    assert_eq!(top_req.query.sorting, Some(Sorting::Toplist));
    assert_eq!(top_req.query.top_range, Some(TopRange::OneWeek));
    let _ = controller.apply_page_response(&mut session, fetch_page(&provider, top_req).await);

    let ord_req = controller
        .set_order(Some(SortOrder::Asc))
        .expect("order change reloads");
    assert_eq!(ord_req.query.order, Some(SortOrder::Asc));
    let _ = controller.apply_page_response(&mut session, fetch_page(&provider, ord_req).await);

    // Random sorting retains meta.seed across pages and rotates on refresh().
    let rnd_req = controller.set_sorting(Sorting::Random).unwrap();
    assert!(rnd_req.query.seed.is_none());
    let _ = controller.apply_page_response(&mut session, fetch_page(&provider, rnd_req).await);
    assert_eq!(controller.seed(), Some("rseed1"));
    let rnd_p2 = controller.request_next_page().unwrap();
    assert_eq!(rnd_p2.query.seed.as_deref(), Some("rseed1"));
    assert_eq!(
        provider.tag_calls().len(),
        tag_calls_before_filter,
        "filter changes must not re-fetch tag metadata"
    );
    assert_eq!(controller.tag().map(|t| t.id), Some(1));

    let refreshed = controller.refresh().unwrap();
    assert!(controller.seed().is_none());
    assert!(refreshed.page.query.seed.is_none());
    assert_eq!(refreshed.metadata.tag_id, 1);
}

#[tokio::test]
async fn uploader_loads_at_username_rejects_invalid_paginates_and_drops_stale() {
    let fail_p2 = Arc::new(Mutex::new(true));
    let provider = {
        let fail_p2 = Arc::clone(&fail_p2);
        FakeProvider::new(
            |_, _| Err(ProviderError::NotFound),
            move |q, _| {
                if q.q.as_deref() == Some("@stale_user") {
                    return Ok(page(&["stale1"], 1, 1, None, None));
                }
                assert_eq!(q.q.as_deref(), Some("@artist_one"));
                assert_eq!(q.categories, None);
                match q.page.unwrap_or(1) {
                    1 => Ok(page(&["u1", "u2"], 1, 2, None, None)),
                    2 if *fail_p2.lock().unwrap() => Err(ProviderError::Timeout),
                    2 => Ok(page(&["u2", "u3"], 2, 2, None, None)),
                    other => panic!("unexpected uploader page {other}"),
                }
            },
        )
    };

    let mut session = guest_session();
    let mut controller = UploaderController::from_session(&session);

    for bad in [
        "", "   ", ".", "..", "a/b", "a\\b", "a?x=1", "a#b", "a%20b", "a b", "a\"b",
    ] {
        assert!(
            controller.open_uploader(bad).is_err(),
            "invalid username {bad:?} must fail"
        );
    }
    assert!(provider.search_calls().is_empty());

    let stale_req = controller.open_uploader("stale_user").unwrap();
    let active_req = controller.open_uploader("  artist_one  ").unwrap();
    assert_eq!(controller.username(), Some("artist_one"));
    assert_eq!(
        controller.search_query_string().as_deref(),
        Some("@artist_one")
    );
    assert_eq!(active_req.query.q.as_deref(), Some("@artist_one"));
    assert!(active_req.generation > stale_req.generation);

    // Stale generation is dropped.
    let stale_resp = fetch_page(&provider, stale_req).await;
    assert_eq!(
        controller.apply_response(&mut session, stale_resp),
        ApplyOutcome::Stale
    );

    let p1_resp = fetch_page(&provider, active_req).await;
    assert_eq!(
        controller.apply_response(&mut session, p1_resp.clone()),
        ApplyOutcome::Applied { added: 2 }
    );
    assert_eq!(
        controller.apply_response(&mut session, p1_resp),
        ApplyOutcome::Stale
    );

    let p2_req = controller.request_next_page().unwrap();
    let p2_fail = fetch_page(&provider, p2_req).await;
    assert_eq!(
        controller.apply_response(&mut session, p2_fail),
        ApplyOutcome::Failed(ProviderError::Timeout)
    );
    *fail_p2.lock().unwrap() = false;
    let p2_retry = controller.retry().unwrap();
    let p2_ok = fetch_page(&provider, p2_retry).await;
    assert_eq!(
        controller.apply_response(&mut session, p2_ok),
        ApplyOutcome::Applied { added: 1 }
    );
    assert!(!controller.can_load_more());
    assert!(controller.request_next_page().is_none());
    let ids: Vec<&str> = controller.items().iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, vec!["u1", "u2", "u3"]);
}

#[tokio::test]
async fn account_401_revokes_and_clears_while_guest_401_surfaces_restriction() {
    // 1. Account 401 on tag metadata revokes session and clears tag state.
    {
        let (_dir, mut session) = durable_session();
        let account_scope = sign_in(&mut session, "alice");
        let provider = FakeProvider::new(
            |_, _| Err(ProviderError::Unauthorized),
            |_, _| Ok(page(&["t1"], 1, 1, None, None)),
        );
        let mut controller = TagPageController::from_session(&session);
        let reqs = controller.open_tag(5, Some("secret".to_string())).unwrap();
        let _ =
            controller.apply_page_response(&mut session, fetch_page(&provider, reqs.page).await);
        assert_eq!(controller.items().len(), 1);

        let meta_resp = fetch_tag_metadata(&provider, reqs.metadata).await;
        let outcome = controller.apply_metadata_response(&mut session, meta_resp);
        assert!(matches!(outcome, TagMetadataOutcome::Revoked(_)));
        assert_eq!(
            session.scope(),
            SessionScope::guest(account_scope.generation + 1)
        );
        assert_eq!(controller.scope(), &session.scope());
        assert!(controller.items().is_empty());
        assert!(controller.hint_name().is_none());
        assert_eq!(controller.metadata_status(), &TagMetadataStatus::Idle);
        assert_eq!(controller.items_status(), &DiscoveryStatus::Idle);
    }

    // 2. Account 401 on tag page AND uploader page each revoke session and clear state.
    {
        let (_dir, mut session) = durable_session();
        let scope_a = sign_in(&mut session, "bob");
        let provider = FakeProvider::new(
            |id, _| Ok(sample_tag(id, "nature")),
            |_, _| Err(ProviderError::Unauthorized),
        );
        let mut tag_ctrl = TagPageController::from_session(&session);
        let reqs = tag_ctrl.open_tag(2, Some("nature".to_string())).unwrap();
        let _ = tag_ctrl.apply_metadata_response(
            &mut session,
            fetch_tag_metadata(&provider, reqs.metadata).await,
        );
        assert!(tag_ctrl.tag().is_some());

        let outcome =
            tag_ctrl.apply_page_response(&mut session, fetch_page(&provider, reqs.page).await);
        assert!(matches!(outcome, ApplyOutcome::Revoked(_)));
        assert_eq!(session.scope().kind, SessionKind::Guest);
        assert!(session.scope().generation > scope_a.generation);
        assert!(tag_ctrl.tag().is_none());
        assert!(tag_ctrl.items().is_empty());
        assert_eq!(tag_ctrl.metadata_status(), &TagMetadataStatus::Idle);
        assert_eq!(tag_ctrl.items_status(), &DiscoveryStatus::Idle);

        let scope_b = sign_in(&mut session, "carol");
        let mut up_ctrl = UploaderController::from_session(&session);
        let req = up_ctrl.open_uploader("artist").unwrap();
        let outcome = up_ctrl.apply_response(&mut session, fetch_page(&provider, req).await);
        assert!(matches!(outcome, ApplyOutcome::Revoked(_)));
        assert_eq!(session.scope(), SessionScope::guest(scope_b.generation + 1));
        assert_eq!(up_ctrl.scope(), &session.scope());
        assert!(up_ctrl.items().is_empty());
        assert_eq!(up_ctrl.status(), &DiscoveryStatus::Idle);
    }

    // 3. Guest 401 surfaces upstream restriction without revoking Guest or filtering results.
    {
        let mut session = guest_session();
        let guest = session.scope();
        let provider = FakeProvider::new(
            |id, _| Ok(sample_tag(id, "mixed")),
            |q, _| {
                if q.purity.is_some_and(|p| p.nsfw) {
                    return Err(ProviderError::Unauthorized);
                }
                Ok(SearchPage {
                    data: vec![
                        wallpaper("s", Purity::Sfw),
                        wallpaper("k", Purity::Sketchy),
                        wallpaper("n", Purity::Nsfw),
                    ],
                    meta: SearchMeta {
                        current_page: 1,
                        last_page: 1,
                        per_page: 3,
                        total: 3,
                        query: None,
                        seed: None,
                    },
                })
            },
        );

        let mut tag_ctrl = TagPageController::from_session(&session);
        let _ = tag_ctrl.open_tag(3, None).unwrap();
        let nsfw_req = tag_ctrl.set_purity(PurityMask::ALL).unwrap();
        assert_eq!(nsfw_req.query.purity, Some(PurityMask::ALL));
        assert_eq!(
            tag_ctrl.apply_page_response(&mut session, fetch_page(&provider, nsfw_req).await),
            ApplyOutcome::Failed(ProviderError::Unauthorized)
        );
        assert_eq!(session.scope(), guest);

        let sfw_req = tag_ctrl.set_purity(PurityMask::SFW_ONLY).unwrap();
        tag_ctrl.apply_page_response(&mut session, fetch_page(&provider, sfw_req).await);
        let tag_purities: Vec<Purity> = tag_ctrl.items().iter().map(|w| w.purity).collect();
        assert_eq!(
            tag_purities,
            vec![Purity::Sfw, Purity::Sketchy, Purity::Nsfw]
        );

        let mut up_ctrl = UploaderController::from_session(&session);
        let _ = up_ctrl.open_uploader("artist").unwrap();
        let up_nsfw = up_ctrl.set_purity(PurityMask::ALL).unwrap();
        assert_eq!(
            up_ctrl.apply_response(&mut session, fetch_page(&provider, up_nsfw).await),
            ApplyOutcome::Failed(ProviderError::Unauthorized)
        );
        assert_eq!(session.scope(), guest);

        let up_sfw = up_ctrl.set_purity(PurityMask::SFW_ONLY).unwrap();
        up_ctrl.apply_response(&mut session, fetch_page(&provider, up_sfw).await);
        let up_purities: Vec<Purity> = up_ctrl.items().iter().map(|w| w.purity).collect();
        assert_eq!(
            up_purities,
            vec![Purity::Sfw, Purity::Sketchy, Purity::Nsfw]
        );
    }
}

#[test]
fn profile_tag_handoffs_validate_urls_and_labels() {
    for action in ProfileTagUnsupportedAction::ALL {
        assert_eq!(action.label(), OPEN_ON_WALLHAVEN_LABEL);
        assert!(!action.title().is_empty());
        assert!(!action.explanation().is_empty());
    }

    assert_eq!(
        ProfileTagUnsupportedAction::user_profile_url("  alice_1  "),
        Ok("https://wallhaven.cc/user/alice_1".to_string())
    );
    for bad in ["", "   ", ".", "..", "a/b", "a?x=1", "a#frag", "a b"] {
        assert!(
            ProfileTagUnsupportedAction::user_profile_url(bad).is_err(),
            "bad username {bad:?} must fail"
        );
    }

    assert_eq!(
        ProfileTagUnsupportedAction::tag_directory_url(),
        WALLHAVEN_TAGS_URL
    );
    let dir = ProfileTagUnsupportedAction::tag_directory_hand_off();
    assert_eq!(dir.label, OPEN_ON_WALLHAVEN_LABEL);
    assert_eq!(dir.url, "https://wallhaven.cc/tags");

    assert_eq!(
        ProfileTagUnsupportedAction::tag_moderation_url(&WallpaperId::new("abc123")),
        Ok("https://wallhaven.cc/w/abc123".to_string())
    );
    for bad in [
        "", ".", "..", "a/b", "a\\b", "a?x=1", "a#frag", "a@b", "a b",
    ] {
        assert!(
            ProfileTagUnsupportedAction::tag_moderation_url(&WallpaperId::new(bad)).is_err(),
            "bad wallpaper id {bad:?} must fail"
        );
    }
}
