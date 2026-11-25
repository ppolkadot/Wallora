//! GUI-free collections service, Favorites mapping policy, and controller (Task 6a).
//!
//! Models authenticated own collections alongside browsing other users' public collections without
//! performing I/O directly. State transitions emit [`CollectionListRequest`] or
//! [`CollectionItemsRequest`] values, callers execute them through a [`Provider`], and responses
//! return through [`CollectionsController::apply_list_response`] and
//! [`CollectionsController::apply_items_response`].
//!
//! Guarantees:
//! - Own collection **items** are strictly gated behind `UsernameVerification::Verified`. For
//!   accepted-but-unverified states (`UnverifiablePrivate`, `UnverifiableEmpty`, `Inconclusive`),
//!   the authenticated own-collection list remains visible while item loads are refused with an
//!   explicit explanation (including attempts to bypass the gate via the public user browser).
//! - Task 1b Favorites mapping is currently unresolved: no collection is ever hardcoded or guessed
//!   as `"Favorites"` (even if upstream labels it `"Default"`). Only an explicit per-`AccountId`
//!   confirmed ID combined with `UsernameVerification::Verified` renders `"Favorites"`.
//! - Writes (favoriting/unfavoriting, collection CRUD, privacy toggles) are read-only gaps in the
//!   public API: no shadow write state is stored, and validated `https://wallhaven.cc/...`
//!   hand-offs are exposed via [`CollectionUnsupportedAction`].

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::domain::{
    AccountId, CollectionSummary, PurityMask, SearchPage, SessionKind, SessionScope,
    UsernameVerification, Wallpaper, WallpaperId, validate_username,
};
use crate::provider::{Provider, ProviderError};
use crate::services::discovery::{ApplyOutcome, DiscoveryStatus, OPEN_ON_WALLHAVEN_LABEL};
use crate::services::session::{ScopedResponse, SessionManager, SessionNotice};

/// Per-account confirmed Favorites collection identifier.
///
/// Stored per `AccountId` and never shared across accounts. Because Task 1b Favorites mapping is
/// currently unresolved, this map starts empty and never guesses `"Default"` or any other label.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FavoritesMapping {
    confirmed: HashMap<AccountId, u64>,
}

impl FavoritesMapping {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a confirmed stable Favorites collection ID for `account_id`.
    pub fn record_confirmed(&mut self, account_id: AccountId, collection_id: u64) {
        self.confirmed.insert(account_id, collection_id);
    }

    /// Removes any confirmed Favorites mapping recorded for `account_id`.
    pub fn remove(&mut self, account_id: &AccountId) {
        self.confirmed.remove(account_id);
    }

    /// Returns the confirmed Favorites collection ID recorded specifically for `account_id`.
    pub fn confirmed_id(&self, account_id: &AccountId) -> Option<u64> {
        self.confirmed.get(account_id).copied()
    }

    /// Resolves the display label for `collection` under the given account and verification state.
    pub fn display_label<'a>(
        &self,
        collection: &'a CollectionSummary,
        account_id: Option<&AccountId>,
        verification: Option<UsernameVerification>,
    ) -> &'a str {
        let confirmed_id = account_id.and_then(|id| self.confirmed_id(id));
        display_collection_label(collection, account_id, verification, confirmed_id)
    }
}

/// Returns `"Favorites"` ONLY when both (a) a confirmed Favorites collection ID exists for the
/// active `AccountId` and (b) `verification` is `UsernameVerification::Verified`.
///
/// Otherwise returns the exact upstream `collection.label`.
pub fn display_collection_label<'a>(
    collection: &'a CollectionSummary,
    account_id: Option<&AccountId>,
    verification: Option<UsernameVerification>,
    confirmed_favorites_id: Option<u64>,
) -> &'a str {
    if account_id.is_some()
        && verification.is_some_and(UsernameVerification::is_verified)
        && confirmed_favorites_id == Some(collection.id)
    {
        "Favorites"
    } else {
        collection.label.as_str()
    }
}

/// Collection and favorite write actions not supported by the public Wallhaven API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CollectionUnsupportedAction {
    FavoriteToggle,
    ManageCollections,
}

