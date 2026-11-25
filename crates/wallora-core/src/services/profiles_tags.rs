//! GUI-free profiles and tags services and documented Option A hand-offs (Task 6b).
//!
//! Models tag detail pages (`Provider::tag` + `q=id:{tag_id}` search) and uploader listings
//! (`q=@{username}` search) without performing I/O directly. State transitions emit typed
//! requests, callers run them against any [`Provider`], and responses return via
//! `apply_*_response` with session-scope, generation, and duplicate-page checks.
//!
//! Guarantees:
//! - Monotonically increasing generation IDs drop superseded metadata and wallpaper page responses.
//! - Duplicate page requests are deduplicated and paging stops at `last_page`.
//! - `Random` sorting retains `meta.seed` across pages and drops it on explicit refresh or filter
//!   changes.
//! - Purity and sorting are sent upstream exactly as selected with zero client-side filtering.
//! - Unsupported site features (full user profiles/comments, tag directory, and tag moderation)
//!   expose validated `https://wallhaven.cc/...` hand-offs labelled `"Open on wallhaven.cc"`.

use std::collections::{BTreeSet, HashSet};

use crate::domain::{
    BrowseDefaults, PurityMask, QueryEcho, QueryToken, RequestPriority, SearchPage, SearchQuery,
    SessionScope, SortOrder, Sorting, Tag, TopRange, Wallpaper, WallpaperId, validate_username,
};
use crate::provider::{Provider, ProviderError};
use crate::services::discovery::{
    ApplyOutcome, DiscoveryStatus, HandOff, OPEN_ON_WALLHAVEN_LABEL, PageRequest, PageResponse,
};
use crate::services::session::{ScopedResponse, SessionManager, SessionNotice};

/// External browser hand-off URL for Wallhaven's tag directory (Option A gap).
pub const WALLHAVEN_TAGS_URL: &str = "https://wallhaven.cc/tags";

/// Profile and tag actions that have no public API route under Option A.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileTagUnsupportedAction {
    /// Full user profile view, avatar/stats, and profile comments (`https://wallhaven.cc/user/{username}`).
    UserProfileAndComments,
    /// Tag directory, categories, and directory sorts (`https://wallhaven.cc/tags`).
    TagDirectory,
    /// Adding, removing, voting on, or reporting wallpaper tags (`https://wallhaven.cc/w/{id}`).
    TagModeration,
}

impl ProfileTagUnsupportedAction {
    pub const ALL: [Self; 3] = [
        Self::UserProfileAndComments,
        Self::TagDirectory,
        Self::TagModeration,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::UserProfileAndComments => "Full profile & comments",
            Self::TagDirectory => "Tag directory",
            Self::TagModeration => "Edit / moderate tags",
        }
    }

    pub fn label(self) -> &'static str {
        OPEN_ON_WALLHAVEN_LABEL
    }

    pub fn explanation(self) -> &'static str {
        match self {
            Self::UserProfileAndComments => {
                "Full user profiles and profile comments are website-only with no documented API route."
            }
            Self::TagDirectory => {
                "The Wallhaven tag directory and its category/sort views have no documented API route."
            }
            Self::TagModeration => {
                "Adding, removing, voting on, and reporting tags are read-only gaps in the public Wallhaven API."
            }
        }
    }

    /// Validated `https://wallhaven.cc/user/{username}` URL for full profile & comments hand-off.
    pub fn user_profile_url(username: &str) -> Result<String, &'static str> {
        let clean = validate_username(username)?;
        Ok(format!("https://wallhaven.cc/user/{clean}"))
    }

    /// Validated `https://wallhaven.cc/tags` URL for the tag directory hand-off.
    pub fn tag_directory_url() -> &'static str {
        WALLHAVEN_TAGS_URL
    }

    /// Labelled hand-off descriptor for the tag directory.
    pub fn tag_directory_hand_off() -> HandOff {
        HandOff {
            label: OPEN_ON_WALLHAVEN_LABEL,
            url: WALLHAVEN_TAGS_URL,
        }
    }

    /// Validated `https://wallhaven.cc/w/{id}` URL for wallpaper tagging & moderation actions.
    pub fn tag_moderation_url(id: &WallpaperId) -> Result<String, &'static str> {
        let raw = id.as_str();
        if raw.is_empty()
            || raw == "."
            || raw == ".."
            || raw.contains(['/', '\\', '?', '#', '@'])
            || raw.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err("invalid wallpaper id for wallhaven.cc link");
        }
        Ok(format!("https://wallhaven.cc/w/{raw}"))
    }
}

