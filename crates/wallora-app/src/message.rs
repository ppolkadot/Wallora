use std::fmt;

use iced::Size;
use wallora_core::domain::{
    AccountId, Category, Purity, SessionScope, SortOrder, Sorting, TopRange, WallpaperId,
};
use wallora_core::services::cache::{CacheError, CacheStats};
use wallora_core::services::{
    AccountRefreshPayload, ImagePipelineError, PageResponse, SignInError, SignInProbe,
    UnsupportedFeature,
};

use crate::widgets::gallery_grid::GridKey;
use crate::widgets::thumbnail::LoadedThumbnail;

/// Top-level navigation destinations in the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Home,
    Search,
    Account,
    Settings,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Search => "Search",
            Self::Account => "Account",
            Self::Settings => "Settings",
        }
    }
}

/// Identifies which discovery gallery a shared message targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GalleryKind {
    Home,
    Search,
}

/// UI selection state for the local proxy preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyModeChoice {
    #[default]
    System,
    Direct,
    Manual,
}

impl ProxyModeChoice {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System proxy",
            Self::Direct => "Direct (no proxy)",
            Self::Manual => "Manual proxy",
        }
    }
}

#[derive(Clone)]
pub enum Message {
    Navigate(Route),
    UsernameChanged(String),
    ApiKeyChanged(String),
    SubmitSignIn,
    SignInProbeFinished {
        op_id: u64,
        result: Result<SignInProbe, SignInError>,
    },
    RefreshAccount {
        reverify: bool,
    },
    AccountRefreshFinished {
        op_id: u64,
        payload: AccountRefreshPayload,
    },
    SignOut,
    SwitchAccount(AccountId),
    SwitchToGuest,
    ProxyModeSelected(ProxyModeChoice),
    ProxyUrlChanged(String),
    ApplyProxy,
    OpenUpstreamSettings,
    CacheCapInputChanged(String),
    ApplyCacheCap,
    CacheCapApplied {
        op_id: u64,
        cap_bytes: u64,
        result: Result<(), CacheError>,
    },
    RefreshCacheStats,
    CacheStatsLoaded(Result<CacheStats, CacheError>),

    // Discovery (Home)
    HomeSelectLatest,
    HomeSelectToplist,
    HomeSelectRandom,
    HomeSelectTopRange(TopRange),

    // Search & filter bar
    SearchQueryChanged(String),
    SearchSubmit,
    SearchClear,
    SearchToggleFiltersPanel,
    SearchToggleCategory(Category),
    SearchTogglePurity(Purity),
    SearchSelectSorting(Sorting),
    SearchSelectOrder(SortOrder),
    SearchSelectTopRange(TopRange),
    SearchAtleastChanged(String),
    SearchResolutionsChanged(String),
    SearchRatiosChanged(String),
    SearchColorsChanged(String),