impl CollectionUnsupportedAction {
    pub const ALL: [Self; 2] = [Self::FavoriteToggle, Self::ManageCollections];

    pub fn title(self) -> &'static str {
        match self {
            Self::FavoriteToggle => "Favorite / Unfavorite",
            Self::ManageCollections => "Manage collections & privacy",
        }
    }

    pub fn label(self) -> &'static str {
        OPEN_ON_WALLHAVEN_LABEL
    }

    pub fn explanation(self) -> &'static str {
        match self {
            Self::FavoriteToggle => {
                "Favoriting wallpapers is read-only in the public Wallhaven API. Open this wallpaper on wallhaven.cc to add or remove favorites."
            }
            Self::ManageCollections => {
                "Creating, renaming, deleting, and changing collection privacy are not supported by the public Wallhaven API. Manage collections on wallhaven.cc."
            }
        }
    }

    /// Validated `https://wallhaven.cc/w/{id}` hand-off URL for toggling favorites in the browser.
    pub fn favorite_url(id: &WallpaperId) -> Result<String, &'static str> {
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

    /// Validated `https://wallhaven.cc/...` hand-off URL for managing collections or privacy.
    pub fn manage_collections_url(username: Option<&str>) -> String {
        if let Some(clean) = username.and_then(|u| validate_username(u).ok()) {
            format!("https://wallhaven.cc/user/{clean}/favorites")
        } else {
            "https://wallhaven.cc/".to_string()
        }
    }
}

/// Snapshot of the active signed-in account needed for collection access checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnAccountState {
    pub account_id: AccountId,
    pub username: String,
    pub verification: UsernameVerification,
}

/// Availability of the authenticated account's own collection list (`GET /api/v1/collections`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnCollectionsListStatus {
    SignedOut,
    NotLoaded,
    Empty,
    Ready,
}

/// Status of loading another user's public collection list (`GET /api/v1/collections/{username}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionListStatus {
    Idle,
    Loading,
    Ready,
    Empty,
    Failed(ProviderError),
}

/// Identifies whether the currently displayed collection belongs to the active account or another
/// user's public profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionSource {
    Own {
        account_id: AccountId,
        username: String,
    },
    PublicUser {
        username: String,
    },
}

/// Metadata for the currently selected collection whose items are displayed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedCollection {
    pub source: CollectionSource,
    pub collection_id: u64,
    pub label: String,
    pub public: Option<bool>,
    pub count: Option<u64>,
}

/// Explicit reason why own collection items cannot be fetched or displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnItemsBlockedReason {
    SignedOut,
    Unverified(UsernameVerification),
}

impl OwnItemsBlockedReason {
    pub fn explanation(self) -> &'static str {
        match self {
            Self::SignedOut => {
                "Sign in with a verified Wallhaven account to view your collections."
            }
            Self::Unverified(verification) => verification.explanation(),
        }
    }
}

/// Request to list a user's public collections (`GET /api/v1/collections/{username}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionListRequest {
    pub generation: u64,
    pub scope: SessionScope,
    pub username: String,
}

/// Response for a [`CollectionListRequest`], carrying generation and scope for staleness checks.
#[derive(Debug, Clone)]
pub struct CollectionListResponse {
    pub generation: u64,
    pub scope: SessionScope,
    pub username: String,
    pub result: Result<Vec<CollectionSummary>, ProviderError>,
}

/// Executes `request` against `provider`.
pub async fn fetch_user_collections<P: Provider>(
    provider: &P,
    request: CollectionListRequest,
) -> CollectionListResponse {
    let CollectionListRequest {
        generation,
        scope,
        username,
    } = request;
    let result = provider.collections(username.clone(), scope.clone()).await;
    CollectionListResponse {
        generation,
        scope,
        username,
        result,
    }
}

/// Request to fetch one page of items in a collection (`GET /api/v1/collections/{username}/{id}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionItemsRequest {
    pub generation: u64,
    pub scope: SessionScope,
    pub username: String,
    pub collection_id: u64,
    pub purity: PurityMask,
    pub page: u32,
}

