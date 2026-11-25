//! GUI-free discovery and search controller (Task 5).
//!
//! The controller is a plain state machine. It never performs I/O itself: state changes return a
//! [`PageRequest`] describing the page to fetch, the caller runs it with [`fetch_page`] (or any
//! [`Provider`]), and the [`PageResponse`] is fed back through [`DiscoveryController::apply_response`].
//!
//! Guarantees:
//! - Every listing (view, query, filters, session scope, or explicit refresh) gets a new,
//!   monotonically increasing generation; responses from earlier generations are dropped.
//! - A page is never requested twice (fetched or in flight) and paging stops at `last_page`.
//! - `Random` keeps the upstream `meta.seed` across pages and drops it on an explicit refresh.
//! - Only documented API parameters are emitted. `sorting=hot` and any AI-art parameter are not
//!   part of the public API; see [`UnsupportedFeature`] for the labelled browser hand-offs.
//! - Purity and categories are sent exactly as selected. Results are never filtered client-side.

use std::collections::{BTreeSet, HashSet};

use crate::domain::{
    AspectRatio, BrowseDefaults, CategoryMask, Dimensions, PurityMask, RequestPriority, SearchPage,
    SearchQuery, SearchTerms, SessionKind, SessionScope, SortOrder, Sorting, TopRange,
    UserSettings, Wallpaper, WallpaperId,
};
use crate::provider::{Provider, ProviderError};
use crate::services::session::{ScopedResponse, SessionManager, SessionNotice};

/// Label used for every browser hand-off to a feature the public API does not cover.
pub const OPEN_ON_WALLHAVEN_LABEL: &str = "Open on wallhaven.cc";

/// Site features without a documented API route; Wallora shows a hand-off instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnsupportedFeature {
    /// The website's Hot listing (`sorting=hot` is not a documented API value).
    HotSorting,
    /// The website's AI-art filter (no documented API parameter).
    AiArtFilter,
}

/// Remaining website-only features without a documented public API route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SiteGapAction {
    /// Website homepage curated sections, announcements, and site stats (`https://wallhaven.cc/`).
    Homepage,
    /// Website tag directory, categories, and directory sorts (`https://wallhaven.cc/tags`).
    TagDirectory,
    /// Website Few Tags / Untagged queue (`https://wallhaven.cc/untagged`).
    UntaggedQueue,
    /// Website wallpaper upload flow (`https://wallhaven.cc/upload`).
    UploadWallpaper,
    /// Website forums for reading and posting discussions (`https://wallhaven.cc/forums`).
    Forums,
}

/// Unsupported wallpaper-detail actions that hand off to `https://wallhaven.cc/w/{id}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DetailUnsupportedAction {
    /// Website Crop & Scale Download tool (no documented API route).
    CropAndScale,
    /// Website image-based "Search for similar" action (distinct from tag-based `like:ID`).
    ImageSimilar,
}

impl DetailUnsupportedAction {
    pub const ALL: [Self; 2] = [Self::CropAndScale, Self::ImageSimilar];

    pub fn title(self) -> &'static str {
        match self {
            Self::CropAndScale => "Crop & Scale Download",
            Self::ImageSimilar => "Search for similar from wallpaper image",
        }
    }

    pub fn label(self) -> &'static str {
        OPEN_ON_WALLHAVEN_LABEL
    }

    pub fn explanation(self) -> &'static str {
        match self {
            Self::CropAndScale => {
                "Crop & Scale is a website-only tool with no documented API route."
            }
            Self::ImageSimilar => {
                "Image-based similarity search has no documented API route (use More like this for tag similarity)."
            }
        }
    }

    pub fn wallpaper_url(self, id: &WallpaperId) -> Result<String, &'static str> {
        let raw = id.as_str();
        if raw.is_empty()
            || raw == "."
            || raw == ".."
            || raw.contains(['/', '\\', '?', '#', '@'])
            || raw.chars().any(char::is_whitespace)
        {
            return Err("invalid wallpaper id for wallhaven.cc link");
        }
        Ok(format!("https://wallhaven.cc/w/{raw}"))
    }
}