    // Shared gallery & thumbnails
    GalleryRefresh(GalleryKind),
    GalleryRetry(GalleryKind),
    GallerySelectTile {
        kind: GalleryKind,
        index: usize,
    },
    GalleryScrolled {
        kind: GalleryKind,
        offset_y: f32,
        viewport_width: f32,
        viewport_height: f32,
        content_height: f32,
    },
    DiscoveryPageLoaded {
        kind: GalleryKind,
        response: Box<PageResponse>,
    },
    ThumbnailLoaded {
        kind: GalleryKind,
        generation: u64,
        scope: SessionScope,
        id: WallpaperId,
        result: Result<LoadedThumbnail, ImagePipelineError>,
    },
    RetryThumbnail {
        kind: GalleryKind,
        id: WallpaperId,
    },
    OpenUnsupportedFeature(UnsupportedFeature),
    WindowResized(Size),
    RuntimeGalleryKey(GridKey),
    SearchKeyFocusChecked {
        key: GridKey,
        any_input_focused: bool,
    },
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Navigate(route) => f.debug_tuple("Navigate").field(route).finish(),
            Self::UsernameChanged(u) => f.debug_tuple("UsernameChanged").field(u).finish(),
            // Never log raw API key input.
            Self::ApiKeyChanged(_) => f.write_str("ApiKeyChanged(\"***\")"),
            Self::SubmitSignIn => f.write_str("SubmitSignIn"),
            Self::SignInProbeFinished { op_id, result } => f
                .debug_struct("SignInProbeFinished")
                .field("op_id", op_id)
                .field("result", result)
                .finish(),
            Self::RefreshAccount { reverify } => f
                .debug_struct("RefreshAccount")
                .field("reverify", reverify)
                .finish(),
            Self::AccountRefreshFinished { op_id, .. } => f
                .debug_struct("AccountRefreshFinished")
                .field("op_id", op_id)
                .finish(),
            Self::SignOut => f.write_str("SignOut"),
            Self::SwitchAccount(id) => f.debug_tuple("SwitchAccount").field(id).finish(),
            Self::SwitchToGuest => f.write_str("SwitchToGuest"),
            Self::ProxyModeSelected(mode) => {
                f.debug_tuple("ProxyModeSelected").field(mode).finish()
            }
            Self::ProxyUrlChanged(url) => f.debug_tuple("ProxyUrlChanged").field(url).finish(),
            Self::ApplyProxy => f.write_str("ApplyProxy"),
            Self::OpenUpstreamSettings => f.write_str("OpenUpstreamSettings"),
            Self::CacheCapInputChanged(input) => {
                f.debug_tuple("CacheCapInputChanged").field(input).finish()
            }
            Self::ApplyCacheCap => f.write_str("ApplyCacheCap"),
            Self::CacheCapApplied {
                op_id,
                cap_bytes,
                result,
            } => f
                .debug_struct("CacheCapApplied")
                .field("op_id", op_id)
                .field("cap_bytes", cap_bytes)
                .field("result", result)
                .finish(),
            Self::RefreshCacheStats => f.write_str("RefreshCacheStats"),
            Self::CacheStatsLoaded(result) => {
                f.debug_tuple("CacheStatsLoaded").field(result).finish()
            }
            Self::HomeSelectLatest => f.write_str("HomeSelectLatest"),
            Self::HomeSelectToplist => f.write_str("HomeSelectToplist"),
            Self::HomeSelectRandom => f.write_str("HomeSelectRandom"),
            Self::HomeSelectTopRange(range) => {
                f.debug_tuple("HomeSelectTopRange").field(range).finish()
            }
            Self::SearchQueryChanged(q) => f.debug_tuple("SearchQueryChanged").field(q).finish(),
            Self::SearchSubmit => f.write_str("SearchSubmit"),
            Self::SearchClear => f.write_str("SearchClear"),
            Self::SearchToggleFiltersPanel => f.write_str("SearchToggleFiltersPanel"),
            Self::SearchToggleCategory(cat) => {
                f.debug_tuple("SearchToggleCategory").field(cat).finish()
            }
            Self::SearchTogglePurity(purity) => {
                f.debug_tuple("SearchTogglePurity").field(purity).finish()
            }
            Self::SearchSelectSorting(sorting) => {
                f.debug_tuple("SearchSelectSorting").field(sorting).finish()
            }
            Self::SearchSelectOrder(order) => {
                f.debug_tuple("SearchSelectOrder").field(order).finish()
            }
            Self::SearchSelectTopRange(range) => {
                f.debug_tuple("SearchSelectTopRange").field(range).finish()
            }
            Self::SearchAtleastChanged(v) => {
                f.debug_tuple("SearchAtleastChanged").field(v).finish()
            }
            Self::SearchResolutionsChanged(v) => {
                f.debug_tuple("SearchResolutionsChanged").field(v).finish()
            }
            Self::SearchRatiosChanged(v) => f.debug_tuple("SearchRatiosChanged").field(v).finish(),
            Self::SearchColorsChanged(v) => f.debug_tuple("SearchColorsChanged").field(v).finish(),
            Self::GalleryRefresh(kind) => f.debug_tuple("GalleryRefresh").field(kind).finish(),
            Self::GalleryRetry(kind) => f.debug_tuple("GalleryRetry").field(kind).finish(),
            Self::GallerySelectTile { kind, index } => f
                .debug_struct("GallerySelectTile")
                .field("kind", kind)
                .field("index", index)
                .finish(),
            Self::GalleryScrolled {
                kind,
                offset_y,
                viewport_width,
                viewport_height,
                content_height,
            } => f
                .debug_struct("GalleryScrolled")
                .field("kind", kind)
                .field("offset_y", offset_y)
                .field("viewport_width", viewport_width)
                .field("viewport_height", viewport_height)
                .field("content_height", content_height)
                .finish(),
            Self::DiscoveryPageLoaded { kind, response } => f
                .debug_struct("DiscoveryPageLoaded")
                .field("kind", kind)
                .field("generation", &response.generation)
                .field("page", &response.page)
                .field("ok", &response.result.is_ok())
                .finish(),
            Self::ThumbnailLoaded {
                kind,
                generation,
                scope,
                id,
                result,
            } => f
                .debug_struct("ThumbnailLoaded")
                .field("kind", kind)
                .field("generation", generation)
                .field("scope", scope)
                .field("id", id)
                .field("result", result)
                .finish(),
            Self::RetryThumbnail { kind, id } => f
                .debug_struct("RetryThumbnail")
                .field("kind", kind)
                .field("id", id)
                .finish(),
            Self::OpenUnsupportedFeature(feature) => f
                .debug_tuple("OpenUnsupportedFeature")
                .field(feature)
                .finish(),
            Self::WindowResized(size) => f.debug_tuple("WindowResized").field(size).finish(),
            Self::RuntimeGalleryKey(key) => f.debug_tuple("RuntimeGalleryKey").field(key).finish(),
            Self::SearchKeyFocusChecked {
                key,
                any_input_focused,
            } => f
                .debug_struct("SearchKeyFocusChecked")
                .field("key", key)
                .field("any_input_focused", any_input_focused)
                .finish(),
        }
    }
}