/// Response for a [`CollectionItemsRequest`], carrying generation, scope, and page number.
#[derive(Debug, Clone)]
pub struct CollectionItemsResponse {
    pub generation: u64,
    pub scope: SessionScope,
    pub page: u32,
    pub result: Result<SearchPage, ProviderError>,
}

/// Executes `request` against `provider`.
pub async fn fetch_collection_items<P: Provider>(
    provider: &P,
    request: CollectionItemsRequest,
) -> CollectionItemsResponse {
    let CollectionItemsRequest {
        generation,
        scope,
        username,
        collection_id,
        purity,
        page,
    } = request;
    let result = provider
        .collection_items(
            username,
            collection_id,
            Some(purity),
            (page > 1).then_some(page),
            scope.clone(),
        )
        .await;
    CollectionItemsResponse {
        generation,
        scope,
        page,
        result,
    }
}

/// Result of applying a [`CollectionListResponse`] to [`CollectionsController`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollectionListOutcome {
    Applied { count: usize },
    Failed(ProviderError),
    Stale,
    Revoked(SessionNotice),
}

/// GUI-free collections state machine governing own collections, other users' public collections,
/// item pagination, identity verification gating, and Favorites label policy.
#[derive(Debug, Clone)]
pub struct CollectionsController {
    scope: SessionScope,
    own_account: Option<OwnAccountState>,
    own_collections: Option<Vec<CollectionSummary>>,
    favorites: FavoritesMapping,
    list_generation: u64,
    browsed_username: Option<String>,
    user_collections: Vec<CollectionSummary>,
    user_collections_status: CollectionListStatus,
    list_in_flight: Option<u64>,
    selected_collection: Option<SelectedCollection>,
    items_blocked: Option<OwnItemsBlockedReason>,
    purity: PurityMask,
    items_generation: u64,
    items: Vec<Wallpaper>,
    seen: HashSet<WallpaperId>,
    fetched_pages: BTreeSet<u32>,
    items_in_flight: Option<u32>,
    last_page: Option<u32>,
    total: Option<u64>,
    items_status: DiscoveryStatus,
}

impl CollectionsController {
    /// Initializes a controller synchronized with `session`.
    pub fn from_session(session: &SessionManager) -> Self {
        let mut controller = Self::new(session.scope(), session.browse_defaults().purity);
        controller.own_account = Self::extract_own_account(session);
        controller.own_collections = session.own_collections().map(<[CollectionSummary]>::to_vec);
        controller
    }

    /// Creates an empty controller for `scope` and initial `purity`.
    pub fn new(scope: SessionScope, purity: PurityMask) -> Self {
        Self {
            scope,
            own_account: None,
            own_collections: None,
            favorites: FavoritesMapping::new(),
            list_generation: 0,
            browsed_username: None,
            user_collections: Vec::new(),
            user_collections_status: CollectionListStatus::Idle,
            list_in_flight: None,
            selected_collection: None,
            items_blocked: None,
            purity,
            items_generation: 0,
            items: Vec::new(),
            seen: HashSet::new(),
            fetched_pages: BTreeSet::new(),
            items_in_flight: None,
            last_page: None,
            total: None,
            items_status: DiscoveryStatus::Idle,
        }
    }

    fn extract_own_account(session: &SessionManager) -> Option<OwnAccountState> {
        match (&session.scope().kind, session.active_account()) {
            (SessionKind::Account(_), Some(account)) => Some(OwnAccountState {
                account_id: account.id.clone(),
                username: account.username.clone(),
                verification: account.verification,
            }),
            _ => None,
        }
    }

    fn clear_items_state(&mut self) {
        self.items_generation = self.items_generation.saturating_add(1);
        self.selected_collection = None;
        self.items_blocked = None;
        self.items.clear();
        self.seen.clear();
        self.fetched_pages.clear();
        self.items_in_flight = None;
        self.last_page = None;
        self.total = None;
        self.items_status = DiscoveryStatus::Idle;
    }

