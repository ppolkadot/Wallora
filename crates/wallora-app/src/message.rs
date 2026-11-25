use std::fmt;
use std::path::PathBuf;

use iced::Size;
use wallora_core::domain::{
    AccountId, Category, Purity, SessionScope, SortOrder, Sorting, TopRange, Wallpaper, WallpaperId,
};
use wallora_core::provider::ProviderError;
use wallora_core::services::cache::{CacheError, CacheStats};
use wallora_core::services::{
    AccountRefreshPayload, CollectionItemsResponse, CollectionListResponse,
    DetailUnsupportedAction, DownloadError, DownloadOutcome, ImagePipelineError, PageResponse,
    SignInError, SignInProbe, TagMetadataResponse, UnsupportedFeature,
};

use crate::widgets::gallery_grid::GridKey;
use crate::widgets::thumbnail::LoadedThumbnail;

/// Top-level navigation destinations in the shell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Home,
    Search,
    Collections,
    Tag(u64),
    Uploader(String),
    Detail(WallpaperId),
    Account,
    Settings,
}

impl Route {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Search => "Search",
            Self::Collections => "Collections",
            Self::Tag(_) => "Tag",
            Self::Uploader(_) => "Uploader",
            Self::Detail(_) => "Wallpaper Detail",
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
    Collections,
    Tag,
    Uploader,
}

/// Identifies which detail URL was copied to the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailCopyTarget {
    Page,
    Original,
    Short,
}

