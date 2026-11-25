//! Discovery controller behavior against a scripted `FakeProvider` and a real `SessionManager`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use wallora_core::domain::{
    Category, CategoryMask, CollectionSummary, CredentialStore, Dimensions,
    InMemoryCredentialStore, Purity, PurityMask, QueryBuilder, SearchMeta, SearchPage, SearchQuery,
    SearchTerms, SecretString, SessionKind, SessionScope, SortOrder, Sorting, Tag, Thumbs,
    TopRange, UserSettings, UsernameVerification, Wallpaper, WallpaperId,
};
use wallora_core::provider::{Provider, ProviderError};
use wallora_core::services::{
    ApplyOutcome, DiscoveryController, DiscoveryFilters, DiscoveryStatus, DiscoveryView,
    PageRequest, SessionManager, SignInCommitOutcome, SignInProbe, fetch_page,
};
use wallora_core::storage::{SessionStorage, SqliteStore};

type Handler =
    dyn Fn(&SearchQuery, &SessionScope) -> Result<SearchPage, ProviderError> + Send + Sync;

/// Scripted provider that records every search it receives.
#[derive(Clone)]
struct FakeProvider {
    handler: Arc<Handler>,
    calls: Arc<Mutex<Vec<(SearchQuery, SessionScope)>>>,
}

impl FakeProvider {
    fn new(
        handler: impl Fn(&SearchQuery, &SessionScope) -> Result<SearchPage, ProviderError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            handler: Arc::new(handler),
            calls: Arc::default(),
        }
    }

    fn calls(&self) -> Vec<(SearchQuery, SessionScope)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Provider for FakeProvider {
    fn search(
        &self,
        q: SearchQuery,
        scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<SearchPage, ProviderError>> + Send + 'static {
        self.calls.lock().unwrap().push((q.clone(), scope.clone()));
        let result = (self.handler)(&q, &scope);
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
        _id: u64,
        _scope: SessionScope,
    ) -> impl std::future::Future<Output = Result<Tag, ProviderError>> + Send + 'static {
        std::future::ready(Err(ProviderError::NotFound))
    }
}

fn wallpaper(id: &str, purity: Purity) -> Wallpaper {
    Wallpaper {
        id: WallpaperId::new(id),
        url: format!("https://wallhaven.cc/w/{id}"),
        short_url: None,
        uploader: None,
        views: 0,
        favorites: 0,
        source: String::new(),
        purity,
        category: Category::General,
        dimension_x: 1920,
        dimension_y: 1080,
        resolution: "1920x1080".to_string(),
        ratio: "1.78".to_string(),
        file_size: 1,
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

fn page(ids: &[&str], current: u32, last: u32, seed: Option<&str>) -> SearchPage {
    SearchPage {
        data: ids.iter().map(|id| wallpaper(id, Purity::Sfw)).collect(),
        meta: SearchMeta {
            current_page: current,
            last_page: last,
            per_page: ids.len().max(1) as u32,
            total: u64::from(last) * 24,
            query: None,
            seed: seed.map(str::to_string),
        },
    }
}

fn requested_page(query: &SearchQuery) -> u32 {
    query.page.unwrap_or(1)
}

/// Three pages named `p{page}-a` / `p{page}-b`.
fn three_pages(query: &SearchQuery, _scope: &SessionScope) -> Result<SearchPage, ProviderError> {
    let n = requested_page(query);
    let a = format!("p{n}-a");
    let b = format!("p{n}-b");
    Ok(page(&[a.as_str(), b.as_str()], n, 3, None))
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("wallora-discovery-{}", uuid::Uuid::new_v4()));
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

/// A durable session so sign-in works; returns the temp dir that owns the database.
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
        api_key: SecretString::new("wh_synthetic_discovery_key"),
        user_settings: None,
        own_collections: Some(Vec::new()),
        verification: UsernameVerification::UnverifiableEmpty,
    };
    match session.commit_sign_in(probe).unwrap() {
        SignInCommitOutcome::Applied { scope, .. } => scope,
        other => panic!("unexpected sign-in outcome: {other:?}"),
    }
}

fn pair_map(query: &SearchQuery) -> std::collections::HashMap<&'static str, String> {
    query.to_query_pairs().into_iter().collect()
}