/// A labelled link that opens the website in the user's browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandOff {
    pub label: &'static str,
    pub url: &'static str,
}

impl UnsupportedFeature {
    pub const ALL: [Self; 2] = [Self::HotSorting, Self::AiArtFilter];

    pub fn title(self) -> &'static str {
        match self {
            Self::HotSorting => "Hot",
            Self::AiArtFilter => "AI-Art Filter",
        }
    }

    pub fn hand_off(self) -> HandOff {
        HandOff {
            label: OPEN_ON_WALLHAVEN_LABEL,
            url: match self {
                Self::HotSorting => "https://wallhaven.cc/hot",
                Self::AiArtFilter => "https://wallhaven.cc/search",
            },
        }
    }

    pub fn explanation(self) -> &'static str {
        match self {
            Self::HotSorting => {
                "Hot is not available through the public Wallhaven API. Open it on wallhaven.cc."
            }
            Self::AiArtFilter => {
                "The AI-art filter has no documented API parameter. Set it on wallhaven.cc."
            }
        }
    }
}

impl SiteGapAction {
    pub const ALL: [Self; 5] = [
        Self::Homepage,
        Self::TagDirectory,
        Self::UntaggedQueue,
        Self::UploadWallpaper,
        Self::Forums,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::Homepage => "Homepage",
            Self::TagDirectory => "Tag Directory",
            Self::UntaggedQueue => "Few Tags / Untagged",
            Self::UploadWallpaper => "Upload",
            Self::Forums => "Forums",
        }
    }

    pub fn label(self) -> &'static str {
        OPEN_ON_WALLHAVEN_LABEL
    }

    pub fn url(self) -> &'static str {
        match self {
            Self::Homepage => "https://wallhaven.cc/",
            Self::TagDirectory => "https://wallhaven.cc/tags",
            Self::UntaggedQueue => "https://wallhaven.cc/untagged",
            Self::UploadWallpaper => "https://wallhaven.cc/upload",
            Self::Forums => "https://wallhaven.cc/forums",
        }
    }

    pub fn hand_off(self) -> HandOff {
        HandOff {
            label: OPEN_ON_WALLHAVEN_LABEL,
            url: self.url(),
        }
    }

    pub fn explanation(self) -> &'static str {
        match self {
            Self::Homepage => {
                "Curated homepage sections, announcements, and site stats have no public API route."
            }
            Self::TagDirectory => {
                "Browse the full tag directory by category or sort on wallhaven.cc (no public API route)."
            }
            Self::UntaggedQueue => "The Few Tags / Untagged queue has no documented API route.",
            Self::UploadWallpaper => {
                "Uploading wallpapers is a website-only write action with no public API route."
            }
            Self::Forums => {
                "Reading and posting in the Wallhaven forums has no documented API route."
            }
        }
    }
}

/// Supported listing views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiscoveryView {
    Latest,
    Toplist { range: TopRange },
    Random,
    Search,
}

impl DiscoveryView {
    pub fn label(self) -> &'static str {
        match self {
            Self::Latest => "Latest",
            Self::Toplist { .. } => "Toplist",
            Self::Random => "Random",
            Self::Search => "Search",
        }
    }
}

/// Documented search filters. Sorting and order apply to `Search`; the other views define their
/// own sorting. `order` is passed through for every view when set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryFilters {
    pub categories: CategoryMask,
    pub purity: PurityMask,
    pub atleast: Option<Dimensions>,
    pub resolutions: Vec<Dimensions>,
    pub ratios: Vec<AspectRatio>,
    pub colors: Option<String>,
    pub sorting: Option<Sorting>,
    pub order: Option<SortOrder>,
    /// Range for `Search` with `sorting=toplist` (defaults to one month).
    pub top_range: Option<TopRange>,
}

impl DiscoveryFilters {
    /// Seeds filters from the session's supported upstream defaults.
    pub fn from_defaults(defaults: &BrowseDefaults) -> Self {
        Self {
            categories: defaults.categories,
            purity: defaults.purity,
            atleast: None,
            resolutions: defaults.resolutions.clone(),
            ratios: defaults.aspect_ratios.clone(),
            colors: None,
            sorting: None,
            order: None,
            top_range: None,
        }
    }
}