    /// Synchronizes controller state with `session` after sign-in, sign-out, switch, 401
    /// revocation, or account refresh.
    ///
    /// Returns `true` when active collection state/items were cleared and any in-flight tasks in
    /// the caller should be cancelled.
    pub fn sync_session(&mut self, session: &SessionManager) -> bool {
        let next_scope = session.scope();
        let next_account = Self::extract_own_account(session);
        let next_own = session.own_collections().map(<[CollectionSummary]>::to_vec);

        if next_scope != self.scope {
            self.scope = next_scope;
            self.own_account = next_account;
            self.own_collections = next_own;
            self.purity = session.browse_defaults().purity;
            self.list_generation = self.list_generation.saturating_add(1);
            self.list_in_flight = None;
            self.browsed_username = None;
            self.user_collections.clear();
            self.user_collections_status = CollectionListStatus::Idle;
            self.clear_items_state();
            return true;
        }

        self.own_account = next_account;
        self.own_collections = next_own;
        if self.selected_collection.is_none() {
            self.purity = session.browse_defaults().purity;
        }

        if let Some(selected) = &self.selected_collection {
            match &selected.source {
                CollectionSource::Own { .. } => {
                    let collection_id = selected.collection_id;
                    match self.own_account.clone() {
                        None => {
                            self.clear_items_state();
                            self.items_blocked = Some(OwnItemsBlockedReason::SignedOut);
                            return true;
                        }
                        Some(account)
                            if !account.verification.can_access_own_collection_items() =>
                        {
                            self.clear_items_state();
                            self.items_blocked =
                                Some(OwnItemsBlockedReason::Unverified(account.verification));
                            return true;
                        }
                        Some(account) => {
                            let updated = self
                                .own_collections
                                .as_deref()
                                .and_then(|list| list.iter().find(|c| c.id == collection_id))
                                .map(|summary| {
                                    (
                                        self.favorites
                                            .display_label(
                                                summary,
                                                Some(&account.account_id),
                                                Some(account.verification),
                                            )
                                            .to_string(),
                                        summary.public,
                                        summary.count,
                                    )
                                });
                            if let (Some(selected), Some((label, public, count))) =
                                (&mut self.selected_collection, updated)
                            {
                                selected.label = label;
                                selected.public = Some(public);
                                selected.count = Some(count);
                            }
                        }
                    }
                }
                CollectionSource::PublicUser { username } => {
                    if let Some(account) = &self.own_account
                        && !account.verification.can_access_own_collection_items()
                        && account.username.eq_ignore_ascii_case(username)
                    {
                        let verification = account.verification;
                        self.clear_items_state();
                        self.items_blocked = Some(OwnItemsBlockedReason::Unverified(verification));
                        return true;
                    }
                }
            }
        } else if self
            .own_account
            .as_ref()
            .is_some_and(|a| a.verification.can_access_own_collection_items())
            && matches!(
                self.items_blocked,
                Some(OwnItemsBlockedReason::Unverified(_))
            )
        {
            self.items_blocked = None;
        }

        false
    }

    pub fn scope(&self) -> &SessionScope {
        &self.scope
    }

    pub fn own_account(&self) -> Option<&OwnAccountState> {
        self.own_account.as_ref()
    }

    pub fn own_collections(&self) -> Option<&[CollectionSummary]> {
        self.own_collections.as_deref()
    }

    pub fn own_collections_status(&self) -> OwnCollectionsListStatus {
        if self.own_account.is_none() {
            return OwnCollectionsListStatus::SignedOut;
        }
        match &self.own_collections {
            None => OwnCollectionsListStatus::NotLoaded,
            Some(list) if list.is_empty() => OwnCollectionsListStatus::Empty,
            Some(_) => OwnCollectionsListStatus::Ready,
        }
    }

    pub fn can_access_own_collection_items(&self) -> bool {
        self.own_account
            .as_ref()
            .is_some_and(|a| a.verification.can_access_own_collection_items())
    }

    pub fn own_items_blocked_reason(&self) -> Option<OwnItemsBlockedReason> {
        match &self.own_account {
            None => Some(OwnItemsBlockedReason::SignedOut),
            Some(account) if !account.verification.can_access_own_collection_items() => {
                Some(OwnItemsBlockedReason::Unverified(account.verification))
            }
            Some(_) => None,
        }
    }

    pub fn favorites_mapping(&self) -> &FavoritesMapping {
        &self.favorites
    }