#[tokio::test]
async fn every_view_builds_documented_queries_with_session_defaults() {
    let mut c = DiscoveryController::new(SessionScope::guest(1), DiscoveryFilters::default());

    let latest = pair_map(&c.refresh().query);
    assert_eq!(latest["sorting"], "date_added");
    assert_eq!(latest["categories"], "111");
    assert_eq!(latest["purity"], "100");
    assert!(
        !latest.contains_key("q") && !latest.contains_key("page") && !latest.contains_key("seed")
    );

    let toplist = c
        .set_view(DiscoveryView::Toplist {
            range: TopRange::OneWeek,
        })
        .unwrap();
    let toplist = pair_map(&toplist.query);
    assert_eq!(toplist["sorting"], "toplist");
    assert_eq!(toplist["topRange"], "1w");

    let random = pair_map(&c.set_view(DiscoveryView::Random).unwrap().query);
    assert_eq!(random["sorting"], "random");
    assert!(
        !random.contains_key("seed"),
        "the first random page has no seed yet"
    );

    let filters = DiscoveryFilters {
        categories: CategoryMask {
            general: true,
            anime: false,
            people: true,
        },
        purity: PurityMask::SFW_AND_SKETCHY,
        atleast: Some(Dimensions::new(1920, 1080)),
        resolutions: vec![Dimensions::new(2560, 1440)],
        ratios: vec![wallora_core::domain::AspectRatio::new("16x9")],
        colors: Some("#336699".to_string()),
        sorting: Some(Sorting::Favorites),
        order: Some(SortOrder::Asc),
        top_range: None,
    };
    c.set_filters(filters).unwrap();
    let terms = SearchTerms::from_tokens(
        QueryBuilder::new()
            .tag("city")
            .unwrap()
            .exclude_tag("people")
            .unwrap()
            .tokens()
            .to_vec(),
    );
    c.set_terms(terms.clone()); // not in Search view yet: stored, no reload
    let search = pair_map(&c.set_view(DiscoveryView::Search).unwrap().query);
    assert_eq!(search["q"], "city -people");
    assert_eq!(search["sorting"], "favorites");
    assert_eq!(search["order"], "asc");
    assert_eq!(search["categories"], "101");
    assert_eq!(search["purity"], "110");
    assert_eq!(search["atleast"], "1920x1080");
    assert_eq!(search["resolutions"], "2560x1440");
    assert_eq!(search["ratios"], "16x9");
    assert_eq!(search["colors"], "336699");
    assert!(!search.contains_key("topRange"));

    // Raw text reaches the API exactly as typed.
    c.set_terms(SearchTerms::raw("  id:abc  -\"x ")).unwrap();
    assert_eq!(c.query_for_page(1).q.as_deref(), Some("  id:abc  -\"x "));

    // Toplist sorting inside Search takes its range from the filters, defaulting to a month.
    let mut toplist_search = c.filters().clone();
    toplist_search.sorting = Some(Sorting::Toplist);
    let request = c.set_filters(toplist_search).unwrap();
    assert_eq!(pair_map(&request.query)["topRange"], "1M");
}

#[tokio::test]
async fn superseded_responses_are_dropped_in_either_arrival_order() {
    let provider = FakeProvider::new(three_pages);
    let mut session = guest_session();
    let mut c = DiscoveryController::new(session.scope(), DiscoveryFilters::default());

    for old_first in [true, false] {
        let a = c.refresh();
        let b = c.set_view(DiscoveryView::Toplist {
            range: TopRange::OneDay,
        });
        let b = b.expect("view changed");
        assert!(b.generation > a.generation);
        let (ra, rb) = (
            fetch_page(&provider, a).await,
            fetch_page(&provider, b).await,
        );
        let (first, second) = if old_first { (ra, rb) } else { (rb, ra) };
        let (old, new) = if old_first {
            (first, second)
        } else {
            (second, first)
        };
        if old_first {
            assert_eq!(c.apply_response(&mut session, old), ApplyOutcome::Stale);
            assert_eq!(
                c.apply_response(&mut session, new),
                ApplyOutcome::Applied { added: 2 }
            );
        } else {
            assert_eq!(
                c.apply_response(&mut session, new),
                ApplyOutcome::Applied { added: 2 }
            );
            assert_eq!(c.apply_response(&mut session, old), ApplyOutcome::Stale);
        }
        assert_eq!(c.items().len(), 2);
        // Return to Latest for the next iteration.
        c.set_view(DiscoveryView::Latest).unwrap();
    }
}