impl Default for DiscoveryFilters {
    fn default() -> Self {
        Self::from_defaults(&BrowseDefaults::guest())
    }
}

/// Purity levels the UI should offer for a session (advisory; nothing is clamped).
///
/// Guests and accounts are offered SFW and Sketchy (the audited guest controls); NSFW is offered
/// only when the signed-in account's upstream settings enable it. If a caller still selects a
/// level upstream refuses, the request is sent as selected and upstream's answer is shown.
pub fn selectable_purity(scope: &SessionScope, settings: Option<&UserSettings>) -> PurityMask {
    let nsfw = match (&scope.kind, settings) {
        (SessionKind::Account(_), Some(settings)) => settings.purity_mask().nsfw,
        _ => false,
    };
    PurityMask {
        sfw: true,
        sketchy: true,
        nsfw,
    }
}

/// One page to fetch for the current listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageRequest {
    pub generation: u64,
    pub scope: SessionScope,
    pub page: u32,
    pub query: SearchQuery,
}

/// The outcome of fetching a [`PageRequest`], carrying everything needed to validate it later.
#[derive(Debug, Clone)]
pub struct PageResponse {
    pub generation: u64,
    pub scope: SessionScope,
    pub page: u32,
    pub result: Result<SearchPage, ProviderError>,
}