/// Request to fetch metadata for a single tag (`GET /api/v1/tag/{id}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagMetadataRequest {
    pub generation: u64,
    pub scope: SessionScope,
    pub tag_id: u64,
}

/// Response for a [`TagMetadataRequest`], carrying generation, scope, and tag ID.
#[derive(Debug, Clone)]
pub struct TagMetadataResponse {
    pub generation: u64,
    pub scope: SessionScope,
    pub tag_id: u64,
    pub result: Result<Tag, ProviderError>,
}

/// Executes `request` against `provider`.
pub async fn fetch_tag_metadata<P: Provider>(
    provider: &P,
    request: TagMetadataRequest,
) -> TagMetadataResponse {
    let TagMetadataRequest {
        generation,
        scope,
        tag_id,
    } = request;
    let result = provider.tag(tag_id, scope.clone()).await;
    TagMetadataResponse {
        generation,
        scope,
        tag_id,
        result,
    }
}

/// Status of loading tag metadata on [`TagPageController`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagMetadataStatus {
    Idle,
    Loading,
    Loaded(Tag),
    Failed(ProviderError),
}

/// Outcome of applying a [`TagMetadataResponse`] to [`TagPageController`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagMetadataOutcome {
    Applied,
    Failed(ProviderError),
    Stale,
    Revoked(SessionNotice),
}

/// Combined metadata + first-page requests emitted when opening or refreshing a tag page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagLoadRequests {
    pub metadata: TagMetadataRequest,
    pub page: PageRequest,
}