#[tokio::test]
async fn query_filter_and_refresh_changes_each_supersede_in_flight_work() {
    let provider = FakeProvider::new(three_pages);
    let mut session = guest_session();
    let mut c = DiscoveryController::new(session.scope(), DiscoveryFilters::default());
    c.set_view(DiscoveryView::Search).unwrap();

    let mut generations = vec![c.generation()];
    let mut stale: Vec<PageRequest> = Vec::new();
    let mut current = c.refresh();
    let mut supersede =
        |c: &mut DiscoveryController, next: PageRequest, current: &mut PageRequest| {
            stale.push(std::mem::replace(current, next));
            generations.push(c.generation());
        };

    let next = c
        .set_terms(SearchTerms::raw("cats"))
        .expect("terms changed");
    supersede(&mut c, next, &mut current);
    let mut filters = c.filters().clone();
    filters.purity = PurityMask::SFW_AND_SKETCHY;
    let next = c.set_filters(filters).expect("filters changed");
    supersede(&mut c, next, &mut current);
    let next = c.refresh();
    supersede(&mut c, next, &mut current);

    assert!(
        generations.windows(2).all(|w| w[0] < w[1]),
        "{generations:?}"
    );
    // Setting the same value again changes nothing and does not bump the generation.
    let before = c.generation();
    assert!(c.set_terms(SearchTerms::raw("cats")).is_none());
    assert!(c.set_view(DiscoveryView::Search).is_none());
    assert!(c.set_filters(c.filters().clone()).is_none());
    assert_eq!(c.generation(), before);

    for old in stale {
        let response = fetch_page(&provider, old).await;
        assert_eq!(
            c.apply_response(&mut session, response),
            ApplyOutcome::Stale
        );
    }
    assert!(c.items().is_empty());
    assert_eq!(*c.status(), DiscoveryStatus::Loading);
    let response = fetch_page(&provider, current).await;
    assert_eq!(
        c.apply_response(&mut session, response),
        ApplyOutcome::Applied { added: 2 }
    );
}

#[tokio::test]
async fn duplicate_page_fetches_are_deduplicated() {
    let provider = FakeProvider::new(three_pages);
    let mut session = guest_session();
    let mut c = DiscoveryController::new(session.scope(), DiscoveryFilters::default());

    let first = c.refresh();
    assert_eq!(first.page, 1);
    assert!(c.request_page(1).is_none(), "page 1 is already in flight");
    assert!(c.request_next_page().is_none(), "nothing to scroll to yet");
    let response = fetch_page(&provider, first).await;
    c.apply_response(&mut session, response.clone());
    assert!(c.request_page(1).is_none(), "page 1 is already fetched");
    // A late duplicate delivery of the same response adds nothing.
    assert_eq!(
        c.apply_response(&mut session, response),
        ApplyOutcome::Stale
    );
    assert_eq!(c.items().len(), 2);

    let second = c.request_next_page().expect("page 2");
    assert_eq!(second.page, 2);
    assert!(c.request_page(2).is_none(), "page 2 is already in flight");
    assert!(c.request_page(3).is_none(), "only one page flies at a time");
    assert!(c.request_next_page().is_none());
    let response = fetch_page(&provider, second).await;
    c.apply_response(&mut session, response.clone());
    assert_eq!(
        c.apply_response(&mut session, response),
        ApplyOutcome::Stale
    );
    assert_eq!(c.items().len(), 4);

    let fetched: Vec<u32> = provider
        .calls()
        .iter()
        .map(|(query, _)| requested_page(query))
        .collect();
    assert_eq!(fetched, vec![1, 2], "each page reached the provider once");
}