    pub fn favorites_mapping_mut(&mut self) -> &mut FavoritesMapping {
        &mut self.favorites
    }

    pub fn display_own_collection_label<'a>(
        &'a self,
        collection: &'a CollectionSummary,
    ) -> &'a str {
        match &self.own_account {
            Some(account) => self.favorites.display_label(
                collection,
                Some(&account.account_id),
                Some(account.verification),
            ),
            None => collection.label.as_str(),
        }
    }

    pub fn browsed_username(&self) -> Option<&str> {
        self.browsed_username.as_deref()
    }

    pub fn user_collections(&self) -> &[CollectionSummary] {
        &self.user_collections
    }

    pub fn user_collections_status(&self) -> &CollectionListStatus {
        &self.user_collections_status
    }

    pub fn list_generation(&self) -> u64 {
        self.list_generation
    }

    pub fn is_loading_user_collections(&self) -> bool {
        self.list_in_flight.is_some()
    }

    pub fn selected_collection(&self) -> Option<&SelectedCollection> {
        self.selected_collection.as_ref()
    }

    pub fn items_blocked(&self) -> Option<OwnItemsBlockedReason> {
        self.items_blocked
    }

    pub fn purity(&self) -> PurityMask {
        self.purity
    }

    pub fn items_generation(&self) -> u64 {
        self.items_generation
    }

    pub fn items(&self) -> &[Wallpaper] {
        &self.items
    }

    pub fn items_status(&self) -> &DiscoveryStatus {
        &self.items_status
    }

    pub fn is_loading_items(&self) -> bool {
        self.items_in_flight.is_some()
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

    pub fn can_load_more(&self) -> bool {
        self.selected_collection.is_some()
            && self.items_status == DiscoveryStatus::Ready
            && self.items_in_flight.is_none()
            && match (self.fetched_pages.last(), self.last_page) {
                (Some(&highest), Some(last)) => highest < last,
                _ => false,
            }
    }

    /// Selects one of the active signed-in account's collections for item viewing.
    ///
    /// Refuses item access unless `UsernameVerification::Verified` holds; accepted-but-unverified
    /// states leave the authenticated own-collection list visible while returning the blocking
    /// reason.
    pub fn select_own_collection(
        &mut self,
        collection_id: u64,
    ) -> Result<CollectionItemsRequest, OwnItemsBlockedReason> {
        let Some(account) = self.own_account.clone() else {
            self.clear_items_state();
            self.items_blocked = Some(OwnItemsBlockedReason::SignedOut);
            return Err(OwnItemsBlockedReason::SignedOut);
        };

        if !account.verification.can_access_own_collection_items() {
            let reason = OwnItemsBlockedReason::Unverified(account.verification);
            self.clear_items_state();
            self.items_blocked = Some(reason);
            return Err(reason);
        }

        let summary = self
            .own_collections
            .as_deref()
            .and_then(|list| list.iter().find(|c| c.id == collection_id));
        let label = match summary {
            Some(col) => self
                .favorites
                .display_label(col, Some(&account.account_id), Some(account.verification))
                .to_string(),
            None => format!("Collection #{collection_id}"),
        };

        self.items_blocked = None;
        self.selected_collection = Some(SelectedCollection {
            source: CollectionSource::Own {
                account_id: account.account_id,
                username: account.username,
            },
            collection_id,
            label,
            public: summary.map(|c| c.public),
            count: summary.map(|c| c.count),
        });

        Ok(self.reload_items())
    }

    /// Begins loading another user's public collections (`GET /api/v1/collections/{username}`).
    pub fn load_user_collections(
        &mut self,
        raw_username: &str,
    ) -> Result<CollectionListRequest, &'static str> {
        let clean = validate_username(raw_username)?;
        self.list_generation = self.list_generation.saturating_add(1);
        self.browsed_username = Some(clean.clone());
        self.user_collections.clear();
        self.user_collections_status = CollectionListStatus::Loading;
        self.list_in_flight = Some(self.list_generation);

        if matches!(
            self.selected_collection,
            Some(SelectedCollection {
                source: CollectionSource::PublicUser { .. },
                ..
            })
        ) {
            self.clear_items_state();
        }

        Ok(CollectionListRequest {
            generation: self.list_generation,
            scope: self.scope.clone(),
            username: clean,
        })
    }

    /// Retries a failed user collection list lookup.
    pub fn retry_user_collections(&mut self) -> Option<CollectionListRequest> {
        if !matches!(
            self.user_collections_status,
            CollectionListStatus::Failed(_)
        ) || self.list_in_flight.is_some()
        {
            return None;
        }
        let username = self.browsed_username.clone()?;
        self.list_generation = self.list_generation.saturating_add(1);
        self.user_collections_status = CollectionListStatus::Loading;
        self.list_in_flight = Some(self.list_generation);
        Some(CollectionListRequest {
            generation: self.list_generation,
            scope: self.scope.clone(),
            username,
        })
    }

    /// Applies the result of a [`CollectionListRequest`] after scope, session, and generation
    /// validation.
    pub fn apply_list_response(
        &mut self,
        session: &mut SessionManager,
        response: CollectionListResponse,
    ) -> CollectionListOutcome {
        let CollectionListResponse {
            generation,
            scope,
            username: _,
            result,
        } = response;

        match session.handle_provider_result(&scope, result) {
            ScopedResponse::Stale => CollectionListOutcome::Stale,
            ScopedResponse::Revoked(notice) => {
                self.sync_session(session);
                CollectionListOutcome::Revoked(notice)
            }
            ScopedResponse::Ready(result) => {
                if generation != self.list_generation
                    || scope != self.scope
                    || self.list_in_flight != Some(generation)
                {
                    return CollectionListOutcome::Stale;
                }
                self.list_in_flight = None;
                match result {
                    Ok(list) => {
                        let count = list.len();
                        self.user_collections = list;
                        self.user_collections_status = if count == 0 {
                            CollectionListStatus::Empty
                        } else {
                            CollectionListStatus::Ready
                        };
                        CollectionListOutcome::Applied { count }
                    }
                    Err(error) => {
                        self.user_collections_status = CollectionListStatus::Failed(error.clone());
                        CollectionListOutcome::Failed(error)
                    }
                }
            }
        }
    }

    /// Selects a collection from the currently loaded user's public collection list.
    pub fn select_user_collection(
        &mut self,
        collection_id: u64,
    ) -> Result<CollectionItemsRequest, &'static str> {
        let username = self
            .browsed_username
            .clone()
            .ok_or("Load a user's collections first.")?;
        self.load_public_collection_items(&username, collection_id)
    }

    /// Requests page 1 of a user's collection items (`GET /api/v1/collections/{username}/{id}`).
    ///
    /// Also enforces the own-collection item gate when an unverified signed-in account attempts
    /// to query its own username through the public browser.
    pub fn load_public_collection_items(
        &mut self,
        raw_username: &str,
        collection_id: u64,
    ) -> Result<CollectionItemsRequest, &'static str> {
        let clean = validate_username(raw_username)?;
        if let Some(account) = &self.own_account
            && !account.verification.can_access_own_collection_items()
            && account.username.eq_ignore_ascii_case(&clean)
        {
            let reason = OwnItemsBlockedReason::Unverified(account.verification);
            self.clear_items_state();
            self.items_blocked = Some(reason);
            return Err(reason.explanation());
        }

        let summary = self
            .browsed_username
            .as_deref()
            .filter(|u| u.eq_ignore_ascii_case(&clean))
            .and_then(|_| self.user_collections.iter().find(|c| c.id == collection_id));
        let label = summary
            .map(|c| c.label.clone())
            .unwrap_or_else(|| format!("Collection #{collection_id}"));

        self.items_blocked = None;
        self.selected_collection = Some(SelectedCollection {
            source: CollectionSource::PublicUser { username: clean },
            collection_id,
            label,
            public: summary.map(|c| c.public),
            count: summary.map(|c| c.count),
        });

        Ok(self.reload_items())
    }

    /// Updates the purity mask sent to `collection_items` and reloads if a collection is active.
    pub fn set_purity(&mut self, purity: PurityMask) -> Option<CollectionItemsRequest> {
        if self.purity == purity {
            return None;
        }
        self.purity = purity;
        self.selected_collection
            .is_some()
            .then(|| self.reload_items())
    }

    /// Reloads page 1 of the currently selected collection.
    pub fn refresh_items(&mut self) -> Option<CollectionItemsRequest> {
        self.selected_collection
            .is_some()
            .then(|| self.reload_items())
    }

    /// Requests `page` unless it is page 0, already fetched, in flight, past `last_page`, or no
    /// collection is selected.
    pub fn request_page(&mut self, page: u32) -> Option<CollectionItemsRequest> {
        let selected = self.selected_collection.as_ref()?;
        if page == 0
            || self.fetched_pages.contains(&page)
            || self.items_in_flight.is_some()
            || self.last_page.is_some_and(|last| page > last)
        {
            return None;
        }
        self.items_in_flight = Some(page);
        if self.items_status == DiscoveryStatus::Idle {
            self.items_status = DiscoveryStatus::Loading;
        }
        Some(self.items_request(selected, page))
    }

    /// Requests the next page after the highest fetched page (infinite scroll).
    pub fn request_next_page(&mut self) -> Option<CollectionItemsRequest> {
        if !self.can_load_more() {
            return None;
        }
        let next = self.fetched_pages.last().copied()?.checked_add(1)?;
        self.request_page(next)
    }

    /// Retries the most recently failed collection items page.
    pub fn retry_items(&mut self) -> Option<CollectionItemsRequest> {
        let DiscoveryStatus::Failed { page, .. } = self.items_status else {
            return None;
        };
        self.selected_collection.as_ref()?;
        self.items_status = if self.fetched_pages.is_empty() {
            DiscoveryStatus::Loading
        } else {
            DiscoveryStatus::Ready
        };
        self.request_page(page)
    }

    /// Applies one fetched collection-items page after validating session scope and generation.
    pub fn apply_items_response(
        &mut self,
        session: &mut SessionManager,
        response: CollectionItemsResponse,
    ) -> ApplyOutcome {
        let CollectionItemsResponse {
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
                if generation != self.items_generation
                    || scope != self.scope
                    || self.items_in_flight != Some(page)
                {
                    return ApplyOutcome::Stale;
                }
                self.items_in_flight = None;
                match result {
                    Ok(page_data) => self.apply_page(page, page_data),
                    Err(error) => {
                        self.items_status = DiscoveryStatus::Failed {
                            page,
                            error: error.clone(),
                        };
                        ApplyOutcome::Failed(error)
                    }
                }
            }
        }
    }

    fn items_request(&self, selected: &SelectedCollection, page: u32) -> CollectionItemsRequest {
        let username = match &selected.source {
            CollectionSource::Own { username, .. } | CollectionSource::PublicUser { username } => {
                username.clone()
            }
        };
        CollectionItemsRequest {
            generation: self.items_generation,
            scope: self.scope.clone(),
            username,
            collection_id: selected.collection_id,
            purity: self.purity,
            page,
        }
    }

    fn reload_items(&mut self) -> CollectionItemsRequest {
        self.items_generation = self.items_generation.saturating_add(1);
        self.items.clear();
        self.seen.clear();
        self.fetched_pages.clear();
        self.last_page = None;
        self.total = None;
        self.items_status = DiscoveryStatus::Loading;
        self.items_in_flight = Some(1);
        let selected = self
            .selected_collection
            .as_ref()
            .expect("reload_items requires a selected collection");
        self.items_request(selected, 1)
    }

    fn apply_page(&mut self, page: u32, data: SearchPage) -> ApplyOutcome {
        self.fetched_pages.insert(page);
        self.last_page = Some(data.meta.last_page);
        self.total = Some(data.meta.total);
        let mut added = 0;
        for wallpaper in data.data {
            if self.seen.insert(wallpaper.id.clone()) {
                self.items.push(wallpaper);
                added += 1;
            }
        }
        self.items_status = if self.items.is_empty() && self.fetched_pages.len() == 1 {
            DiscoveryStatus::Empty
        } else {
            DiscoveryStatus::Ready
        };
        ApplyOutcome::Applied { added }
    }
}