impl DetailCopyTarget {
    pub fn label(self) -> &'static str {
        match self {
            Self::Page => "page URL",
            Self::Original => "original image URL",
            Self::Short => "short URL",
        }
    }
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

    // Wallpaper detail & original download
    DetailBack,
    DetailRetryMetadata,
    DetailMetadataLoaded {
        op_id: u64,
        scope: SessionScope,
        id: WallpaperId,
        result: Box<Result<Wallpaper, ProviderError>>,
    },
    DetailPreviewLoaded {
        op_id: u64,
        scope: SessionScope,
        id: WallpaperId,
        fallback_used: bool,
        result: Result<LoadedThumbnail, ImagePipelineError>,
    },
    DetailRetryPreview,
    DetailSelectTag {
        id: u64,
        name: String,
    },
    DetailSelectUploader(String),
    DetailMoreLikeThis(WallpaperId),
    DetailCopyLink {
        target: DetailCopyTarget,
        url: String,
    },
    DetailOpenHandOff(DetailUnsupportedAction),
    DetailOpenTagModerationHandOff,
    DetailDownloadOriginal,
    DetailSavePathPicked {
        op_id: u64,
        scope: SessionScope,
        id: WallpaperId,
        path: Option<PathBuf>,
    },
    DetailDownloadProgress {
        op_id: u64,
        scope: SessionScope,
        id: WallpaperId,
        bytes_written: u64,
        total_bytes: Option<u64>,
    },
    DetailDownloadFinished {
        op_id: u64,
        scope: SessionScope,
        id: WallpaperId,
        result: Result<DownloadOutcome, DownloadError>,
    },
    DetailCancelDownload,
    DetailOpenFavoriteHandOff,

    // Collections (read-only)
    CollectionsSelectOwn(u64),
    CollectionsUsernameChanged(String),
    CollectionsLoadUser,
    CollectionsRetryUserList,
    CollectionsUserListLoaded(Box<CollectionListResponse>),
    CollectionsSelectUserCollection(u64),
    CollectionsTogglePurity(Purity),
    CollectionsItemsPageLoaded(Box<CollectionItemsResponse>),
    CollectionsKeyFocusChecked {
        key: GridKey,
        input_focused: bool,
    },
    CollectionsOpenManageHandOff,

    // Tag detail & listing
    TagBack,
    TagRetryMetadata,
    TagMetadataLoaded(Box<TagMetadataResponse>),
    TagPageLoaded(Box<PageResponse>),
    TagTogglePurity(Purity),
    TagSelectSorting(Sorting),
    TagSelectOrder(SortOrder),
    TagSelectTopRange(TopRange),
    TagSearchThisTag,
    TagOpenDirectoryHandOff,

    // Uploader uploads & profile hand-offs
    UploaderBack,
    UploaderPageLoaded(Box<PageResponse>),
    UploaderTogglePurity(Purity),
    UploaderSelectSorting(Sorting),
    UploaderSelectOrder(SortOrder),
    UploaderSelectTopRange(TopRange),
    UploaderViewPublicCollections,
    UploaderSearchUploads,
    UploaderOpenProfileHandOff,
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
            Self::DetailBack => f.write_str("DetailBack"),
            Self::DetailRetryMetadata => f.write_str("DetailRetryMetadata"),
            Self::DetailMetadataLoaded {
                op_id,
                scope,
                id,
                result,
            } => f
                .debug_struct("DetailMetadataLoaded")
                .field("op_id", op_id)
                .field("scope", scope)
                .field("id", id)
                .field("ok", &result.is_ok())
                .finish(),
            Self::DetailPreviewLoaded {
                op_id,
                scope,
                id,
                fallback_used,
                result,
            } => f
                .debug_struct("DetailPreviewLoaded")
                .field("op_id", op_id)
                .field("scope", scope)
                .field("id", id)
                .field("fallback_used", fallback_used)
                .field("result", result)
                .finish(),
            Self::DetailRetryPreview => f.write_str("DetailRetryPreview"),
            Self::DetailSelectTag { id, name } => f
                .debug_struct("DetailSelectTag")
                .field("id", id)
                .field("name", name)
                .finish(),
            Self::DetailSelectUploader(username) => f
                .debug_tuple("DetailSelectUploader")
                .field(username)
                .finish(),
            Self::DetailMoreLikeThis(id) => f.debug_tuple("DetailMoreLikeThis").field(id).finish(),
            Self::DetailCopyLink { target, url } => f
                .debug_struct("DetailCopyLink")
                .field("target", target)
                .field("url", url)
                .finish(),
            Self::DetailOpenHandOff(action) => {
                f.debug_tuple("DetailOpenHandOff").field(action).finish()
            }
            Self::DetailOpenTagModerationHandOff => f.write_str("DetailOpenTagModerationHandOff"),
            Self::DetailDownloadOriginal => f.write_str("DetailDownloadOriginal"),
            Self::DetailSavePathPicked {
                op_id,
                scope,
                id,
                path,
            } => f
                .debug_struct("DetailSavePathPicked")
                .field("op_id", op_id)
                .field("scope", scope)
                .field("id", id)
                .field("picked", &path.is_some())
                .finish(),
            Self::DetailDownloadProgress {
                op_id,
                scope,
                id,
                bytes_written,
                total_bytes,
            } => f
                .debug_struct("DetailDownloadProgress")
                .field("op_id", op_id)
                .field("scope", scope)
                .field("id", id)
                .field("bytes_written", bytes_written)
                .field("total_bytes", total_bytes)
                .finish(),
            Self::DetailDownloadFinished {
                op_id,
                scope,
                id,
                result,
            } => f
                .debug_struct("DetailDownloadFinished")
                .field("op_id", op_id)
                .field("scope", scope)
                .field("id", id)
                .field("ok", &result.is_ok())
                .finish(),
            Self::DetailCancelDownload => f.write_str("DetailCancelDownload"),
            Self::DetailOpenFavoriteHandOff => f.write_str("DetailOpenFavoriteHandOff"),
            Self::CollectionsSelectOwn(id) => {
                f.debug_tuple("CollectionsSelectOwn").field(id).finish()
            }
            Self::CollectionsUsernameChanged(username) => f
                .debug_tuple("CollectionsUsernameChanged")
                .field(username)
                .finish(),
            Self::CollectionsLoadUser => f.write_str("CollectionsLoadUser"),
            Self::CollectionsRetryUserList => f.write_str("CollectionsRetryUserList"),
            Self::CollectionsUserListLoaded(response) => f
                .debug_struct("CollectionsUserListLoaded")
                .field("generation", &response.generation)
                .field("username", &response.username)
                .field("ok", &response.result.is_ok())
                .finish(),
            Self::CollectionsSelectUserCollection(id) => f
                .debug_tuple("CollectionsSelectUserCollection")
                .field(id)
                .finish(),
            Self::CollectionsTogglePurity(purity) => f
                .debug_tuple("CollectionsTogglePurity")
                .field(purity)
                .finish(),
            Self::CollectionsItemsPageLoaded(response) => f
                .debug_struct("CollectionsItemsPageLoaded")
                .field("generation", &response.generation)
                .field("page", &response.page)
                .field("ok", &response.result.is_ok())
                .finish(),
            Self::CollectionsKeyFocusChecked { key, input_focused } => f
                .debug_struct("CollectionsKeyFocusChecked")
                .field("key", key)
                .field("input_focused", input_focused)
                .finish(),
            Self::CollectionsOpenManageHandOff => f.write_str("CollectionsOpenManageHandOff"),
            Self::TagBack => f.write_str("TagBack"),
            Self::TagRetryMetadata => f.write_str("TagRetryMetadata"),
            Self::TagMetadataLoaded(response) => f
                .debug_struct("TagMetadataLoaded")
                .field("generation", &response.generation)
                .field("tag_id", &response.tag_id)
                .field("ok", &response.result.is_ok())
                .finish(),
            Self::TagPageLoaded(response) => f
                .debug_struct("TagPageLoaded")
                .field("generation", &response.generation)
                .field("page", &response.page)
                .field("ok", &response.result.is_ok())
                .finish(),
            Self::TagTogglePurity(purity) => {
                f.debug_tuple("TagTogglePurity").field(purity).finish()
            }
            Self::TagSelectSorting(sorting) => {
                f.debug_tuple("TagSelectSorting").field(sorting).finish()
            }
            Self::TagSelectOrder(order) => f.debug_tuple("TagSelectOrder").field(order).finish(),
            Self::TagSelectTopRange(range) => {
                f.debug_tuple("TagSelectTopRange").field(range).finish()
            }
            Self::TagSearchThisTag => f.write_str("TagSearchThisTag"),
            Self::TagOpenDirectoryHandOff => f.write_str("TagOpenDirectoryHandOff"),
            Self::UploaderBack => f.write_str("UploaderBack"),
            Self::UploaderPageLoaded(response) => f
                .debug_struct("UploaderPageLoaded")
                .field("generation", &response.generation)
                .field("page", &response.page)
                .field("ok", &response.result.is_ok())
                .finish(),
            Self::UploaderTogglePurity(purity) => {
                f.debug_tuple("UploaderTogglePurity").field(purity).finish()
            }
            Self::UploaderSelectSorting(sorting) => f
                .debug_tuple("UploaderSelectSorting")
                .field(sorting)
                .finish(),
            Self::UploaderSelectOrder(order) => {
                f.debug_tuple("UploaderSelectOrder").field(order).finish()
            }
            Self::UploaderSelectTopRange(range) => f
                .debug_tuple("UploaderSelectTopRange")
                .field(range)
                .finish(),
            Self::UploaderViewPublicCollections => f.write_str("UploaderViewPublicCollections"),
            Self::UploaderSearchUploads => f.write_str("UploaderSearchUploads"),
            Self::UploaderOpenProfileHandOff => f.write_str("UploaderOpenProfileHandOff"),
        }
    }
}