#[tokio::test]
async fn paging_stops_at_last_page() {
    let provider = FakeProvider::new(three_pages);
    let mut session = guest_session();
    let mut c = DiscoveryController::new(session.scope(), DiscoveryFilters::default());

    let mut request = Some(c.refresh());
    let mut pages = Vec::new();
    while let Some(next) = request {
        pages.push(next.page);
        let response = fetch_page(&provider, next).await;
        c.apply_response(&mut session, response);
        request = c.request_next_page();
    }
    assert_eq!(pages, vec![1, 2, 3]);
    assert_eq!(c.last_page(), Some(3));
    assert!(!c.can_load_more());
    assert!(c.request_next_page().is_none());
    assert!(c.request_page(4).is_none());
    assert_eq!(c.items().len(), 6);
    assert_eq!(*c.status(), DiscoveryStatus::Ready);
    assert_eq!(provider.calls().len(), 3, "no fetch beyond last_page");

    // A single-page listing never offers more, and an empty one reports Empty.
    let single = FakeProvider::new(|_, _| Ok(page(&["only"], 1, 1, None)));
    let mut c = DiscoveryController::new(session.scope(), DiscoveryFilters::default());
    let response = fetch_page(&single, c.refresh()).await;
    c.apply_response(&mut session, response);
    assert!(!c.can_load_more() && c.request_next_page().is_none());

    let empty = FakeProvider::new(|_, _| Ok(page(&[], 1, 1, None)));
    let response = fetch_page(&empty, c.refresh()).await;
    c.apply_response(&mut session, response);
    assert_eq!(*c.status(), DiscoveryStatus::Empty);
    assert!(!c.can_load_more());
}

#[tokio::test]
async fn random_keeps_its_seed_across_pages_and_rotates_on_refresh() {
    let counter = Arc::new(Mutex::new(0u32));
    let provider = {
        let counter = Arc::clone(&counter);
        FakeProvider::new(move |query, _| {
            let mut guard = counter.lock().unwrap();
            *guard += 1;
            // Upstream invents a seed for the first page and echoes the one it was given.
            let seed = query
                .seed
                .clone()
                .unwrap_or_else(|| format!("seed{}", *guard));
            Ok(page(
                &[format!("r{}-a", *guard).as_str()],
                requested_page(query),
                4,
                Some(seed.as_str()),
            ))
        })
    };
    let mut session = guest_session();
    let mut c = DiscoveryController::new(session.scope(), DiscoveryFilters::default());
    c.set_view(DiscoveryView::Random).unwrap();

    let first = c.refresh();
    assert!(first.query.seed.is_none());
    let response = fetch_page(&provider, first).await;
    c.apply_response(&mut session, response);
    assert_eq!(c.seed(), Some("seed1"));

    for expected_page in [2, 3] {
        let next = c.request_next_page().expect("more pages");
        assert_eq!(next.page, expected_page);
        assert_eq!(next.query.seed.as_deref(), Some("seed1"));
        assert_eq!(pair_map(&next.query)["seed"], "seed1");
        let response = fetch_page(&provider, next).await;
        c.apply_response(&mut session, response);
        assert_eq!(c.seed(), Some("seed1"), "the first seed is kept");
    }

    // An explicit refresh rotates the seed: none is sent and a new one is adopted.
    let refreshed = c.refresh();
    assert!(refreshed.query.seed.is_none());
    assert!(c.seed().is_none());
    let response = fetch_page(&provider, refreshed).await;
    c.apply_response(&mut session, response);
    assert_eq!(c.seed(), Some("seed4"));

    // Changing a filter is also a new random listing.
    let mut filters = c.filters().clone();
    filters.purity = PurityMask::SFW_AND_SKETCHY;
    let request = c.set_filters(filters).unwrap();
    assert!(request.query.seed.is_none() && c.seed().is_none());
}

#[tokio::test]
async fn seed_is_ignored_outside_random_listings() {
    let provider =
        FakeProvider::new(|query, _| Ok(page(&["a"], requested_page(query), 2, Some("leak"))));
    let mut session = guest_session();
    let mut c = DiscoveryController::new(session.scope(), DiscoveryFilters::default());
    let response = fetch_page(&provider, c.refresh()).await;
    c.apply_response(&mut session, response);
    assert!(c.seed().is_none());
    assert!(c.request_next_page().unwrap().query.seed.is_none());

    // Search sorted randomly is a random listing and keeps its seed too.
    let mut filters = c.filters().clone();
    filters.sorting = Some(Sorting::Random);
    c.set_view(DiscoveryView::Search).unwrap();
    let request = c.set_filters(filters).unwrap();
    let response = fetch_page(&provider, request).await;
    c.apply_response(&mut session, response);
    assert_eq!(c.seed(), Some("leak"));
    assert_eq!(
        c.request_next_page().unwrap().query.seed.as_deref(),
        Some("leak")
    );
}