#[derive(Debug, Clone)]
struct PaginatedSearchState {
    purity: PurityMask,
    sorting: Sorting,
    order: Option<SortOrder>,
    top_range: TopRange,
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

impl PaginatedSearchState {
    fn new(purity: PurityMask, top_range: TopRange) -> Self {
        Self {
            purity,
            sorting: Sorting::DateAdded,
            order: None,
            top_range,
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

    fn clear_for_scope(&mut self, defaults: &BrowseDefaults) {
        self.purity = defaults.purity;
        self.top_range = defaults.toplist_range;
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

    fn apply_idle_defaults(&mut self, defaults: &BrowseDefaults) {
        if self.status == DiscoveryStatus::Idle && self.in_flight.is_none() {
            self.purity = defaults.purity;
            self.top_range = defaults.toplist_range;
        }
    }

    fn query_for_page(&self, q: String, page: u32) -> SearchQuery {
        SearchQuery {
            q: Some(q),
            categories: None,
            purity: Some(self.purity),
            sorting: Some(self.sorting),
            order: self.order,
            top_range: (self.sorting == Sorting::Toplist).then_some(self.top_range),
            atleast: None,
            resolutions: Vec::new(),
            ratios: Vec::new(),
            colors: None,
            page: (page > 1).then_some(page),
            seed: if self.sorting == Sorting::Random {
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

    fn can_load_more(&self) -> bool {
        self.status == DiscoveryStatus::Ready
            && self.in_flight.is_none()
            && match (self.fetched_pages.last(), self.last_page) {
                (Some(&highest), Some(last)) => highest < last,
                _ => false,
            }
    }

    fn reload(&mut self, scope: &SessionScope, q: String) -> PageRequest {
        self.generation = self.generation.saturating_add(1);
        self.items.clear();
        self.seen.clear();
        self.fetched_pages.clear();
        self.last_page = None;
        self.total = None;
        self.seed = None;
        self.status = DiscoveryStatus::Loading;
        self.in_flight = Some(1);
        PageRequest {
            generation: self.generation,
            scope: scope.clone(),
            page: 1,
            query: self.query_for_page(q, 1),
        }
    }

    fn request_page(&mut self, scope: &SessionScope, q: String, page: u32) -> Option<PageRequest> {
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
        Some(PageRequest {
            generation: self.generation,
            scope: scope.clone(),
            page,
            query: self.query_for_page(q, page),
        })
    }

    fn request_next_page(&mut self, scope: &SessionScope, q: String) -> Option<PageRequest> {
        if !self.can_load_more() {
            return None;
        }
        let next = self.fetched_pages.last().copied()?.checked_add(1)?;
        self.request_page(scope, q, next)
    }

    fn retry(&mut self, scope: &SessionScope, q: String) -> Option<PageRequest> {
        let DiscoveryStatus::Failed { page, .. } = self.status else {
            return None;
        };
        self.status = if self.fetched_pages.is_empty() {
            DiscoveryStatus::Loading
        } else {
            DiscoveryStatus::Ready
        };
        self.request_page(scope, q, page)
    }

    fn apply_ready_page(
        &mut self,
        generation: u64,
        scope_matches: bool,
        page: u32,
        result: Result<SearchPage, ProviderError>,
        on_success: impl FnOnce(&SearchPage),
    ) -> ApplyOutcome {
        if generation != self.generation || !scope_matches || self.in_flight != Some(page) {
            return ApplyOutcome::Stale;
        }
        self.in_flight = None;
        match result {
            Ok(page_data) => {
                on_success(&page_data);
                self.fetched_pages.insert(page);
                self.last_page = Some(page_data.meta.last_page);
                self.total = Some(page_data.meta.total);
                if self.sorting == Sorting::Random
                    && self.seed.is_none()
                    && let Some(seed) = page_data.meta.seed.filter(|seed| !seed.is_empty())
                {
                    self.seed = Some(seed);
                }
                let mut added = 0;
                for wallpaper in page_data.data {
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

/// GUI-free state machine for a tag detail + `id:{tag_id}` wallpaper listing page.
#[derive(Debug, Clone)]
pub struct TagPageController {
    scope: SessionScope,
    tag_id: Option<u64>,
    hint_name: Option<String>,
    metadata_generation: u64,
    metadata_in_flight: Option<u64>,
    metadata_status: TagMetadataStatus,
    listing: PaginatedSearchState,
}

impl TagPageController {
    /// Initializes a controller synchronized with `session`.
    pub fn from_session(session: &SessionManager) -> Self {
        let defaults = session.browse_defaults();
        Self::new(session.scope(), defaults.purity, defaults.toplist_range)
    }

    /// Creates an empty controller for `scope` and initial purity/toplist defaults.
    pub fn new(scope: SessionScope, purity: PurityMask, top_range: TopRange) -> Self {
        Self {
            scope,
            tag_id: None,
            hint_name: None,
            metadata_generation: 0,
            metadata_in_flight: None,
            metadata_status: TagMetadataStatus::Idle,
            listing: PaginatedSearchState::new(purity, top_range),
        }
    }

    /// Synchronizes controller state with `session` after sign-in, sign-out, switch, 401
    /// revocation, or browse-default refresh.
    ///
    /// Returns `true` when active tag metadata and wallpaper items were cleared and callers should
    /// abort in-flight tasks.
    pub fn sync_session(&mut self, session: &SessionManager) -> bool {
        let next_scope = session.scope();
        let defaults = session.browse_defaults();
        if next_scope != self.scope {
            self.scope = next_scope;
            self.hint_name = None;
            self.metadata_generation = self.metadata_generation.saturating_add(1);
            self.metadata_in_flight = None;
            self.metadata_status = TagMetadataStatus::Idle;
            self.listing.clear_for_scope(&defaults);
            return true;
        }
        if self.metadata_status == TagMetadataStatus::Idle && self.metadata_in_flight.is_none() {
            self.listing.apply_idle_defaults(&defaults);
        }
        false
    }

    pub fn scope(&self) -> &SessionScope {
        &self.scope
    }

    pub fn tag_id(&self) -> Option<u64> {
        self.tag_id
    }

    pub fn hint_name(&self) -> Option<&str> {
        self.hint_name.as_deref()
    }

    pub fn tag(&self) -> Option<&Tag> {
        match &self.metadata_status {
            TagMetadataStatus::Loaded(tag) => Some(tag),
            _ => None,
        }
    }

    pub fn metadata_generation(&self) -> u64 {
        self.metadata_generation
    }

    pub fn metadata_status(&self) -> &TagMetadataStatus {
        &self.metadata_status
    }

    pub fn is_loading_metadata(&self) -> bool {
        self.metadata_in_flight.is_some()
    }

    pub fn purity(&self) -> PurityMask {
        self.listing.purity
    }

    pub fn sorting(&self) -> Sorting {
        self.listing.sorting
    }

    pub fn order(&self) -> Option<SortOrder> {
        self.listing.order
    }

    pub fn top_range(&self) -> TopRange {
        self.listing.top_range
    }

    pub fn items_generation(&self) -> u64 {
        self.listing.generation
    }

    pub fn items(&self) -> &[Wallpaper] {
        &self.listing.items
    }

    pub fn items_status(&self) -> &DiscoveryStatus {
        &self.listing.status
    }

    pub fn is_loading_items(&self) -> bool {
        self.listing.in_flight.is_some()
    }

    pub fn last_page(&self) -> Option<u32> {
        self.listing.last_page
    }

    pub fn total(&self) -> Option<u64> {
        self.listing.total
    }

    pub fn seed(&self) -> Option<&str> {
        self.listing.seed.as_deref()
    }

    pub fn fetched_pages(&self) -> impl Iterator<Item = u32> + '_ {
        self.listing.fetched_pages.iter().copied()
    }

    pub fn can_load_more(&self) -> bool {
        self.tag_id.is_some() && self.listing.can_load_more()
    }

    pub fn search_query_string(&self) -> Option<String> {
        self.tag_id.map(|id| QueryToken::tag_id(id).render())
    }

    pub fn query_for_page(&self, page: u32) -> Option<SearchQuery> {
        let q = self.search_query_string()?;
        Some(self.listing.query_for_page(q, page))
    }

    /// Starts loading metadata (`Provider::tag`) and page 1 (`q=id:{tag_id}`) for `tag_id`.
    pub fn open_tag(
        &mut self,
        tag_id: u64,
        hint_name: Option<String>,
    ) -> Result<TagLoadRequests, &'static str> {
        if tag_id == 0 {
            return Err("Invalid tag ID.");
        }
        self.tag_id = Some(tag_id);
        self.hint_name = hint_name.and_then(|s| {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });
        let metadata = self.reload_metadata(tag_id);
        let page = self
            .listing
            .reload(&self.scope, QueryToken::tag_id(tag_id).render());
        Ok(TagLoadRequests { metadata, page })
    }

    /// Refreshes both metadata and page 1 of the current tag.
    pub fn refresh(&mut self) -> Option<TagLoadRequests> {
        let tag_id = self.tag_id?;
        let metadata = self.reload_metadata(tag_id);
        let page = self
            .listing
            .reload(&self.scope, QueryToken::tag_id(tag_id).render());
        Some(TagLoadRequests { metadata, page })
    }

    /// Retries a failed tag metadata request.
    pub fn retry_metadata(&mut self) -> Option<TagMetadataRequest> {
        if !matches!(self.metadata_status, TagMetadataStatus::Failed(_))
            || self.metadata_in_flight.is_some()
        {
            return None;
        }
        let tag_id = self.tag_id?;
        Some(self.reload_metadata(tag_id))
    }

    /// Updates purity and reloads wallpaper pages while keeping tag metadata intact.
    pub fn set_purity(&mut self, purity: PurityMask) -> Option<PageRequest> {
        if self.listing.purity == purity {
            return None;
        }
        self.listing.purity = purity;
        let q = self.search_query_string()?;
        Some(self.listing.reload(&self.scope, q))
    }

    /// Updates sorting and reloads wallpaper pages while keeping tag metadata intact.
    pub fn set_sorting(&mut self, sorting: Sorting) -> Option<PageRequest> {
        if self.listing.sorting == sorting {
            return None;
        }
        self.listing.sorting = sorting;
        let q = self.search_query_string()?;
        Some(self.listing.reload(&self.scope, q))
    }

    /// Updates sort order and reloads wallpaper pages while keeping tag metadata intact.
    pub fn set_order(&mut self, order: Option<SortOrder>) -> Option<PageRequest> {
        if self.listing.order == order {
            return None;
        }
        self.listing.order = order;
        let q = self.search_query_string()?;
        Some(self.listing.reload(&self.scope, q))
    }

    /// Updates toplist range (activating `Toplist` sorting if needed) and reloads wallpaper pages.
    pub fn set_top_range(&mut self, range: TopRange) -> Option<PageRequest> {
        if self.listing.top_range == range && self.listing.sorting == Sorting::Toplist {
            return None;
        }
        self.listing.top_range = range;
        self.listing.sorting = Sorting::Toplist;
        let q = self.search_query_string()?;
        Some(self.listing.reload(&self.scope, q))
    }

    pub fn request_page(&mut self, page: u32) -> Option<PageRequest> {
        let q = self.search_query_string()?;
        self.listing.request_page(&self.scope, q, page)
    }

    pub fn request_next_page(&mut self) -> Option<PageRequest> {
        let q = self.search_query_string()?;
        self.listing.request_next_page(&self.scope, q)
    }

    pub fn retry_items(&mut self) -> Option<PageRequest> {
        let q = self.search_query_string()?;
        self.listing.retry(&self.scope, q)
    }

    /// Applies a fetched tag metadata response after session-scope and generation validation.
    pub fn apply_metadata_response(
        &mut self,
        session: &mut SessionManager,
        response: TagMetadataResponse,
    ) -> TagMetadataOutcome {
        let TagMetadataResponse {
            generation,
            scope,
            tag_id,
            result,
        } = response;
        match session.handle_provider_result(&scope, result) {
            ScopedResponse::Stale => TagMetadataOutcome::Stale,
            ScopedResponse::Revoked(notice) => {
                self.sync_session(session);
                TagMetadataOutcome::Revoked(notice)
            }
            ScopedResponse::Ready(result) => {
                if generation != self.metadata_generation
                    || scope != self.scope
                    || self.metadata_in_flight != Some(generation)
                    || self.tag_id != Some(tag_id)
                {
                    return TagMetadataOutcome::Stale;
                }
                self.metadata_in_flight = None;
                match result {
                    Ok(tag) => {
                        if !tag.name.trim().is_empty() {
                            self.hint_name = Some(tag.name.clone());
                        }
                        self.metadata_status = TagMetadataStatus::Loaded(tag);
                        TagMetadataOutcome::Applied
                    }
                    Err(error) => {
                        self.metadata_status = TagMetadataStatus::Failed(error.clone());
                        TagMetadataOutcome::Failed(error)
                    }
                }
            }
        }
    }

    /// Applies one fetched `id:{tag_id}` wallpaper page after session-scope and generation checks.
    pub fn apply_page_response(
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
                self.sync_session(session);
                ApplyOutcome::Revoked(notice)
            }
            ScopedResponse::Ready(result) => {
                let scope_matches = scope == self.scope;
                let active_tag_id = self.tag_id;
                let hint_name = &mut self.hint_name;
                self.listing.apply_ready_page(
                    generation,
                    scope_matches,
                    page,
                    result,
                    |page_data| {
                        if hint_name.is_none()
                            && let Some(QueryEcho::Tag { id, tag }) = &page_data.meta.query
                            && Some(*id) == active_tag_id
                            && !tag.trim().is_empty()
                        {
                            *hint_name = Some(tag.clone());
                        }
                    },
                )
            }
        }
    }

    fn reload_metadata(&mut self, tag_id: u64) -> TagMetadataRequest {
        self.metadata_generation = self.metadata_generation.saturating_add(1);
        self.metadata_in_flight = Some(self.metadata_generation);
        self.metadata_status = TagMetadataStatus::Loading;
        TagMetadataRequest {
            generation: self.metadata_generation,
            scope: self.scope.clone(),
            tag_id,
        }
    }
}

/// GUI-free state machine for browsing an uploader's wallpapers via `q=@{username}`.
#[derive(Debug, Clone)]
pub struct UploaderController {
    scope: SessionScope,
    username: Option<String>,
    listing: PaginatedSearchState,
}

impl UploaderController {
    /// Initializes a controller synchronized with `session`.
    pub fn from_session(session: &SessionManager) -> Self {
        let defaults = session.browse_defaults();
        Self::new(session.scope(), defaults.purity, defaults.toplist_range)
    }

    /// Creates an empty controller for `scope` and initial purity/toplist defaults.
    pub fn new(scope: SessionScope, purity: PurityMask, top_range: TopRange) -> Self {
        Self {
            scope,
            username: None,
            listing: PaginatedSearchState::new(purity, top_range),
        }
    }

    /// Synchronizes controller state with `session` after sign-in, sign-out, switch, 401
    /// revocation, or browse-default refresh.
    ///
    /// Returns `true` when active items were cleared and callers should abort in-flight tasks.
    pub fn sync_session(&mut self, session: &SessionManager) -> bool {
        let next_scope = session.scope();
        let defaults = session.browse_defaults();
        if next_scope != self.scope {
            self.scope = next_scope;
            self.listing.clear_for_scope(&defaults);
            return true;
        }
        self.listing.apply_idle_defaults(&defaults);
        false
    }

    pub fn scope(&self) -> &SessionScope {
        &self.scope
    }

    pub fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }

    pub fn purity(&self) -> PurityMask {
        self.listing.purity
    }

    pub fn sorting(&self) -> Sorting {
        self.listing.sorting
    }

    pub fn order(&self) -> Option<SortOrder> {
        self.listing.order
    }

    pub fn top_range(&self) -> TopRange {
        self.listing.top_range
    }

    pub fn generation(&self) -> u64 {
        self.listing.generation
    }

    pub fn items(&self) -> &[Wallpaper] {
        &self.listing.items
    }

    pub fn status(&self) -> &DiscoveryStatus {
        &self.listing.status
    }

    pub fn is_loading(&self) -> bool {
        self.listing.in_flight.is_some()
    }

    pub fn last_page(&self) -> Option<u32> {
        self.listing.last_page
    }

    pub fn total(&self) -> Option<u64> {
        self.listing.total
    }

    pub fn seed(&self) -> Option<&str> {
        self.listing.seed.as_deref()
    }

    pub fn fetched_pages(&self) -> impl Iterator<Item = u32> + '_ {
        self.listing.fetched_pages.iter().copied()
    }

    pub fn can_load_more(&self) -> bool {
        self.username.is_some() && self.listing.can_load_more()
    }

    pub fn search_query_string(&self) -> Option<String> {
        self.username.as_deref().map(|u| format!("@{u}"))
    }

    pub fn profile_handoff_url(&self) -> Option<String> {
        self.username
            .as_deref()
            .and_then(|u| ProfileTagUnsupportedAction::user_profile_url(u).ok())
    }

    pub fn query_for_page(&self, page: u32) -> Option<SearchQuery> {
        let q = self.search_query_string()?;
        Some(self.listing.query_for_page(q, page))
    }

    /// Validates `raw_username` and begins loading page 1 of `q=@{username}`.
    pub fn open_uploader(&mut self, raw_username: &str) -> Result<PageRequest, &'static str> {
        let clean = validate_username(raw_username)?;
        QueryToken::user(clean.clone()).map_err(|_| "Username contains invalid characters.")?;
        self.username = Some(clean);
        let q = self.search_query_string().expect("username just assigned");
        Ok(self.listing.reload(&self.scope, q))
    }

    pub fn refresh(&mut self) -> Option<PageRequest> {
        let q = self.search_query_string()?;
        Some(self.listing.reload(&self.scope, q))
    }

    pub fn set_purity(&mut self, purity: PurityMask) -> Option<PageRequest> {
        if self.listing.purity == purity {
            return None;
        }
        self.listing.purity = purity;
        let q = self.search_query_string()?;
        Some(self.listing.reload(&self.scope, q))
    }

    pub fn set_sorting(&mut self, sorting: Sorting) -> Option<PageRequest> {
        if self.listing.sorting == sorting {
            return None;
        }
        self.listing.sorting = sorting;
        let q = self.search_query_string()?;
        Some(self.listing.reload(&self.scope, q))
    }

    pub fn set_order(&mut self, order: Option<SortOrder>) -> Option<PageRequest> {
        if self.listing.order == order {
            return None;
        }
        self.listing.order = order;
        let q = self.search_query_string()?;
        Some(self.listing.reload(&self.scope, q))
    }

    pub fn set_top_range(&mut self, range: TopRange) -> Option<PageRequest> {
        if self.listing.top_range == range && self.listing.sorting == Sorting::Toplist {
            return None;
        }
        self.listing.top_range = range;
        self.listing.sorting = Sorting::Toplist;
        let q = self.search_query_string()?;
        Some(self.listing.reload(&self.scope, q))
    }

    pub fn request_page(&mut self, page: u32) -> Option<PageRequest> {
        let q = self.search_query_string()?;
        self.listing.request_page(&self.scope, q, page)
    }

    pub fn request_next_page(&mut self) -> Option<PageRequest> {
        let q = self.search_query_string()?;
        self.listing.request_next_page(&self.scope, q)
    }

    pub fn retry(&mut self) -> Option<PageRequest> {
        let q = self.search_query_string()?;
        self.listing.retry(&self.scope, q)
    }

    /// Applies one fetched `@{username}` search page after session-scope and generation checks.
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
                self.sync_session(session);
                ApplyOutcome::Revoked(notice)
            }
            ScopedResponse::Ready(result) => {
                let scope_matches = scope == self.scope;
                self.listing
                    .apply_ready_page(generation, scope_matches, page, result, |_| {})
            }
        }
    }
}