/// Runs `request` against `provider`. The scope travels with the request and the response.
pub async fn fetch_page<P: Provider>(provider: &P, request: PageRequest) -> PageResponse {
    let PageRequest {
        generation,
        scope,
        page,
        query,
    } = request;
    let result = provider.search(query, scope.clone()).await;
    PageResponse {
        generation,
        scope,
        page,
        result,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryStatus {
    /// Nothing has been requested for the current listing.
    Idle,
    /// The first page is loading.
    Loading,
    /// At least one page is shown; more may be loading.
    Ready,
    /// The first page succeeded but contained no wallpapers.
    Empty,
    /// The most recent request failed; [`DiscoveryController::retry`] re-requests it.
    Failed { page: u32, error: ProviderError },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The page was added to the listing.
    Applied { added: usize },
    /// The request failed upstream (including a guest's 401); the status shows the error.
    Failed(ProviderError),
    /// The response belonged to an older generation or another scope, or was a duplicate.
    Stale,
    /// The active account's key was rejected. The session signed out, the listing was cleared and
    /// the controller now targets the new guest scope; call [`DiscoveryController::refresh`].
    Revoked(SessionNotice),
}

/// Discovery listing state for one view.
#[derive(Debug, Clone)]
pub struct DiscoveryController {
    view: DiscoveryView,
    terms: SearchTerms,
    filters: DiscoveryFilters,
    scope: SessionScope,
    generation: u64,
    items: Vec<Wallpaper>,
    seen: HashSet<WallpaperId>,
    fetched_pages: BTreeSet<u32>,
    in_flight: Option<u32>,
    last_page: Option<u32>,
    total: Option<u64>,
    seed: Option<String>,
    status: DiscoveryStatus,
}

impl DiscoveryController {
    pub fn new(scope: SessionScope, filters: DiscoveryFilters) -> Self {
        Self::with_view(scope, DiscoveryView::Latest, filters)
    }

    pub fn with_view(scope: SessionScope, view: DiscoveryView, filters: DiscoveryFilters) -> Self {
        Self {
            view,
            terms: SearchTerms::default(),
            filters,
            scope,
            generation: 0,
            items: Vec::new(),
            seen: HashSet::new(),
            fetched_pages: BTreeSet::new(),
            in_flight: None,
            last_page: None,
            total: None,
            seed: None,
            status: DiscoveryStatus::Idle,
        }
    }

    /// Resets the controller to `Idle` for a new session scope and defaults, bumping generation
    /// and discarding any prior items, seed, or in-flight state.
    pub fn reset_for_scope(
        &mut self,
        scope: SessionScope,
        view: DiscoveryView,
        filters: DiscoveryFilters,
        clear_terms: bool,
    ) {
        self.view = view;
        self.filters = filters;
        if clear_terms {
            self.terms = SearchTerms::default();
        }
        self.discard_for_scope(scope);
    }

    pub fn view(&self) -> DiscoveryView {
        self.view
    }

    pub fn terms(&self) -> &SearchTerms {
        &self.terms
    }

    pub fn filters(&self) -> &DiscoveryFilters {
        &self.filters
    }

    pub fn scope(&self) -> &SessionScope {
        &self.scope
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn items(&self) -> &[Wallpaper] {
        &self.items
    }

    pub fn status(&self) -> &DiscoveryStatus {
        &self.status
    }

    pub fn seed(&self) -> Option<&str> {
        self.seed.as_deref()
    }

    pub fn last_page(&self) -> Option<u32> {
        self.last_page
    }

    pub fn total(&self) -> Option<u64> {
        self.total
    }

    pub fn fetched_pages(&self) -> impl Iterator<Item = u32> + '_ {
        self.fetched_pages.iter().copied()
    }

    pub fn is_loading(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Whether another page can be requested (infinite scroll trigger).
    pub fn can_load_more(&self) -> bool {
        self.status == DiscoveryStatus::Ready
            && self.in_flight.is_none()
            && match (self.fetched_pages.last(), self.last_page) {
                (Some(&highest), Some(last)) => highest < last,
                _ => false,
            }
    }

    /// Sorting in effect for the current view.
    fn effective_sorting(&self) -> Option<Sorting> {
        match self.view {
            DiscoveryView::Latest => Some(Sorting::DateAdded),
            DiscoveryView::Toplist { .. } => Some(Sorting::Toplist),
            DiscoveryView::Random => Some(Sorting::Random),
            DiscoveryView::Search => self.filters.sorting,
        }
    }

    /// The documented query for `page` of the current listing.
    pub fn query_for_page(&self, page: u32) -> SearchQuery {
        let sorting = self.effective_sorting();
        let top_range = match self.view {
            DiscoveryView::Toplist { range } => Some(range),
            DiscoveryView::Search if sorting == Some(Sorting::Toplist) => {
                Some(self.filters.top_range.unwrap_or(TopRange::OneMonth))
            }
            _ => None,
        };
        let q = match self.view {
            DiscoveryView::Search => self.terms.to_param(),
            _ => None,
        };
        SearchQuery {
            q,
            categories: Some(self.filters.categories),
            purity: Some(self.filters.purity),
            sorting,
            order: self.filters.order,
            top_range,
            atleast: self.filters.atleast,
            resolutions: self.filters.resolutions.clone(),
            ratios: self.filters.ratios.clone(),
            colors: self.filters.colors.clone(),
            page: (page > 1).then_some(page),
            seed: if sorting == Some(Sorting::Random) {
                self.seed.clone()
            } else {
                None
            },
            priority: if page <= 1 {
                RequestPriority::Interactive
            } else {
                RequestPriority::Visible
            },
        }
    }

    /// Starts a new listing: new generation, nothing shown, page 1 in flight.
    fn reload(&mut self) -> PageRequest {
        self.generation = self.generation.saturating_add(1);
        self.items.clear();
        self.seen.clear();
        self.fetched_pages.clear();
        self.last_page = None;
        self.total = None;
        self.seed = None;
        self.status = DiscoveryStatus::Loading;
        self.in_flight = Some(1);
        self.page_request(1)
    }

    fn page_request(&self, page: u32) -> PageRequest {
        PageRequest {
            generation: self.generation,
            scope: self.scope.clone(),
            page,
            query: self.query_for_page(page),
        }
    }

    /// Loads (or reloads) page 1. On `Random` this discards the seed, so the listing reshuffles.
    pub fn refresh(&mut self) -> PageRequest {
        self.reload()
    }

    pub fn set_view(&mut self, view: DiscoveryView) -> Option<PageRequest> {
        if self.view == view {
            return None;
        }
        self.view = view;
        Some(self.reload())
    }

    pub fn set_terms(&mut self, terms: SearchTerms) -> Option<PageRequest> {
        if self.terms == terms {
            return None;
        }
        self.terms = terms;
        // Terms only matter to Search; other views keep their listing.
        (self.view == DiscoveryView::Search).then(|| self.reload())
    }

    pub fn set_filters(&mut self, filters: DiscoveryFilters) -> Option<PageRequest> {
        if self.filters == filters {
            return None;
        }
        self.filters = filters;
        Some(self.reload())
    }

    /// Retargets the listing after a session change (sign-in, sign-out, switch, revocation).
    /// Everything shown for the old scope is discarded immediately.
    pub fn set_scope(&mut self, scope: SessionScope) -> Option<PageRequest> {
        if self.scope == scope {
            return None;
        }
        self.scope = scope;
        Some(self.reload())
    }

    /// Requests `page` unless it is page 0, already fetched, in flight, past `last_page`, or
    /// another page is already in flight.
    pub fn request_page(&mut self, page: u32) -> Option<PageRequest> {
        if page == 0
            || self.fetched_pages.contains(&page)
            || self.in_flight.is_some()
            || self.last_page.is_some_and(|last| page > last)
        {
            return None;
        }
        self.in_flight = Some(page);
        if self.status == DiscoveryStatus::Idle {
            self.status = DiscoveryStatus::Loading;
        }
        Some(self.page_request(page))
    }

    /// Requests the page after the highest fetched one (infinite scroll). `None` while a request
    /// is in flight, after a failure (use [`retry`](Self::retry)), or once `last_page` is reached.
    pub fn request_next_page(&mut self) -> Option<PageRequest> {
        if !self.can_load_more() {
            return None;
        }
        let next = self.fetched_pages.last().copied()?.checked_add(1)?;
        self.request_page(next)
    }

    /// Re-requests the page that failed.
    pub fn retry(&mut self) -> Option<PageRequest> {
        let DiscoveryStatus::Failed { page, .. } = self.status else {
            return None;
        };
        self.status = if self.fetched_pages.is_empty() {
            DiscoveryStatus::Loading
        } else {
            DiscoveryStatus::Ready
        };
        self.request_page(page)
    }

    /// Applies a fetched page after validating scope, session state and generation.
    ///
    /// The session sees every response first: a stale scope is dropped, and a `401` on an active
    /// account revokes it. A guest `401` is upstream's restriction and is shown as an error.
    pub fn apply_response(
        &mut self,
        session: &mut SessionManager,
        response: PageResponse,
    ) -> ApplyOutcome {
        let PageResponse {
            generation,
            scope,
            page,
            result,
        } = response;
        match session.handle_provider_result(&scope, result) {
            ScopedResponse::Stale => ApplyOutcome::Stale,
            ScopedResponse::Revoked(notice) => {
                self.discard_for_scope(session.scope());
                ApplyOutcome::Revoked(notice)
            }
            ScopedResponse::Ready(result) => {
                if generation != self.generation
                    || scope != self.scope
                    || self.in_flight != Some(page)
                {
                    return ApplyOutcome::Stale;
                }
                self.in_flight = None;
                match result {
                    Ok(page_data) => self.apply_page(page, page_data),
                    Err(error) => {
                        self.status = DiscoveryStatus::Failed {
                            page,
                            error: error.clone(),
                        };
                        ApplyOutcome::Failed(error)
                    }
                }
            }
        }
    }

    fn discard_for_scope(&mut self, scope: SessionScope) {
        self.scope = scope;
        self.generation = self.generation.saturating_add(1);
        self.items.clear();
        self.seen.clear();
        self.fetched_pages.clear();
        self.in_flight = None;
        self.last_page = None;
        self.total = None;
        self.seed = None;
        self.status = DiscoveryStatus::Idle;
    }

    fn apply_page(&mut self, page: u32, data: SearchPage) -> ApplyOutcome {
        self.fetched_pages.insert(page);
        self.last_page = Some(data.meta.last_page);
        self.total = Some(data.meta.total);
        if self.effective_sorting() == Some(Sorting::Random)
            && self.seed.is_none()
            && let Some(seed) = data.meta.seed.filter(|seed| !seed.is_empty())
        {
            self.seed = Some(seed);
        }
        let mut added = 0;
        for wallpaper in data.data {
            if self.seen.insert(wallpaper.id.clone()) {
                self.items.push(wallpaper);
                added += 1;
            }
        }
        self.status = if self.items.is_empty() && self.fetched_pages.len() == 1 {
            DiscoveryStatus::Empty
        } else {
            DiscoveryStatus::Ready
        };
        ApplyOutcome::Applied { added }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AccountId, UserSettings};

    fn controller() -> DiscoveryController {
        DiscoveryController::new(SessionScope::guest(1), DiscoveryFilters::default())
    }

    #[test]
    fn queries_only_use_documented_parameters() {
        let mut c = controller();
        for view in [
            DiscoveryView::Latest,
            DiscoveryView::Toplist {
                range: TopRange::OneWeek,
            },
            DiscoveryView::Random,
            DiscoveryView::Search,
        ] {
            c.view = view;
            for sorting in [
                None,
                Some(Sorting::DateAdded),
                Some(Sorting::Relevance),
                Some(Sorting::Random),
                Some(Sorting::Views),
                Some(Sorting::Favorites),
                Some(Sorting::Toplist),
            ] {
                c.filters.sorting = sorting;
                for (key, value) in c.query_for_page(2).to_query_pairs() {
                    assert!(
                        [
                            "q",
                            "categories",
                            "purity",
                            "sorting",
                            "order",
                            "topRange",
                            "atleast",
                            "resolutions",
                            "ratios",
                            "colors",
                            "page",
                            "seed"
                        ]
                        .contains(&key),
                        "undocumented parameter {key}"
                    );
                    if key == "sorting" {
                        assert!(
                            [
                                "date_added",
                                "relevance",
                                "random",
                                "views",
                                "favorites",
                                "toplist"
                            ]
                            .contains(&value.as_str()),
                            "undocumented sorting {value}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn selectable_purity_follows_upstream_session_rules() {
        let guest = SessionScope::guest(1);
        assert_eq!(selectable_purity(&guest, None), PurityMask::SFW_AND_SKETCHY);

        let account = SessionScope::account(AccountId::generate(), 2);
        assert_eq!(
            selectable_purity(&account, None),
            PurityMask::SFW_AND_SKETCHY
        );

        let mut settings: UserSettings = serde_json::from_value(serde_json::json!({
            "per_page": 24,
            "purity": ["sfw", "sketchy", "nsfw"],
        }))
        .unwrap();
        assert_eq!(
            selectable_purity(&account, Some(&settings)),
            PurityMask::ALL
        );
        assert_eq!(
            selectable_purity(&guest, Some(&settings)),
            PurityMask::SFW_AND_SKETCHY,
            "a guest never gets NSFW from stale account settings"
        );
        settings.purity = vec!["sfw".into()];
        assert_eq!(
            selectable_purity(&account, Some(&settings)),
            PurityMask::SFW_AND_SKETCHY
        );
    }

    #[test]
    fn hand_offs_are_labelled_and_point_at_the_website() {
        for feature in UnsupportedFeature::ALL {
            let hand_off = feature.hand_off();
            assert_eq!(hand_off.label, OPEN_ON_WALLHAVEN_LABEL);
            assert!(hand_off.url.starts_with("https://wallhaven.cc/"));
            assert!(!feature.title().is_empty());
            assert!(!feature.explanation().is_empty());
        }
        for action in SiteGapAction::ALL {
            let hand_off = action.hand_off();
            assert_eq!(action.label(), OPEN_ON_WALLHAVEN_LABEL);
            assert_eq!(hand_off.label, OPEN_ON_WALLHAVEN_LABEL);
            assert_eq!(hand_off.url, action.url());
            assert!(hand_off.url.starts_with("https://wallhaven.cc/"));
            assert!(!action.title().is_empty());
            assert!(!action.explanation().is_empty());
        }
    }
}