#[tokio::test]
async fn failures_surface_and_retry_requests_the_failed_page_again() {
    let fail_page_two = Arc::new(Mutex::new(true));
    let provider = {
        let flag = Arc::clone(&fail_page_two);
        FakeProvider::new(move |query, scope| {
            if requested_page(query) == 2 && *flag.lock().unwrap() {
                Err(ProviderError::Network)
            } else {
                three_pages(query, scope)
            }
        })
    };
    let mut session = guest_session();
    let mut c = DiscoveryController::new(session.scope(), DiscoveryFilters::default());
    let response = fetch_page(&provider, c.refresh()).await;
    c.apply_response(&mut session, response);

    let second = c.request_next_page().unwrap();
    let response = fetch_page(&provider, second).await;
    assert_eq!(
        c.apply_response(&mut session, response),
        ApplyOutcome::Failed(ProviderError::Network)
    );
    assert_eq!(
        *c.status(),
        DiscoveryStatus::Failed {
            page: 2,
            error: ProviderError::Network
        }
    );
    assert_eq!(c.items().len(), 2, "earlier pages stay visible");
    assert!(
        c.request_next_page().is_none(),
        "scrolling does not hammer a failing page"
    );

    *fail_page_two.lock().unwrap() = false;
    let retry = c.retry().expect("retry the failed page");
    assert_eq!(retry.page, 2);
    assert!(c.retry().is_none());
    let response = fetch_page(&provider, retry).await;
    assert_eq!(
        c.apply_response(&mut session, response),
        ApplyOutcome::Applied { added: 2 }
    );
    assert_eq!(*c.status(), DiscoveryStatus::Ready);
    assert_eq!(c.items().len(), 4);

    // A first-page failure retries as a first-page load.
    let down = FakeProvider::new(|_, _| Err(ProviderError::Timeout));
    let response = fetch_page(&down, c.refresh()).await;
    assert_eq!(
        c.apply_response(&mut session, response),
        ApplyOutcome::Failed(ProviderError::Timeout)
    );
    assert_eq!(c.retry().unwrap().page, 1);
}

#[tokio::test]
async fn guest_401_shows_upstream_restriction_without_filtering_or_revocation() {
    // Upstream refuses NSFW for guests and otherwise returns every purity unfiltered.
    let provider = FakeProvider::new(|query, _| {
        if query.purity.is_some_and(|purity| purity.nsfw) {
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
    });
    let mut session = guest_session();
    let guest = session.scope();
    let filters = DiscoveryFilters {
        purity: PurityMask::ALL,
        ..DiscoveryFilters::default()
    };
    let mut c = DiscoveryController::new(guest.clone(), filters);

    let request = c.refresh();
    assert_eq!(
        request.query.purity,
        Some(PurityMask::ALL),
        "sent as selected, not clamped"
    );
    let response = fetch_page(&provider, request).await;
    assert_eq!(
        c.apply_response(&mut session, response),
        ApplyOutcome::Failed(ProviderError::Unauthorized)
    );
    assert_eq!(session.scope(), guest, "a guest 401 is not a revocation");
    assert!(matches!(c.status(), DiscoveryStatus::Failed { .. }));

    // Whatever upstream returns is shown as is, including mixed purities under an SFW filter.
    let mut filters = c.filters().clone();
    filters.purity = PurityMask::SFW_ONLY;
    let request = c.set_filters(filters).unwrap();
    assert_eq!(request.query.purity, Some(PurityMask::SFW_ONLY));
    let response = fetch_page(&provider, request).await;
    c.apply_response(&mut session, response);
    let purities: Vec<Purity> = c.items().iter().map(|w| w.purity).collect();
    assert_eq!(purities, vec![Purity::Sfw, Purity::Sketchy, Purity::Nsfw]);
}

#[tokio::test]
async fn account_401_revokes_the_session_and_clears_the_listing() {
    let (_dir, mut session) = durable_session();
    let account_scope = sign_in(&mut session, "alice");
    assert!(matches!(account_scope.kind, SessionKind::Account(_)));

    let revoked = Arc::new(Mutex::new(false));
    let provider = {
        let revoked = Arc::clone(&revoked);
        FakeProvider::new(move |query, scope| {
            assert!(matches!(scope.kind, SessionKind::Account(_)));
            if *revoked.lock().unwrap() {
                Err(ProviderError::Unauthorized)
            } else {
                three_pages(query, scope)
            }
        })
    };
    let mut c = DiscoveryController::new(account_scope.clone(), DiscoveryFilters::default());
    let response = fetch_page(&provider, c.refresh()).await;
    c.apply_response(&mut session, response);
    assert_eq!(c.items().len(), 2);

    *revoked.lock().unwrap() = true;
    let next = c.request_next_page().unwrap();
    let response = fetch_page(&provider, next).await;
    let outcome = c.apply_response(&mut session, response);
    assert!(matches!(outcome, ApplyOutcome::Revoked(_)), "{outcome:?}");
    assert_eq!(
        session.scope(),
        SessionScope::guest(account_scope.generation + 1)
    );
    assert_eq!(c.scope(), &session.scope());
    assert!(
        c.items().is_empty(),
        "account listings are cleared on revocation"
    );
    assert_eq!(*c.status(), DiscoveryStatus::Idle);
    assert!(!c.can_load_more());
    assert!(session.active_account().is_none());

    // The next listing is a guest listing.
    let again = c.refresh();
    assert_eq!(again.scope.kind, SessionKind::Guest);
}

#[tokio::test]
async fn scope_changes_discard_account_results_and_ignore_late_responses() {
    let (_dir, mut session) = durable_session();
    let guest_scope = session.scope();
    let mut c = DiscoveryController::new(guest_scope.clone(), DiscoveryFilters::default());
    let provider = FakeProvider::new(three_pages);

    let guest_request = c.refresh();
    let account_scope = sign_in(&mut session, "bob");
    let account_request = c.set_scope(account_scope.clone()).expect("scope changed");
    assert_eq!(account_request.scope, account_scope);
    assert!(account_request.generation > guest_request.generation);
    assert!(c.set_scope(account_scope.clone()).is_none());

    // The guest response arrives after sign-in and is dropped by both session and controller.
    let late = fetch_page(&provider, guest_request).await;
    assert_eq!(c.apply_response(&mut session, late), ApplyOutcome::Stale);
    let response = fetch_page(&provider, account_request).await;
    assert_eq!(
        c.apply_response(&mut session, response),
        ApplyOutcome::Applied { added: 2 }
    );

    // Signing out clears everything shown for the account at once.
    session.logout();
    let guest_again = c.set_scope(session.scope()).expect("scope changed");
    assert!(c.items().is_empty());
    assert_eq!(guest_again.scope.kind, SessionKind::Guest);
    // Every request reached the provider under exactly the scope it was issued for.
    let scopes: Vec<SessionScope> = provider
        .calls()
        .into_iter()
        .map(|(_, scope)| scope)
        .collect();
    assert_eq!(scopes, vec![guest_scope, account_scope]);
}

#[tokio::test]
async fn items_are_unique_across_pages() {
    // Newly uploaded wallpapers shift later pages, so page 2 repeats the last id of page 1.
    let provider = FakeProvider::new(|query, _| match requested_page(query) {
        1 => Ok(page(&["a", "b"], 1, 2, None)),
        _ => Ok(page(&["b", "c"], 2, 2, None)),
    });
    let mut session = guest_session();
    let mut c = DiscoveryController::new(session.scope(), DiscoveryFilters::default());
    let response = fetch_page(&provider, c.refresh()).await;
    c.apply_response(&mut session, response);
    let response = fetch_page(&provider, c.request_next_page().unwrap()).await;
    assert_eq!(
        c.apply_response(&mut session, response),
        ApplyOutcome::Applied { added: 1 }
    );
    let ids: Vec<&str> = c.items().iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, vec!["a", "b", "c"]);
}
