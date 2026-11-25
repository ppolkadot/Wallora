use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use iced::futures::SinkExt;
use iced::task::Handle;
use iced::widget::operation::{self, AbsoluteOffset};
use iced::widget::{Id, button, column, container, image as iced_image, row, text};
use iced::{Alignment, Background, Border, Element, Length, Size, Subscription, Task, Theme};
use wallora_core::domain::{
    BrowseDefaults, CacheCapError, Category, CredentialStore, DEFAULT_CACHE_CAP_BYTES,
    HARD_MAX_CACHE_CAP_BYTES, ProxyConfig, Purity, PurityMask, QueryToken, SearchTerms,
    SecretString, SessionKind, SessionScope, Sorting, Thumbs, TopRange, WALLHAVEN_SETTINGS_URL,
    Wallpaper, WallpaperId, validate_username,
};
use wallora_core::provider::client::PREVIEW_BYTE_LIMIT;
use wallora_core::provider::{Provider, ProviderError, WallhavenClient, WallhavenClientConfig};
use wallora_core::services::cache::{
    CacheConfig, CacheError, CacheStats, CacheStore, CancelToken, NoopSessionCacheSink,
    SessionCacheSink,
};
use wallora_core::services::{
    ApplyOutcome, CollectionItemsRequest, CollectionListOutcome, CollectionListRequest,
    CollectionUnsupportedAction, CollectionsController, DiscoveryController, DiscoveryFilters,
    DiscoveryStatus, DiscoveryView, DownloadError, DownloadRequest, FilePublisher, ImageGuard,
    ImageGuardError, ImagePipeline, ImagePipelineError, ImageRequest, NoticeLevel,
    OriginalDownloader, PREVIEW_MAX_PIXELS, PREVIEW_MAX_SIDE, PageRequest,
    ProfileTagUnsupportedAction, ScopedResponse, SessionManager, SessionNotice,
    SignInCommitOutcome, SignInError, TagLoadRequests, TagMetadataOutcome, TagMetadataRequest,
    TagMetadataStatus, TagPageController, UploaderController, fetch_account_refresh,
    fetch_collection_items, fetch_page, fetch_tag_metadata, fetch_user_collections, load_uncached,
    probe_sign_in, selectable_purity,
};
use wallora_core::storage::{AppPaths, SessionStorage, SqliteStore, StorageError};
use wallora_core::util::default_download_filename;
use wallora_windows::{WindowsCredentialStore, WindowsFilePublisher};

use crate::cache_host::CacheHost;
use crate::message::{GalleryKind, Message, ProxyModeChoice, Route};
use crate::theme::{palette, surface_color, wallora_theme};
use crate::views;
use crate::views::collections::COLLECTIONS_SIDEBAR_WIDTH;
use crate::views::detail::{DetailDownloadView, DetailPreviewView, DetailViewState};
use crate::views::search::{
    SearchInputIds, format_ratios, format_resolutions, parse_atleast_input, parse_color_input,
    parse_ratios_input, parse_resolutions_input,
};
use crate::views::settings::{
    describe_cache_cap_error, format_cap_gb, format_size, parse_cache_cap_gb,
};
use crate::widgets::gallery_grid::{GridKey, GridMetrics};
use crate::widgets::thumbnail::{LoadedThumbnail, ThumbnailViewState, select_thumbnail_url};

const MAX_RESIDENT_THUMBNAILS: usize = 256;
const MAX_NAV_STACK: usize = 32;

struct InFlight {
    id: u64,
    scope: SessionScope,
    handle: Handle,
}

#[derive(Clone)]
enum ThumbnailLoader {
    Cached(ImagePipeline<WallhavenClient>),
    Uncached {
        client: Arc<WallhavenClient>,
        guard: ImageGuard,
    },
}

impl ThumbnailLoader {
    fn from_runtime(
        client: Arc<WallhavenClient>,
        cache: Option<CacheStore>,
        guard: ImageGuard,
    ) -> Self {
        match cache {
            Some(store) => Self::Cached(ImagePipeline::new(client, store, guard)),
            None => Self::Uncached { client, guard },
        }
    }

    async fn load(
        &self,
        request: ImageRequest,
        cancel: CancelToken,
    ) -> Result<LoadedThumbnail, ImagePipelineError> {
        let loaded = match self {
            Self::Cached(pipeline) => pipeline.load(request, cancel).await?,
            Self::Uncached { client, guard } => {
                load_uncached(client.as_ref(), guard, request, cancel).await?
            }
        };
        let (width, height) = loaded.image.dimensions();
        let rgba = loaded.image.rgba().to_vec();
        let stale = loaded.stale;
        drop(loaded);
        Ok(LoadedThumbnail {
            handle: iced_image::Handle::from_rgba(width, height, rgba),
            stale,
        })
    }
}

struct InFlightPage {
    generation: u64,
    scope: SessionScope,
    page: u32,
    handle: Handle,
}

enum ThumbnailEntry {
    InFlight {
        generation: u64,
        scope: SessionScope,
        cancel: CancelToken,
        handle: Handle,
    },
    Loaded {
        handle: iced_image::Handle,
        stale: bool,
    },
    Failed {
        message: String,
    },
}

struct InFlightCollectionList {
    generation: u64,
    scope: SessionScope,
    handle: Handle,
}

struct InFlightTagMetadata {
    generation: u64,
    scope: SessionScope,
    tag_id: u64,
    handle: Handle,
}

fn reset_scroll_task(scrollable_id: &Id, metrics: &mut GridMetrics) -> Task<Message> {
    let was_scrolled = metrics.scroll_y > 0.0;
    metrics.scroll_y = 0.0;
    if was_scrolled {
        operation::scroll_to(
            scrollable_id.clone(),
            AbsoluteOffset {
                x: Some(0.0),
                y: Some(0.0),
            },
        )
    } else {
        Task::none()
    }
}

fn toggle_purity_mask(mut mask: PurityMask, purity: Purity) -> PurityMask {
    match purity {
        Purity::Sfw => mask.sfw = !mask.sfw,
        Purity::Sketchy => mask.sketchy = !mask.sketchy,
        Purity::Nsfw => mask.nsfw = !mask.nsfw,
    }
    mask
}

fn entry_thumbnail_state<'a>(
    thumbnails: &'a HashMap<WallpaperId, ThumbnailEntry>,
    id: &WallpaperId,
) -> ThumbnailViewState<'a> {
    match thumbnails.get(id) {
        Some(ThumbnailEntry::Loaded { handle, stale }) => ThumbnailViewState::Loaded {
            handle,
            stale: *stale,
        },
        Some(ThumbnailEntry::Failed { message }) => ThumbnailViewState::Failed {
            message: message.as_str(),
        },
        Some(ThumbnailEntry::InFlight { .. }) | None => ThumbnailViewState::Loading,
    }
}

fn abort_in_flight_thumbnails(thumbnails: &mut HashMap<WallpaperId, ThumbnailEntry>) {
    thumbnails.retain(|_, entry| match entry {
        ThumbnailEntry::InFlight { cancel, handle, .. } => {
            cancel.cancel();
            handle.abort();
            false
        }
        ThumbnailEntry::Loaded { .. } | ThumbnailEntry::Failed { .. } => true,
    });
}

fn evict_excess_loaded_thumbnails(
    thumbnails: &mut HashMap<WallpaperId, ThumbnailEntry>,
    items: &[Wallpaper],
    visible_range: &std::ops::Range<usize>,
) {
    let mut loaded_count = thumbnails
        .values()
        .filter(|e| matches!(e, ThumbnailEntry::Loaded { .. }))
        .count();
    if loaded_count <= MAX_RESIDENT_THUMBNAILS {
        return;
    }

    let mid = (visible_range.start + visible_range.end) / 2;
    let mut candidates: Vec<(usize, WallpaperId)> = items
        .iter()
        .enumerate()
        .filter(|(idx, w)| {
            !visible_range.contains(idx)
                && matches!(thumbnails.get(&w.id), Some(ThumbnailEntry::Loaded { .. }))
        })
        .map(|(idx, w)| (idx.abs_diff(mid), w.id.clone()))
        .collect();
    candidates.sort_by_key(|a| std::cmp::Reverse(a.0));

    for (_, id) in candidates {
        if loaded_count <= MAX_RESIDENT_THUMBNAILS {
            break;
        }
        if matches!(thumbnails.get(&id), Some(ThumbnailEntry::Loaded { .. })) {
            thumbnails.remove(&id);
            loaded_count -= 1;
        }
    }
}

struct ThumbnailSyncTarget<'a> {
    kind: GalleryKind,
    items: &'a [Wallpaper],
    generation: u64,
    scope: SessionScope,
    metrics: &'a GridMetrics,
    thumbnails: &'a mut HashMap<WallpaperId, ThumbnailEntry>,
}

fn sync_gallery_visible_thumbnails(
    target: ThumbnailSyncTarget<'_>,
    loader: &ThumbnailLoader,
    thumb_size: &str,
) -> Task<Message> {
    let ThumbnailSyncTarget {
        kind,
        items,
        generation,
        scope,
        metrics,
        thumbnails,
    } = target;
    if items.is_empty() {
        abort_in_flight_thumbnails(thumbnails);
        return Task::none();
    }

    let visible_range = metrics.visible_item_range(items.len());
    let visible_ids: HashSet<WallpaperId> = items[visible_range.clone()]
        .iter()
        .map(|w| w.id.clone())
        .collect();

    // Cancel and remove any in-flight thumbnails outside the visible + overscan window or
    // belonging to an older scope/generation.
    thumbnails.retain(|id, entry| match entry {
        ThumbnailEntry::InFlight {
            generation: g,
            scope: s,
            cancel,
            handle,
        } if *g != generation || *s != scope || !visible_ids.contains(id) => {
            cancel.cancel();
            handle.abort();
            false
        }
        _ => true,
    });

    evict_excess_loaded_thumbnails(thumbnails, items, &visible_range);

    let mut tasks = Vec::new();
    for wallpaper in &items[visible_range] {
        if thumbnails.contains_key(&wallpaper.id) {
            continue;
        }
        let raw_url = select_thumbnail_url(&wallpaper.thumbs, thumb_size);
        let request = match ImageRequest::thumbnail_url(raw_url, scope.clone()) {
            Ok(req) => req,
            Err(err) => {
                thumbnails.insert(
                    wallpaper.id.clone(),
                    ThumbnailEntry::Failed {
                        message: thumbnail_error_message(&err),
                    },
                );
                continue;
            }
        };

        let cancel = CancelToken::new();
        let cancel_for_task = cancel.clone();
        let loader = loader.clone();
        let id = wallpaper.id.clone();
        let msg_id = id.clone();
        let msg_scope = scope.clone();

        let (task, handle) = Task::perform(
            async move { loader.load(request, cancel_for_task).await },
            move |result| Message::ThumbnailLoaded {
                kind,
                generation,
                scope: msg_scope,
                id: msg_id,
                result,
            },
        )
        .abortable();

        thumbnails.insert(
            id,
            ThumbnailEntry::InFlight {
                generation,
                scope: scope.clone(),
                cancel,
                handle: handle.abort_on_drop(),
            },
        );
        tasks.push(task);
    }

    Task::batch(tasks)
}

fn apply_thumbnail_loaded(
    current_scope: &SessionScope,
    target_generation: u64,
    generation: u64,
    scope: SessionScope,
    id: WallpaperId,
    result: Result<LoadedThumbnail, ImagePipelineError>,
    thumbnails: &mut HashMap<WallpaperId, ThumbnailEntry>,
) {
    if &scope != current_scope || generation != target_generation {
        return;
    }
    let matches_in_flight = matches!(
        thumbnails.get(&id),
        Some(ThumbnailEntry::InFlight {
            generation: g,
            scope: s,
            ..
        }) if *g == generation && *s == scope
    );
    if !matches_in_flight {
        return;
    }
    match result {
        Ok(loaded) => {
            thumbnails.insert(
                id,
                ThumbnailEntry::Loaded {
                    handle: loaded.handle,
                    stale: loaded.stale,
                },
            );
        }
        Err(ImagePipelineError::Cancelled | ImagePipelineError::ScopeChanged) => {
            thumbnails.remove(&id);
        }
        Err(err) => {
            thumbnails.insert(
                id,
                ThumbnailEntry::Failed {
                    message: thumbnail_error_message(&err),
                },
            );
        }
    }
}

struct GalleryState {
    kind: GalleryKind,
    controller: DiscoveryController,
    scrollable_id: Id,
    metrics: GridMetrics,
    selected: Option<usize>,
    page_in_flight: Option<InFlightPage>,
    thumbnails: HashMap<WallpaperId, ThumbnailEntry>,
}

impl GalleryState {
    fn new(kind: GalleryKind, controller: DiscoveryController) -> Self {
        Self {
            kind,
            controller,
            scrollable_id: Id::unique(),
            metrics: GridMetrics::default(),
            selected: None,
            page_in_flight: None,
            thumbnails: HashMap::new(),
        }
    }

    fn thumbnail_state(&self, id: &WallpaperId) -> ThumbnailViewState<'_> {
        entry_thumbnail_state(&self.thumbnails, id)
    }

    fn cancel_in_flight_thumbnails(&mut self) {
        abort_in_flight_thumbnails(&mut self.thumbnails);
    }

    fn clear_all_work(&mut self) {
        if let Some(op) = self.page_in_flight.take() {
            op.handle.abort();
        }
        self.cancel_in_flight_thumbnails();
        self.thumbnails.clear();
        self.selected = None;
    }

    fn clear_for_reload(&mut self) -> Task<Message> {
        self.clear_all_work();
        let was_scrolled = self.metrics.scroll_y > 0.0;
        self.metrics.scroll_y = 0.0;
        if was_scrolled {
            operation::scroll_to(
                self.scrollable_id.clone(),
                AbsoluteOffset {
                    x: Some(0.0),
                    y: Some(0.0),
                },
            )
        } else {
            Task::none()
        }
    }

    fn reset_for_scope(
        &mut self,
        scope: SessionScope,
        view: DiscoveryView,
        filters: DiscoveryFilters,
        clear_terms: bool,
    ) {
        self.clear_all_work();
        self.metrics.scroll_y = 0.0;
        self.controller
            .reset_for_scope(scope, view, filters, clear_terms);
    }

    fn sync_visible_thumbnails(
        &mut self,
        loader: &ThumbnailLoader,
        thumb_size: &str,
    ) -> Task<Message> {
        let generation = self.controller.generation();
        let scope = self.controller.scope().clone();
        sync_gallery_visible_thumbnails(
            ThumbnailSyncTarget {
                kind: self.kind,
                items: self.controller.items(),
                generation,
                scope,
                metrics: &self.metrics,
                thumbnails: &mut self.thumbnails,
            },
            loader,
            thumb_size,
        )
    }
}

struct CollectionsState {
    controller: CollectionsController,
    scrollable_id: Id,
    metrics: GridMetrics,
    selected: Option<usize>,
    list_in_flight: Option<InFlightCollectionList>,
    page_in_flight: Option<InFlightPage>,
    thumbnails: HashMap<WallpaperId, ThumbnailEntry>,
}

impl CollectionsState {
    fn new(controller: CollectionsController) -> Self {
        let mut metrics = GridMetrics::default();
        metrics.set_window_size(
            (metrics.window_width - COLLECTIONS_SIDEBAR_WIDTH - 16.0).max(320.0),
            800.0,
        );
        Self {
            controller,
            scrollable_id: Id::unique(),
            metrics,
            selected: None,
            list_in_flight: None,
            page_in_flight: None,
            thumbnails: HashMap::new(),
        }
    }

    fn thumbnail_state(&self, id: &WallpaperId) -> ThumbnailViewState<'_> {
        entry_thumbnail_state(&self.thumbnails, id)
    }

    fn cancel_in_flight_thumbnails(&mut self) {
        abort_in_flight_thumbnails(&mut self.thumbnails);
    }

    fn clear_items_work(&mut self) {
        if let Some(op) = self.page_in_flight.take() {
            op.handle.abort();
        }
        self.cancel_in_flight_thumbnails();
        self.thumbnails.clear();
        self.selected = None;
        self.metrics.scroll_y = 0.0;
    }

    fn clear_items_for_reload(&mut self) -> Task<Message> {
        let was_scrolled = self.metrics.scroll_y > 0.0;
        self.clear_items_work();
        if was_scrolled {
            operation::scroll_to(
                self.scrollable_id.clone(),
                AbsoluteOffset {
                    x: Some(0.0),
                    y: Some(0.0),
                },
            )
        } else {
            Task::none()
        }
    }

    fn clear_all_work(&mut self) {
        if let Some(op) = self.list_in_flight.take() {
            op.handle.abort();
        }
        self.clear_items_work();
    }

    fn sync_visible_thumbnails(
        &mut self,
        loader: &ThumbnailLoader,
        thumb_size: &str,
    ) -> Task<Message> {
        let generation = self.controller.items_generation();
        let scope = self.controller.scope().clone();
        sync_gallery_visible_thumbnails(
            ThumbnailSyncTarget {
                kind: GalleryKind::Collections,
                items: self.controller.items(),
                generation,
                scope,
                metrics: &self.metrics,
                thumbnails: &mut self.thumbnails,
            },
            loader,
            thumb_size,
        )
    }
}

struct TagState {
    controller: TagPageController,
    scrollable_id: Id,
    metrics: GridMetrics,
    selected: Option<usize>,
    metadata_in_flight: Option<InFlightTagMetadata>,
    page_in_flight: Option<InFlightPage>,
    thumbnails: HashMap<WallpaperId, ThumbnailEntry>,
}

impl TagState {
    fn new(controller: TagPageController) -> Self {
        Self {
            controller,
            scrollable_id: Id::unique(),
            metrics: GridMetrics::default(),
            selected: None,
            metadata_in_flight: None,
            page_in_flight: None,
            thumbnails: HashMap::new(),
        }
    }

    fn thumbnail_state(&self, id: &WallpaperId) -> ThumbnailViewState<'_> {
        entry_thumbnail_state(&self.thumbnails, id)
    }

    fn cancel_in_flight_thumbnails(&mut self) {
        abort_in_flight_thumbnails(&mut self.thumbnails);
    }

    fn clear_items_work(&mut self) {
        if let Some(op) = self.page_in_flight.take() {
            op.handle.abort();
        }
        self.cancel_in_flight_thumbnails();
        self.thumbnails.clear();
        self.selected = None;
    }

    fn clear_items_for_reload(&mut self) -> Task<Message> {
        self.clear_items_work();
        reset_scroll_task(&self.scrollable_id, &mut self.metrics)
    }

    fn clear_all_work(&mut self) {
        if let Some(op) = self.metadata_in_flight.take() {
            op.handle.abort();
        }
        self.clear_items_work();
        self.metrics.scroll_y = 0.0;
    }

    fn sync_visible_thumbnails(
        &mut self,
        loader: &ThumbnailLoader,
        thumb_size: &str,
    ) -> Task<Message> {
        let generation = self.controller.items_generation();
        let scope = self.controller.scope().clone();
        sync_gallery_visible_thumbnails(
            ThumbnailSyncTarget {
                kind: GalleryKind::Tag,
                items: self.controller.items(),
                generation,
                scope,
                metrics: &self.metrics,
                thumbnails: &mut self.thumbnails,
            },
            loader,
            thumb_size,
        )
    }
}

struct UploaderState {
    controller: UploaderController,
    scrollable_id: Id,
    metrics: GridMetrics,
    selected: Option<usize>,
    page_in_flight: Option<InFlightPage>,
    thumbnails: HashMap<WallpaperId, ThumbnailEntry>,
}

impl UploaderState {
    fn new(controller: UploaderController) -> Self {
        Self {
            controller,
            scrollable_id: Id::unique(),
            metrics: GridMetrics::default(),
            selected: None,
            page_in_flight: None,
            thumbnails: HashMap::new(),
        }
    }

    fn thumbnail_state(&self, id: &WallpaperId) -> ThumbnailViewState<'_> {
        entry_thumbnail_state(&self.thumbnails, id)
    }

    fn cancel_in_flight_thumbnails(&mut self) {
        abort_in_flight_thumbnails(&mut self.thumbnails);
    }

    fn clear_all_work(&mut self) {
        if let Some(op) = self.page_in_flight.take() {
            op.handle.abort();
        }
        self.cancel_in_flight_thumbnails();
        self.thumbnails.clear();
        self.selected = None;
        self.metrics.scroll_y = 0.0;
    }

    fn clear_for_reload(&mut self) -> Task<Message> {
        if let Some(op) = self.page_in_flight.take() {
            op.handle.abort();
        }
        self.cancel_in_flight_thumbnails();
        self.thumbnails.clear();
        self.selected = None;
        reset_scroll_task(&self.scrollable_id, &mut self.metrics)
    }

    fn sync_visible_thumbnails(
        &mut self,
        loader: &ThumbnailLoader,
        thumb_size: &str,
    ) -> Task<Message> {
        let generation = self.controller.generation();
        let scope = self.controller.scope().clone();
        sync_gallery_visible_thumbnails(
            ThumbnailSyncTarget {
                kind: GalleryKind::Uploader,
                items: self.controller.items(),
                generation,
                scope,
                metrics: &self.metrics,
                thumbnails: &mut self.thumbnails,
            },
            loader,
            thumb_size,
        )
    }
}

fn thumbnail_error_message(error: &ImagePipelineError) -> String {
    match error {
        ImagePipelineError::Provider(err) => err.user_message(),
        ImagePipelineError::Decode(_) => "Invalid or oversized thumbnail".to_string(),
        ImagePipelineError::Cache(err) => format!("Cache error: {err}"),
        ImagePipelineError::Cancelled | ImagePipelineError::ScopeChanged => error.to_string(),
    }
}

fn gallery_keyboard_event(
    event: iced::Event,
    status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    if status == iced::event::Status::Captured {
        return None;
    }
    if let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) = event
        && !modifiers.control()
        && !modifiers.alt()
        && !modifiers.logo()
    {
        return GridKey::from_iced_key(&key).map(Message::RuntimeGalleryKey);
    }
    None
}

enum DetailMetadataState {
    Loading {
        summary: Option<Wallpaper>,
        op_id: u64,
        handle: Handle,
    },
    Loaded(Box<Wallpaper>),
    Failed {
        summary: Option<Wallpaper>,
        error: ProviderError,
    },
}

enum DetailPreviewState {
    Idle,
    Loading {
        op_id: u64,
        scope: SessionScope,
        fallback_used: bool,
        cancel: CancelToken,
        handle: Handle,
    },
    Loaded {
        handle: iced_image::Handle,
        stale: bool,
        fallback_used: bool,
    },
    Failed {
        message: String,
    },
}

enum DetailDownloadState {
    Idle,
    PickingLocation {
        op_id: u64,
        scope: SessionScope,
        handle: Handle,
    },
    Downloading {
        op_id: u64,
        scope: SessionScope,
        target: PathBuf,
        bytes_written: u64,
        total_bytes: Option<u64>,
        cancel: CancelToken,
        handle: Handle,
    },
    Saved {
        path: PathBuf,
        bytes_written: u64,
        collision_renamed: bool,
    },
    Failed {
        message: String,
        part_path: Option<PathBuf>,
    },
}

struct DetailState {
    wallpaper_id: WallpaperId,
    return_to: GalleryKind,
    metadata: DetailMetadataState,
    preview: DetailPreviewState,
    download: DetailDownloadState,
    copy_feedback: Option<String>,
}

impl DetailState {
    fn wallpaper(&self) -> Option<&Wallpaper> {
        match &self.metadata {
            DetailMetadataState::Loaded(wallpaper) => Some(wallpaper),
            DetailMetadataState::Loading { summary, .. }
            | DetailMetadataState::Failed { summary, .. } => summary.as_ref(),
        }
    }

    fn full_wallpaper(&self) -> Option<&Wallpaper> {
        match &self.metadata {
            DetailMetadataState::Loaded(wallpaper) => Some(wallpaper),
            _ => None,
        }
    }

    fn cancel_preview(&mut self) {
        if let DetailPreviewState::Loading { cancel, handle, .. } = &self.preview {
            cancel.cancel();
            handle.abort();
        }
        self.preview = DetailPreviewState::Idle;
    }

    fn cancel_all(&mut self) {
        if let DetailMetadataState::Loading { handle, .. } = &self.metadata {
            handle.abort();
        }
        self.cancel_preview();
        match &self.download {
            DetailDownloadState::PickingLocation { handle, .. } => handle.abort(),
            DetailDownloadState::Downloading { cancel, handle, .. } => {
                cancel.cancel();
                handle.abort();
            }
            DetailDownloadState::Idle
            | DetailDownloadState::Saved { .. }
            | DetailDownloadState::Failed { .. } => {}
        }
        self.download = DetailDownloadState::Idle;
    }
}

fn preview_exceeds_budget(wallpaper: &Wallpaper) -> bool {
    let pixels = u64::from(wallpaper.dimension_x).saturating_mul(u64::from(wallpaper.dimension_y));
    wallpaper.file_size > PREVIEW_BYTE_LIMIT as u64
        || wallpaper.dimension_x > PREVIEW_MAX_SIDE
        || wallpaper.dimension_y > PREVIEW_MAX_SIDE
        || pixels > PREVIEW_MAX_PIXELS
}

fn is_preview_budget_error(error: &ImagePipelineError) -> bool {
    matches!(
        error,
        ImagePipelineError::Provider(ProviderError::ImageTooLarge { .. })
            | ImagePipelineError::Decode(
                ImageGuardError::EncodedTooLarge { .. }
                    | ImageGuardError::DimensionsExceeded { .. }
                    | ImageGuardError::MegapixelsExceeded { .. }
                    | ImageGuardError::DecodedBudgetExceeded { .. }
            )
    )
}

fn select_fallback_preview_url(thumbs: &Thumbs) -> &str {
    if !thumbs.original.is_empty() {
        thumbs.original.as_str()
    } else if !thumbs.large.is_empty() {
        thumbs.large.as_str()
    } else {
        thumbs.small.as_str()
    }
}

fn detail_keyboard_event(
    event: iced::Event,
    status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    if status == iced::event::Status::Captured {
        return None;
    }
    if let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) = event
        && !modifiers.control()
        && !modifiers.alt()
        && !modifiers.logo()
        && key == iced::keyboard::Key::Named(iced::keyboard::key::Named::Escape)
    {
        return Some(Message::DetailBack);
    }
    None
}

async fn pick_download_destination(default_name: String) -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Save original wallpaper")
        .set_file_name(default_name)
        .save_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}

pub struct WalloraApp {
    route: Route,
    nav_stack: Vec<Route>,
    paths: AppPaths,
    client: Arc<WallhavenClient>,
    session: SessionManager,
    username_input: String,
    api_key_input: SecretString,
    next_op_id: u64,
    sign_in_in_flight: Option<InFlight>,
    refresh_in_flight: Option<InFlight>,
    proxy_mode: ProxyModeChoice,
    proxy_url_input: String,
    proxy_feedback: Option<(bool, String)>,
    cache: Option<CacheStore>,
    image_guard: ImageGuard,
    thumbnail_loader: ThumbnailLoader,
    file_publisher: Arc<dyn FilePublisher>,
    cache_cap_input: String,
    cache_cap_feedback: Option<(bool, String)>,
    cache_cap_in_flight: Option<u64>,
    cache_stats: Option<CacheStats>,
    launch_browser: fn(&str) -> Result<(), String>,
    home: GalleryState,
    home_toplist_range: TopRange,
    search: GalleryState,
    search_query_input: String,
    search_atleast_input: String,
    search_resolutions_input: String,
    search_ratios_input: String,
    search_colors_input: String,
    search_filter_error: Option<String>,
    search_filters_expanded: bool,
    search_input_ids: SearchInputIds,
    collections: CollectionsState,
    collections_username_input: String,
    collections_username_input_id: Id,
    collections_username_error: Option<String>,
    tag: TagState,
    uploader: UploaderState,
    detail: Option<DetailState>,
}

fn cache_open_warning(error: &CacheError) -> &'static str {
    match error {
        CacheError::Locked => {
            "Another Wallora window is using the image cache, so images will not be cached in this window."
        }
        _ => "The image cache could not be opened, so images will not be cached in this window.",
    }
}

impl WalloraApp {
    pub fn boot(paths: AppPaths, cache_host: Option<Arc<CacheHost>>) -> (Self, Task<Message>) {
        let mut startup_warnings: Vec<&'static str> = Vec::new();
        let storage: Arc<dyn SessionStorage> = match SqliteStore::open(&paths.db_path()) {
            Ok(db) => Arc::new(db),
            Err(err) => {
                tracing::error!(reason = %err, "failed to open SQLite database; falling back to in-memory storage");
                startup_warnings.push(match err {
                    StorageError::SessionOwned => {
                        "Another Wallora window owns local account storage. Close it and restart this window to sign in."
                    }
                    _ => {
                        "Could not open local SQLite storage. Sign-in is unavailable in this window."
                    }
                });
                Arc::new(
                    SqliteStore::open_in_memory()
                        .expect("in-memory SQLite fallback must open cleanly"),
                )
            }
        };

        let credentials: Arc<dyn CredentialStore> = Arc::new(WindowsCredentialStore::new());

        // Open the cache with the persisted cap before restoring the session so the restore can
        // notify it. The cache starts at generation 0 because a restored session is generation 1
        // and the cache actor ignores scope transitions that do not advance the generation.
        let cache_cap = storage
            .load_local_preferences()
            .map(|prefs| prefs.cache_cap_bytes)
            .unwrap_or_else(|err| {
                tracing::warn!(reason = %err, "could not read the cache cap preference; using the default");
                DEFAULT_CACHE_CAP_BYTES
            });
        let cache = match cache_host.as_deref() {
            Some(host) => {
                let opened = CacheConfig::new(paths.cache_dir.clone(), SessionScope::guest(0))
                    .with_cap_bytes(cache_cap)
                    .and_then(|config| host.open(config));
                match opened {
                    Ok(store) => Some(store),
                    Err(err) => {
                        tracing::warn!(reason = %err, "image cache unavailable; continuing without it");
                        startup_warnings.push(cache_open_warning(&err));
                        None
                    }
                }
            }
            None => {
                startup_warnings.push(
                    "The image cache could not be started, so images will not be cached in this window.",
                );
                None
            }
        };
        let cache_sink: Arc<dyn SessionCacheSink> = match &cache {
            Some(store) => Arc::new(store.clone()),
            None => Arc::new(NoopSessionCacheSink),
        };
        let mut session =
            SessionManager::restore_with_cache(storage, Arc::clone(&credentials), cache_sink);
        if session.notice().is_none() && !startup_warnings.is_empty() {
            session.set_notice(Some(SessionNotice::warning(startup_warnings.join(" "))));
        }

        let initial_proxy = session.local_preferences().proxy.clone();
        let config = WallhavenClientConfig::production().with_proxy(initial_proxy);
        let client = match WallhavenClient::with_config(Arc::clone(&credentials), config) {
            Ok(client) => Arc::new(client),
            Err(_) => {
                tracing::warn!("saved proxy failed transport build; falling back to system proxy");
                Arc::new(
                    WallhavenClient::new(credentials)
                        .expect("production Wallhaven client must build"),
                )
            }
        };

        Self::from_runtime(paths, session, client, cache)
    }

    /// Constructs the app with injected session/client/cache components (used by boot and unit
    /// tests). The `session` must already be wired to `cache` through `restore_with_cache`.
    pub fn from_runtime(
        paths: AppPaths,
        session: SessionManager,
        client: Arc<WallhavenClient>,
        cache: Option<CacheStore>,
    ) -> (Self, Task<Message>) {
        let (proxy_mode, proxy_url_input) = match &session.local_preferences().proxy {
            ProxyConfig::System => (ProxyModeChoice::System, String::new()),
            ProxyConfig::Direct => (ProxyModeChoice::Direct, String::new()),
            ProxyConfig::Manual(url) => (ProxyModeChoice::Manual, url.clone()),
        };

        let cache_cap_input = format_cap_gb(session.local_preferences().cache_cap_bytes);
        let defaults = session.browse_defaults();
        let scope = session.scope();
        let image_guard = ImageGuard::new();
        let thumbnail_loader =
            ThumbnailLoader::from_runtime(Arc::clone(&client), cache.clone(), image_guard.clone());
        let home = GalleryState::new(
            GalleryKind::Home,
            DiscoveryController::with_view(
                scope.clone(),
                DiscoveryView::Latest,
                DiscoveryFilters::from_defaults(&defaults),
            ),
        );
        let search = GalleryState::new(
            GalleryKind::Search,
            DiscoveryController::with_view(
                scope,
                DiscoveryView::Search,
                DiscoveryFilters::from_defaults(&defaults),
            ),
        );
        let collections = CollectionsState::new(CollectionsController::from_session(&session));
        let tag = TagState::new(TagPageController::from_session(&session));
        let uploader = UploaderState::new(UploaderController::from_session(&session));
        let search_resolutions_input = format_resolutions(&defaults.resolutions);
        let search_ratios_input = format_ratios(&defaults.aspect_ratios);

        let mut app = Self {
            route: Route::Home,
            nav_stack: Vec::new(),
            paths,
            client,
            session,
            username_input: String::new(),
            api_key_input: SecretString::new(""),
            next_op_id: 1,
            sign_in_in_flight: None,
            refresh_in_flight: None,
            proxy_mode,
            proxy_url_input,
            proxy_feedback: None,
            cache,
            image_guard,
            thumbnail_loader,
            file_publisher: Arc::new(WindowsFilePublisher),
            cache_cap_input,
            cache_cap_feedback: None,
            cache_cap_in_flight: None,
            cache_stats: None,
            launch_browser: wallora_windows::open_wallhaven_in_browser,
            home,
            home_toplist_range: defaults.toplist_range,
            search,
            search_query_input: String::new(),
            search_atleast_input: String::new(),
            search_resolutions_input,
            search_ratios_input,
            search_colors_input: String::new(),
            search_filter_error: None,
            search_filters_expanded: true,
            search_input_ids: SearchInputIds::new(),
            collections,
            collections_username_input: String::new(),
            collections_username_input_id: Id::unique(),
            collections_username_error: None,
            tag,
            uploader,
            detail: None,
        };

        let startup_task = app.spawn_active_account_refresh(true);
        (app, startup_task)
    }

    /// Runtime application entry point: boots the shell and immediately begins loading `Route::Home`.
    pub fn boot_interactive(
        paths: AppPaths,
        cache_host: Option<Arc<CacheHost>>,
    ) -> (Self, Task<Message>) {
        let (mut app, startup_task) = Self::boot(paths, cache_host);
        let home_task = app.ensure_route_loaded(Route::Home);
        (app, Task::batch([startup_task, home_task]))
    }

    pub fn title(&self) -> String {
        format!(
            "Wallora — {} (Unofficial Wallhaven Client)",
            self.route.label()
        )
    }

    pub fn theme(&self) -> Theme {
        wallora_theme()
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let resize = iced::window::resize_events().map(|(_id, size)| Message::WindowResized(size));
        match self.route {
            Route::Home
            | Route::Search
            | Route::Collections
            | Route::Tag(_)
            | Route::Uploader(_) => {
                Subscription::batch([resize, iced::event::listen_with(gallery_keyboard_event)])
            }
            Route::Detail(_) => {
                Subscription::batch([resize, iced::event::listen_with(detail_keyboard_event)])
            }
            Route::Account | Route::Settings => resize,
        }
    }

    fn gallery(&self, kind: GalleryKind) -> &GalleryState {
        match kind {
            GalleryKind::Home => &self.home,
            GalleryKind::Search => &self.search,
            GalleryKind::Collections | GalleryKind::Tag | GalleryKind::Uploader => {
                unreachable!("non-discovery gallery uses its own state")
            }
        }
    }

    fn gallery_mut(&mut self, kind: GalleryKind) -> &mut GalleryState {
        match kind {
            GalleryKind::Home => &mut self.home,
            GalleryKind::Search => &mut self.search,
            GalleryKind::Collections | GalleryKind::Tag | GalleryKind::Uploader => {
                unreachable!("non-discovery gallery uses its own state")
            }
        }
    }

    fn active_gallery_kind(&self) -> Option<GalleryKind> {
        match self.route {
            Route::Home => Some(GalleryKind::Home),
            Route::Search => Some(GalleryKind::Search),
            Route::Collections => Some(GalleryKind::Collections),
            Route::Tag(_) => Some(GalleryKind::Tag),
            Route::Uploader(_) => Some(GalleryKind::Uploader),
            Route::Detail(_) | Route::Account | Route::Settings => None,
        }
    }

    fn gallery_items(&self, kind: GalleryKind) -> &[Wallpaper] {
        match kind {
            GalleryKind::Home => self.home.controller.items(),
            GalleryKind::Search => self.search.controller.items(),
            GalleryKind::Collections => self.collections.controller.items(),
            GalleryKind::Tag => self.tag.controller.items(),
            GalleryKind::Uploader => self.uploader.controller.items(),
        }
    }

    fn gallery_selected(&self, kind: GalleryKind) -> Option<usize> {
        match kind {
            GalleryKind::Home => self.home.selected,
            GalleryKind::Search => self.search.selected,
            GalleryKind::Collections => self.collections.selected,
            GalleryKind::Tag => self.tag.selected,
            GalleryKind::Uploader => self.uploader.selected,
        }
    }

    fn set_gallery_selected(&mut self, kind: GalleryKind, selected: Option<usize>) {
        match kind {
            GalleryKind::Home => self.home.selected = selected,
            GalleryKind::Search => self.search.selected = selected,
            GalleryKind::Collections => self.collections.selected = selected,
            GalleryKind::Tag => self.tag.selected = selected,
            GalleryKind::Uploader => self.uploader.selected = selected,
        }
    }

    fn gallery_metrics(&self, kind: GalleryKind) -> &GridMetrics {
        match kind {
            GalleryKind::Home => &self.home.metrics,
            GalleryKind::Search => &self.search.metrics,
            GalleryKind::Collections => &self.collections.metrics,
            GalleryKind::Tag => &self.tag.metrics,
            GalleryKind::Uploader => &self.uploader.metrics,
        }
    }

    fn gallery_metrics_mut(&mut self, kind: GalleryKind) -> &mut GridMetrics {
        match kind {
            GalleryKind::Home => &mut self.home.metrics,
            GalleryKind::Search => &mut self.search.metrics,
            GalleryKind::Collections => &mut self.collections.metrics,
            GalleryKind::Tag => &mut self.tag.metrics,
            GalleryKind::Uploader => &mut self.uploader.metrics,
        }
    }

    fn gallery_scrollable_id(&self, kind: GalleryKind) -> &Id {
        match kind {
            GalleryKind::Home => &self.home.scrollable_id,
            GalleryKind::Search => &self.search.scrollable_id,
            GalleryKind::Collections => &self.collections.scrollable_id,
            GalleryKind::Tag => &self.tag.scrollable_id,
            GalleryKind::Uploader => &self.uploader.scrollable_id,
        }
    }

    fn gallery_generation_and_thumbnails_mut(
        &mut self,
        kind: GalleryKind,
    ) -> (u64, &mut HashMap<WallpaperId, ThumbnailEntry>) {
        match kind {
            GalleryKind::Home => (self.home.controller.generation(), &mut self.home.thumbnails),
            GalleryKind::Search => (
                self.search.controller.generation(),
                &mut self.search.thumbnails,
            ),
            GalleryKind::Collections => (
                self.collections.controller.items_generation(),
                &mut self.collections.thumbnails,
            ),
            GalleryKind::Tag => (
                self.tag.controller.items_generation(),
                &mut self.tag.thumbnails,
            ),
            GalleryKind::Uploader => (
                self.uploader.controller.generation(),
                &mut self.uploader.thumbnails,
            ),
        }
    }

    fn spawn_next_gallery_page(&mut self, kind: GalleryKind) -> Option<Task<Message>> {
        match kind {
            GalleryKind::Home | GalleryKind::Search => {
                let req = self.gallery_mut(kind).controller.request_next_page()?;
                Some(self.spawn_page_request(kind, req))
            }
            GalleryKind::Collections => {
                let req = self.collections.controller.request_next_page()?;
                Some(self.spawn_collection_items_request(req))
            }
            GalleryKind::Tag => {
                let req = self.tag.controller.request_next_page()?;
                Some(self.spawn_tag_page_request(req))
            }
            GalleryKind::Uploader => {
                let req = self.uploader.controller.request_next_page()?;
                Some(self.spawn_uploader_page_request(req))
            }
        }
    }

    fn sync_search_inputs_to_defaults(&mut self, defaults: &BrowseDefaults) {
        self.search_query_input.clear();
        self.search_atleast_input.clear();
        self.search_resolutions_input = format_resolutions(&defaults.resolutions);
        self.search_ratios_input = format_ratios(&defaults.aspect_ratios);
        self.search_colors_input.clear();
        self.search_filter_error = None;
    }

    fn spawn_page_request(&mut self, kind: GalleryKind, request: PageRequest) -> Task<Message> {
        let client = Arc::clone(&self.client);
        let generation = request.generation;
        let scope = request.scope.clone();
        let page = request.page;

        let gallery = self.gallery_mut(kind);
        if let Some(prev) = gallery.page_in_flight.take() {
            prev.handle.abort();
        }

        let (task, handle) = Task::perform(
            async move { fetch_page(client.as_ref(), request).await },
            move |response| Message::DiscoveryPageLoaded {
                kind,
                response: Box::new(response),
            },
        )
        .abortable();

        gallery.page_in_flight = Some(InFlightPage {
            generation,
            scope,
            page,
            handle: handle.abort_on_drop(),
        });
        task
    }

    fn reload_gallery(&mut self, kind: GalleryKind, request: PageRequest) -> Task<Message> {
        let scroll_task = self.gallery_mut(kind).clear_for_reload();
        let fetch_task = self.spawn_page_request(kind, request);
        if scroll_task.units() == 0 {
            fetch_task
        } else {
            Task::batch([scroll_task, fetch_task])
        }
    }

    fn sync_gallery_thumbnails(&mut self, kind: GalleryKind) -> Task<Message> {
        let thumb_size = self.session.browse_defaults().thumb_size;
        let loader = self.thumbnail_loader.clone();
        match kind {
            GalleryKind::Home => self.home.sync_visible_thumbnails(&loader, &thumb_size),
            GalleryKind::Search => self.search.sync_visible_thumbnails(&loader, &thumb_size),
            GalleryKind::Collections => self
                .collections
                .sync_visible_thumbnails(&loader, &thumb_size),
            GalleryKind::Tag => self.tag.sync_visible_thumbnails(&loader, &thumb_size),
            GalleryKind::Uploader => self.uploader.sync_visible_thumbnails(&loader, &thumb_size),
        }
    }

    fn spawn_tag_metadata_request(&mut self, request: TagMetadataRequest) -> Task<Message> {
        let client = Arc::clone(&self.client);
        let generation = request.generation;
        let scope = request.scope.clone();
        let tag_id = request.tag_id;

        if let Some(prev) = self.tag.metadata_in_flight.take() {
            prev.handle.abort();
        }

        let (task, handle) = Task::perform(
            async move { fetch_tag_metadata(client.as_ref(), request).await },
            |response| Message::TagMetadataLoaded(Box::new(response)),
        )
        .abortable();

        self.tag.metadata_in_flight = Some(InFlightTagMetadata {
            generation,
            scope,
            tag_id,
            handle: handle.abort_on_drop(),
        });
        task
    }

    fn spawn_tag_page_request(&mut self, request: PageRequest) -> Task<Message> {
        let client = Arc::clone(&self.client);
        let generation = request.generation;
        let scope = request.scope.clone();
        let page = request.page;

        if let Some(prev) = self.tag.page_in_flight.take() {
            prev.handle.abort();
        }

        let (task, handle) = Task::perform(
            async move { fetch_page(client.as_ref(), request).await },
            |response| Message::TagPageLoaded(Box::new(response)),
        )
        .abortable();

        self.tag.page_in_flight = Some(InFlightPage {
            generation,
            scope,
            page,
            handle: handle.abort_on_drop(),
        });
        task
    }

    fn reload_tag_items(&mut self, request: PageRequest) -> Task<Message> {
        let scroll_task = self.tag.clear_items_for_reload();
        let fetch_task = self.spawn_tag_page_request(request);
        if scroll_task.units() == 0 {
            fetch_task
        } else {
            Task::batch([scroll_task, fetch_task])
        }
    }

    fn reload_tag_all(&mut self, reqs: TagLoadRequests) -> Task<Message> {
        let scroll_task = self.tag.clear_items_for_reload();
        let meta_task = self.spawn_tag_metadata_request(reqs.metadata);
        let page_task = self.spawn_tag_page_request(reqs.page);
        if scroll_task.units() == 0 {
            Task::batch([meta_task, page_task])
        } else {
            Task::batch([scroll_task, meta_task, page_task])
        }
    }

    fn spawn_uploader_page_request(&mut self, request: PageRequest) -> Task<Message> {
        let client = Arc::clone(&self.client);
        let generation = request.generation;
        let scope = request.scope.clone();
        let page = request.page;

        if let Some(prev) = self.uploader.page_in_flight.take() {
            prev.handle.abort();
        }

        let (task, handle) = Task::perform(
            async move { fetch_page(client.as_ref(), request).await },
            |response| Message::UploaderPageLoaded(Box::new(response)),
        )
        .abortable();

        self.uploader.page_in_flight = Some(InFlightPage {
            generation,
            scope,
            page,
            handle: handle.abort_on_drop(),
        });
        task
    }

    fn reload_uploader_items(&mut self, request: PageRequest) -> Task<Message> {
        let scroll_task = self.uploader.clear_for_reload();
        let fetch_task = self.spawn_uploader_page_request(request);
        if scroll_task.units() == 0 {
            fetch_task
        } else {
            Task::batch([scroll_task, fetch_task])
        }
    }

    fn push_nav_stack(&mut self, route: Route) {
        if self.nav_stack.last() == Some(&route) {
            return;
        }
        if self.nav_stack.len() >= MAX_NAV_STACK {
            self.nav_stack.remove(0);
        }
        self.nav_stack.push(route);
    }

    fn back_label_for_stack(&self) -> &'static str {
        match self.nav_stack.last() {
            Some(Route::Home) => "Back to Home",
            Some(Route::Search) => "Back to Search",
            Some(Route::Collections) => "Back to Collections",
            Some(Route::Tag(_)) => "Back to Tag",
            Some(Route::Uploader(_)) => "Back to Uploader",
            Some(Route::Detail(_)) => "Back to Wallpaper",
            Some(Route::Account) => "Back to Account",
            Some(Route::Settings) => "Back to Settings",
            None => "Back to Home",
        }
    }

    fn cancel_active_route_view_work(&mut self, route: &Route) {
        match route {
            Route::Home => self.home.cancel_in_flight_thumbnails(),
            Route::Search => self.search.cancel_in_flight_thumbnails(),
            Route::Collections => self.collections.cancel_in_flight_thumbnails(),
            Route::Tag(_) => self.tag.cancel_in_flight_thumbnails(),
            Route::Uploader(_) => self.uploader.cancel_in_flight_thumbnails(),
            Route::Detail(_) => self.clear_detail(),
            Route::Account | Route::Settings => {}
        }
    }

    fn spawn_collection_list_request(&mut self, request: CollectionListRequest) -> Task<Message> {
        let client = Arc::clone(&self.client);
        let generation = request.generation;
        let scope = request.scope.clone();

        if let Some(prev) = self.collections.list_in_flight.take() {
            prev.handle.abort();
        }

        let (task, handle) = Task::perform(
            async move { fetch_user_collections(client.as_ref(), request).await },
            |response| Message::CollectionsUserListLoaded(Box::new(response)),
        )
        .abortable();

        self.collections.list_in_flight = Some(InFlightCollectionList {
            generation,
            scope,
            handle: handle.abort_on_drop(),
        });
        task
    }

    fn spawn_collection_items_request(&mut self, request: CollectionItemsRequest) -> Task<Message> {
        let client = Arc::clone(&self.client);
        let generation = request.generation;
        let scope = request.scope.clone();
        let page = request.page;

        if let Some(prev) = self.collections.page_in_flight.take() {
            prev.handle.abort();
        }

        let (task, handle) = Task::perform(
            async move { fetch_collection_items(client.as_ref(), request).await },
            |response| Message::CollectionsItemsPageLoaded(Box::new(response)),
        )
        .abortable();

        self.collections.page_in_flight = Some(InFlightPage {
            generation,
            scope,
            page,
            handle: handle.abort_on_drop(),
        });
        task
    }

    fn reload_collections_items(&mut self, request: CollectionItemsRequest) -> Task<Message> {
        let scroll_task = self.collections.clear_items_for_reload();
        let fetch_task = self.spawn_collection_items_request(request);
        if scroll_task.units() == 0 {
            fetch_task
        } else {
            Task::batch([scroll_task, fetch_task])
        }
    }

    fn ensure_route_loaded(&mut self, route: Route) -> Task<Message> {
        let kind = match route {
            Route::Home => GalleryKind::Home,
            Route::Search => GalleryKind::Search,
            Route::Collections => {
                return self.sync_gallery_thumbnails(GalleryKind::Collections);
            }
            Route::Tag(id) => {
                let needs_load = self.tag.controller.tag_id() != Some(id)
                    || (*self.tag.controller.items_status() == DiscoveryStatus::Idle
                        && !self.tag.controller.is_loading_items()
                        && *self.tag.controller.metadata_status() == TagMetadataStatus::Idle
                        && !self.tag.controller.is_loading_metadata());
                if needs_load {
                    return match self.tag.controller.open_tag(id, None) {
                        Ok(reqs) => self.reload_tag_all(reqs),
                        Err(_) => Task::none(),
                    };
                }
                return self.sync_gallery_thumbnails(GalleryKind::Tag);
            }
            Route::Uploader(username) => {
                let needs_load = self.uploader.controller.username() != Some(username.as_str())
                    || (*self.uploader.controller.status() == DiscoveryStatus::Idle
                        && !self.uploader.controller.is_loading());
                if needs_load {
                    return match self.uploader.controller.open_uploader(&username) {
                        Ok(req) => self.reload_uploader_items(req),
                        Err(_) => Task::none(),
                    };
                }
                return self.sync_gallery_thumbnails(GalleryKind::Uploader);
            }
            Route::Detail(_) | Route::Account | Route::Settings => return Task::none(),
        };
        let gallery = self.gallery_mut(kind);
        if *gallery.controller.status() == DiscoveryStatus::Idle && !gallery.controller.is_loading()
        {
            let req = gallery.controller.refresh();
            self.spawn_page_request(kind, req)
        } else {
            self.sync_gallery_thumbnails(kind)
        }
    }

    fn clear_detail(&mut self) {
        if let Some(mut detail) = self.detail.take() {
            detail.cancel_all();
        }
    }

    fn clear_detail_and_return_to_gallery(&mut self) {
        let return_to = self.detail.as_ref().map(|d| d.return_to);
        self.clear_detail();
        if matches!(self.route, Route::Detail(_)) {
            self.route = match return_to.unwrap_or(GalleryKind::Home) {
                GalleryKind::Home => Route::Home,
                GalleryKind::Search => Route::Search,
                GalleryKind::Collections => Route::Collections,
                GalleryKind::Tag => self.tag.controller.tag_id().map_or(Route::Home, Route::Tag),
                GalleryKind::Uploader => self
                    .uploader
                    .controller
                    .username()
                    .map_or(Route::Home, |u| Route::Uploader(u.to_string())),
            };
        }
    }

    fn request_cache_stats(&self) -> Task<Message> {
        match &self.cache {
            Some(cache) => {
                let cache = cache.clone();
                Task::perform(
                    async move { cache.stats().await },
                    Message::CacheStatsLoaded,
                )
            }
            None => Task::none(),
        }
    }

    fn allocate_op_id(&mut self) -> u64 {
        let id = self.next_op_id;
        self.next_op_id += 1;
        id
    }

    fn cancel_sign_in(&mut self) {
        if let Some(op) = self.sign_in_in_flight.take() {
            op.handle.abort();
        }
        self.api_key_input = SecretString::new("");
    }

    fn retire_changed_scope(&mut self, previous: &SessionScope) -> Option<Task<Message>> {
        if self.session.scope() == *previous {
            return None;
        }
        if let Some(op) = self.refresh_in_flight.take() {
            op.handle.abort();
        }
        self.cancel_sign_in();
        self.nav_stack.clear();
        self.clear_detail_and_return_to_gallery();

        let scope = self.session.scope();
        let defaults = self.session.browse_defaults();
        self.home_toplist_range = defaults.toplist_range;

        let home_view = match self.home.controller.view() {
            DiscoveryView::Toplist { .. } => DiscoveryView::Toplist {
                range: self.home_toplist_range,
            },
            other => other,
        };
        self.home.reset_for_scope(
            scope.clone(),
            home_view,
            DiscoveryFilters::from_defaults(&defaults),
            false,
        );
        self.search.reset_for_scope(
            scope,
            DiscoveryView::Search,
            DiscoveryFilters::from_defaults(&defaults),
            true,
        );
        self.sync_search_inputs_to_defaults(&defaults);

        self.collections.clear_all_work();
        self.collections.controller.sync_session(&self.session);
        self.collections_username_input.clear();
        self.collections_username_error = None;

        self.tag.clear_all_work();
        self.tag.controller.sync_session(&self.session);

        self.uploader.clear_all_work();
        self.uploader.controller.sync_session(&self.session);

        Some(self.ensure_route_loaded(self.route.clone()))
    }

    fn apply_updated_browse_defaults(&mut self, defaults: &BrowseDefaults) -> Task<Message> {
        let scope = self.session.scope();
        self.home_toplist_range = defaults.toplist_range;
        self.tag.controller.sync_session(&self.session);
        self.uploader.controller.sync_session(&self.session);
        let next_filters = DiscoveryFilters::from_defaults(defaults);
        let next_home_view = match self.home.controller.view() {
            DiscoveryView::Toplist { .. } => DiscoveryView::Toplist {
                range: self.home_toplist_range,
            },
            other => other,
        };

        if *self.search.controller.status() == DiscoveryStatus::Idle
            && !self.search.controller.is_loading()
            && self.search_query_input.is_empty()
            && self.search_atleast_input.is_empty()
            && self.search_colors_input.is_empty()
        {
            self.search.reset_for_scope(
                scope.clone(),
                DiscoveryView::Search,
                next_filters.clone(),
                false,
            );
            self.sync_search_inputs_to_defaults(defaults);
        }

        let home_changed = self.home.controller.view() != next_home_view
            || self.home.controller.filters() != &next_filters;
        if !home_changed {
            return Task::none();
        }

        let was_idle = *self.home.controller.status() == DiscoveryStatus::Idle
            && !self.home.controller.is_loading();
        let scroll_task = self.home.clear_for_reload();
        self.home
            .controller
            .reset_for_scope(scope, next_home_view, next_filters, false);
        if !was_idle && self.route == Route::Home {
            let req = self.home.controller.refresh();
            let fetch_task = self.spawn_page_request(GalleryKind::Home, req);
            if scroll_task.units() == 0 {
                fetch_task
            } else {
                Task::batch([scroll_task, fetch_task])
            }
        } else {
            scroll_task
        }
    }

    fn spawn_active_account_refresh(&mut self, reverify: bool) -> Task<Message> {
        if self.refresh_in_flight.is_some() {
            return Task::none();
        }
        let Some(account) = self.session.active_account() else {
            return Task::none();
        };
        let username = account.username.clone();
        let scope = self.session.scope();
        let request_scope = scope.clone();
        let client = Arc::clone(&self.client);
        let id = self.allocate_op_id();

        let (task, handle) = Task::perform(
            async move {
                fetch_account_refresh(client.as_ref(), request_scope, username, reverify).await
            },
            move |payload| Message::AccountRefreshFinished { op_id: id, payload },
        )
        .abortable();
        self.refresh_in_flight = Some(InFlight {
            id,
            scope,
            handle: handle.abort_on_drop(),
        });
        task
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Navigate(route) => {
                if self.route != route {
                    tracing::info!(?route, "navigated view");
                    self.nav_stack.clear();
                    let previous_route = std::mem::replace(&mut self.route, route.clone());
                    self.cancel_active_route_view_work(&previous_route);
                    return match route {
                        Route::Home
                        | Route::Search
                        | Route::Collections
                        | Route::Tag(_)
                        | Route::Uploader(_) => self.ensure_route_loaded(route),
                        Route::Settings => self.request_cache_stats(),
                        Route::Detail(_) | Route::Account => Task::none(),
                    };
                }
                Task::none()
            }
            Message::UsernameChanged(value) => {
                self.username_input = value;
                Task::none()
            }
            Message::ApiKeyChanged(value) => {
                self.api_key_input = SecretString::new(value);
                Task::none()
            }
            Message::SubmitSignIn => {
                if self.sign_in_in_flight.is_some() {
                    return Task::none();
                }
                if !self.session.can_sign_in() {
                    self.session.set_notice(Some(SessionNotice::error(
                        SignInError::StorageUnavailable.to_string(),
                    )));
                    return Task::none();
                }
                let op_id = self.allocate_op_id();

                let username = self.username_input.clone();
                let api_key = self.api_key_input.clone();
                let start_scope = self.session.scope();
                let scope = start_scope.clone();
                let client = Arc::clone(&self.client);

                let (task, handle) =
                    Task::perform(
                        async move {
                            probe_sign_in(client.as_ref(), username, api_key, start_scope).await
                        },
                        move |result| Message::SignInProbeFinished { op_id, result },
                    )
                    .abortable();
                self.sign_in_in_flight = Some(InFlight {
                    id: op_id,
                    scope,
                    handle: handle.abort_on_drop(),
                });
                task
            }
            Message::SignInProbeFinished { op_id, result } => {
                let previous = self.session.scope();
                if !self
                    .sign_in_in_flight
                    .as_ref()
                    .is_some_and(|op| op.id == op_id && op.scope == previous)
                {
                    return Task::none();
                }
                self.sign_in_in_flight = None;
                // Clear typed secret input once the attempt settles.
                self.api_key_input = SecretString::new("");

                match result {
                    Ok(probe) => match self.session.commit_sign_in(probe) {
                        Ok(SignInCommitOutcome::Applied { .. }) => {
                            self.username_input.clear();
                            return self
                                .retire_changed_scope(&previous)
                                .unwrap_or_else(Task::none);
                        }
                        Ok(SignInCommitOutcome::DiscardedStale) => {}
                        Err(err) => {
                            self.session
                                .set_notice(Some(SessionNotice::error(err.to_string())));
                        }
                    },
                    Err(err) => {
                        self.session
                            .set_notice(Some(SessionNotice::error(err.to_string())));
                    }
                }
                Task::none()
            }
            Message::RefreshAccount { reverify } => self.spawn_active_account_refresh(reverify),
            Message::AccountRefreshFinished { op_id, payload } => {
                let previous = self.session.scope();
                if self.refresh_in_flight.as_ref().is_some_and(|op| {
                    op.id == op_id && op.scope == payload.request_scope && op.scope == previous
                }) {
                    self.refresh_in_flight = None;
                    let prev_defaults = self.session.browse_defaults();
                    self.session.apply_refresh(payload);
                    if let Some(task) = self.retire_changed_scope(&previous) {
                        return task;
                    }
                    if self.collections.controller.sync_session(&self.session) {
                        self.collections.clear_items_work();
                    }
                    let next_defaults = self.session.browse_defaults();
                    if next_defaults != prev_defaults {
                        return self.apply_updated_browse_defaults(&next_defaults);
                    }
                }
                Task::none()
            }
            Message::SignOut => {
                self.cancel_sign_in();
                let previous = self.session.scope();
                self.session.logout();
                self.retire_changed_scope(&previous)
                    .unwrap_or_else(Task::none)
            }
            Message::SwitchAccount(target_id) => {
                let previous = self.session.scope();
                match self.session.switch_account(&target_id) {
                    Ok(_) => {
                        let scope_task = self.retire_changed_scope(&previous);
                        if let Some(scope_task) = scope_task {
                            let refresh_task = self.spawn_active_account_refresh(true);
                            Task::batch([scope_task, refresh_task])
                        } else {
                            Task::none()
                        }
                    }
                    Err(err) => {
                        self.session
                            .set_notice(Some(SessionNotice::error(err.to_string())));
                        Task::none()
                    }
                }
            }
            Message::SwitchToGuest => {
                let previous = self.session.scope();
                match self.session.switch_to_guest() {
                    Ok(_) => {
                        self.cancel_sign_in();
                        return self
                            .retire_changed_scope(&previous)
                            .unwrap_or_else(Task::none);
                    }
                    Err(err) => {
                        tracing::error!(reason = %err, "failed to switch to guest");
                        self.session.set_notice(Some(SessionNotice::error(
                            "Failed to update active session in local storage.",
                        )));
                    }
                }
                Task::none()
            }
            Message::ProxyModeSelected(mode) => {
                self.proxy_mode = mode;
                self.proxy_feedback = None;
                Task::none()
            }
            Message::ProxyUrlChanged(url) => {
                self.proxy_url_input = url;
                self.proxy_feedback = None;
                Task::none()
            }
            Message::ApplyProxy => {
                let desired = match self.proxy_mode {
                    ProxyModeChoice::System => ProxyConfig::System,
                    ProxyModeChoice::Direct => ProxyConfig::Direct,
                    ProxyModeChoice::Manual => ProxyConfig::Manual(self.proxy_url_input.clone()),
                };
                match self.session.update_proxy(&self.client, desired) {
                    Ok(applied) => {
                        if let ProxyConfig::Manual(normalized) = &applied {
                            self.proxy_url_input = normalized.clone();
                        }
                        self.proxy_feedback = Some((
                            false,
                            format!("Applied local proxy mode: {}.", applied.mode_str()),
                        ));
                    }
                    Err(err) => {
                        self.proxy_feedback = Some((true, err.to_string()));
                    }
                }
                Task::none()
            }
            Message::CacheCapInputChanged(input) => {
                self.cache_cap_input = input;
                self.cache_cap_feedback = None;
                Task::none()
            }
            Message::ApplyCacheCap => self.apply_cache_cap(),
            Message::CacheCapApplied {
                op_id,
                cap_bytes,
                result,
            } => self.finish_cache_cap(op_id, cap_bytes, result),
            Message::RefreshCacheStats => self.request_cache_stats(),
            Message::CacheStatsLoaded(result) => {
                match result {
                    Ok(stats) => self.cache_stats = Some(stats),
                    Err(err) => tracing::warn!(reason = %err, "could not read cache statistics"),
                }
                Task::none()
            }
            Message::OpenUpstreamSettings => {
                self.open_wallhaven_url(WALLHAVEN_SETTINGS_URL);
                Task::none()
            }
            Message::HomeSelectLatest => match self.home.controller.set_view(DiscoveryView::Latest)
            {
                Some(req) => self.reload_gallery(GalleryKind::Home, req),
                None => Task::none(),
            },
            Message::HomeSelectToplist => {
                let view = DiscoveryView::Toplist {
                    range: self.home_toplist_range,
                };
                match self.home.controller.set_view(view) {
                    Some(req) => self.reload_gallery(GalleryKind::Home, req),
                    None => Task::none(),
                }
            }
            Message::HomeSelectRandom => match self.home.controller.set_view(DiscoveryView::Random)
            {
                Some(req) => self.reload_gallery(GalleryKind::Home, req),
                None => Task::none(),
            },
            Message::HomeSelectTopRange(range) => {
                self.home_toplist_range = range;
                match self
                    .home
                    .controller
                    .set_view(DiscoveryView::Toplist { range })
                {
                    Some(req) => self.reload_gallery(GalleryKind::Home, req),
                    None => Task::none(),
                }
            }
            Message::SearchQueryChanged(query) => {
                self.search_query_input = query;
                Task::none()
            }
            Message::SearchSubmit => self.submit_search(),
            Message::SearchClear => self.clear_search(),
            Message::SearchToggleFiltersPanel => {
                self.search_filters_expanded = !self.search_filters_expanded;
                Task::none()
            }
            Message::SearchToggleCategory(category) => {
                let mut filters = self.search.controller.filters().clone();
                match category {
                    Category::General => filters.categories.general = !filters.categories.general,
                    Category::Anime => filters.categories.anime = !filters.categories.anime,
                    Category::People => filters.categories.people = !filters.categories.people,
                }
                self.apply_search_filters(filters)
            }
            Message::SearchTogglePurity(purity) => {
                let mut filters = self.search.controller.filters().clone();
                filters.purity = toggle_purity_mask(filters.purity, purity);
                self.apply_search_filters(filters)
            }
            Message::SearchSelectSorting(sorting) => {
                let mut filters = self.search.controller.filters().clone();
                filters.sorting = Some(sorting);
                if sorting == Sorting::Toplist && filters.top_range.is_none() {
                    filters.top_range = Some(self.session.browse_defaults().toplist_range);
                }
                self.apply_search_filters(filters)
            }
            Message::SearchSelectOrder(order) => {
                let mut filters = self.search.controller.filters().clone();
                filters.order = Some(order);
                self.apply_search_filters(filters)
            }
            Message::SearchSelectTopRange(range) => {
                let mut filters = self.search.controller.filters().clone();
                filters.top_range = Some(range);
                if filters.sorting.is_none() {
                    filters.sorting = Some(Sorting::Toplist);
                }
                self.apply_search_filters(filters)
            }
            Message::SearchAtleastChanged(value) => {
                self.search_atleast_input = value;
                self.search_filter_error = None;
                Task::none()
            }
            Message::SearchResolutionsChanged(value) => {
                self.search_resolutions_input = value;
                self.search_filter_error = None;
                Task::none()
            }
            Message::SearchRatiosChanged(value) => {
                self.search_ratios_input = value;
                self.search_filter_error = None;
                Task::none()
            }
            Message::SearchColorsChanged(value) => {
                self.search_colors_input = value;
                self.search_filter_error = None;
                Task::none()
            }
            Message::GalleryRefresh(kind) => match kind {
                GalleryKind::Home | GalleryKind::Search => {
                    let req = self.gallery_mut(kind).controller.refresh();
                    self.reload_gallery(kind, req)
                }
                GalleryKind::Collections => {
                    if let Some(req) = self.collections.controller.refresh_items() {
                        self.reload_collections_items(req)
                    } else {
                        Task::none()
                    }
                }
                GalleryKind::Tag => {
                    if let Some(reqs) = self.tag.controller.refresh() {
                        self.reload_tag_all(reqs)
                    } else {
                        Task::none()
                    }
                }
                GalleryKind::Uploader => {
                    if let Some(req) = self.uploader.controller.refresh() {
                        self.reload_uploader_items(req)
                    } else {
                        Task::none()
                    }
                }
            },
            Message::GalleryRetry(kind) => match kind {
                GalleryKind::Home | GalleryKind::Search => {
                    let Some(req) = self.gallery_mut(kind).controller.retry() else {
                        return Task::none();
                    };
                    self.spawn_page_request(kind, req)
                }
                GalleryKind::Collections => {
                    let Some(req) = self.collections.controller.retry_items() else {
                        return Task::none();
                    };
                    self.spawn_collection_items_request(req)
                }
                GalleryKind::Tag => {
                    let Some(req) = self.tag.controller.retry_items() else {
                        return Task::none();
                    };
                    self.spawn_tag_page_request(req)
                }
                GalleryKind::Uploader => {
                    let Some(req) = self.uploader.controller.retry() else {
                        return Task::none();
                    };
                    self.spawn_uploader_page_request(req)
                }
            },
            Message::GallerySelectTile { kind, index } => {
                if let Some(wallpaper) = self.gallery_items(kind).get(index).cloned() {
                    self.set_gallery_selected(kind, Some(index));
                    let current = self.route.clone();
                    self.push_nav_stack(current);
                    return self.open_detail(wallpaper.id.clone(), Some(wallpaper), kind);
                }
                Task::none()
            }
            Message::GalleryScrolled {
                kind,
                offset_y,
                viewport_width,
                viewport_height,
                content_height,
            } => {
                let near_bottom = {
                    let metrics = self.gallery_metrics_mut(kind);
                    metrics.update_scroll(offset_y, viewport_width, viewport_height);
                    let len = self.gallery_items(kind).len();
                    self.gallery_metrics(kind)
                        .is_near_bottom(len, Some(content_height))
                };
                let next_task = if near_bottom {
                    self.spawn_next_gallery_page(kind)
                } else {
                    None
                };
                let thumb_task = self.sync_gallery_thumbnails(kind);
                match next_task {
                    Some(page_task) => Task::batch([thumb_task, page_task]),
                    None => thumb_task,
                }
            }
            Message::DiscoveryPageLoaded { kind, response } => {
                let response = *response;
                let resp_generation = response.generation;
                let resp_scope = response.scope.clone();
                let resp_page = response.page;
                let previous = self.session.scope();
                let outcome = match kind {
                    GalleryKind::Home => self
                        .home
                        .controller
                        .apply_response(&mut self.session, response),
                    GalleryKind::Search => self
                        .search
                        .controller
                        .apply_response(&mut self.session, response),
                    GalleryKind::Collections | GalleryKind::Tag | GalleryKind::Uploader => {
                        return Task::none();
                    }
                };
                match outcome {
                    ApplyOutcome::Stale => Task::none(),
                    ApplyOutcome::Revoked(_) => self
                        .retire_changed_scope(&previous)
                        .unwrap_or_else(Task::none),
                    ApplyOutcome::Applied { .. } => {
                        if self
                            .gallery(kind)
                            .page_in_flight
                            .as_ref()
                            .is_some_and(|op| {
                                op.generation == resp_generation
                                    && op.scope == resp_scope
                                    && op.page == resp_page
                            })
                        {
                            self.gallery_mut(kind).page_in_flight = None;
                        }
                        if self.active_gallery_kind() == Some(kind) {
                            self.sync_gallery_thumbnails(kind)
                        } else {
                            Task::none()
                        }
                    }
                    ApplyOutcome::Failed(_) => {
                        if self
                            .gallery(kind)
                            .page_in_flight
                            .as_ref()
                            .is_some_and(|op| {
                                op.generation == resp_generation
                                    && op.scope == resp_scope
                                    && op.page == resp_page
                            })
                        {
                            self.gallery_mut(kind).page_in_flight = None;
                        }
                        Task::none()
                    }
                }
            }
            Message::ThumbnailLoaded {
                kind,
                generation,
                scope,
                id,
                result,
            } => {
                let current_scope = self.session.scope();
                let (target_generation, thumbnails) =
                    self.gallery_generation_and_thumbnails_mut(kind);
                apply_thumbnail_loaded(
                    &current_scope,
                    target_generation,
                    generation,
                    scope,
                    id,
                    result,
                    thumbnails,
                );
                Task::none()
            }
            Message::RetryThumbnail { kind, id } => {
                let (_, thumbnails) = self.gallery_generation_and_thumbnails_mut(kind);
                let should_retry =
                    matches!(thumbnails.get(&id), Some(ThumbnailEntry::Failed { .. }));
                if should_retry {
                    thumbnails.remove(&id);
                    return self.sync_gallery_thumbnails(kind);
                }
                Task::none()
            }
            Message::OpenUnsupportedFeature(feature) => {
                self.open_wallhaven_url(feature.hand_off().url);
                Task::none()
            }
            Message::WindowResized(size) => self.handle_window_resized(size),
            Message::RuntimeGalleryKey(key) => match self.route {
                Route::Home => self.apply_gallery_key(GalleryKind::Home, key),
                Route::Search => {
                    let focus_checks: Vec<Task<bool>> = self
                        .search_input_ids
                        .all()
                        .into_iter()
                        .map(operation::is_focused)
                        .collect();
                    Task::batch(focus_checks).collect().map(move |flags| {
                        Message::SearchKeyFocusChecked {
                            key,
                            any_input_focused: flags.into_iter().any(|focused| focused),
                        }
                    })
                }
                Route::Collections => operation::is_focused(
                    self.collections_username_input_id.clone(),
                )
                .map(move |input_focused| Message::CollectionsKeyFocusChecked {
                    key,
                    input_focused,
                }),
                Route::Tag(_) => self.apply_gallery_key(GalleryKind::Tag, key),
                Route::Uploader(_) => self.apply_gallery_key(GalleryKind::Uploader, key),
                Route::Detail(_) | Route::Account | Route::Settings => Task::none(),
            },
            Message::SearchKeyFocusChecked {
                key,
                any_input_focused,
            } => {
                if !any_input_focused && self.route == Route::Search {
                    self.apply_gallery_key(GalleryKind::Search, key)
                } else {
                    Task::none()
                }
            }
            Message::CollectionsKeyFocusChecked { key, input_focused } => {
                if !input_focused && self.route == Route::Collections {
                    self.apply_gallery_key(GalleryKind::Collections, key)
                } else {
                    Task::none()
                }
            }
            Message::DetailBack => self.detail_back(),
            Message::DetailRetryMetadata => self.retry_detail_metadata(),
            Message::DetailMetadataLoaded {
                op_id,
                scope,
                id,
                result,
            } => {
                let matches_in_flight = self.detail.as_ref().is_some_and(|d| {
                    d.wallpaper_id == id
                        && matches!(
                            &d.metadata,
                            DetailMetadataState::Loading { op_id: active_op, .. }
                                if *active_op == op_id
                        )
                });
                if !matches_in_flight {
                    return Task::none();
                }
                let previous = self.session.scope();
                match self.session.handle_provider_result(&scope, *result) {
                    ScopedResponse::Stale => Task::none(),
                    ScopedResponse::Revoked(_) => self
                        .retire_changed_scope(&previous)
                        .unwrap_or_else(Task::none),
                    ScopedResponse::Ready(Ok(wallpaper)) => {
                        if let Some(detail) = &mut self.detail {
                            detail.metadata = DetailMetadataState::Loaded(Box::new(wallpaper));
                        }
                        self.spawn_detail_preview(false)
                    }
                    ScopedResponse::Ready(Err(error)) => {
                        if let Some(detail) = &mut self.detail {
                            let summary = detail.wallpaper().cloned();
                            detail.metadata = DetailMetadataState::Failed { summary, error };
                        }
                        Task::none()
                    }
                }
            }
            Message::DetailPreviewLoaded {
                op_id,
                scope,
                id,
                fallback_used,
                result,
            } => {
                if scope != self.session.scope() {
                    return Task::none();
                }
                let matches_in_flight = self.detail.as_ref().is_some_and(|d| {
                    d.wallpaper_id == id
                        && matches!(
                            &d.preview,
                            DetailPreviewState::Loading {
                                op_id: active_op,
                                scope: active_scope,
                                ..
                            } if *active_op == op_id && *active_scope == scope
                        )
                });
                if !matches_in_flight {
                    return Task::none();
                }
                match result {
                    Ok(loaded) => {
                        if let Some(detail) = &mut self.detail {
                            detail.preview = DetailPreviewState::Loaded {
                                handle: loaded.handle,
                                stale: loaded.stale,
                                fallback_used,
                            };
                        }
                        Task::none()
                    }
                    Err(ImagePipelineError::Cancelled | ImagePipelineError::ScopeChanged) => {
                        if let Some(detail) = &mut self.detail {
                            detail.preview = DetailPreviewState::Idle;
                        }
                        Task::none()
                    }
                    Err(err) if !fallback_used && is_preview_budget_error(&err) => {
                        self.spawn_detail_preview(true)
                    }
                    Err(err) => {
                        if let Some(detail) = &mut self.detail {
                            detail.preview = DetailPreviewState::Failed {
                                message: thumbnail_error_message(&err),
                            };
                        }
                        Task::none()
                    }
                }
            }
            Message::DetailRetryPreview => self.spawn_detail_preview(false),
            Message::DetailSelectTag { id, name } => self.open_tag_route(id, Some(name)),
            Message::DetailSelectUploader(username) => self.open_uploader_route(&username),
            Message::DetailMoreLikeThis(id) => {
                let query = QueryToken::like(id.as_str())
                    .map(|token| token.render())
                    .unwrap_or_else(|_| format!("like:{}", id.as_str()));
                self.open_search_with_query(query)
            }
            Message::DetailCopyLink { target, url } => {
                if let Some(detail) = &mut self.detail {
                    detail.copy_feedback = Some(format!("Copied {} to clipboard.", target.label()));
                }
                iced::clipboard::write(url)
            }
            Message::DetailOpenHandOff(action) => {
                if let Some(detail) = &self.detail {
                    match action.wallpaper_url(&detail.wallpaper_id) {
                        Ok(url) => self.open_wallhaven_url(&url),
                        Err(err) => {
                            self.session.set_notice(Some(SessionNotice::warning(err)));
                        }
                    }
                }
                Task::none()
            }
            Message::DetailOpenTagModerationHandOff => {
                if let Some(detail) = &self.detail {
                    match ProfileTagUnsupportedAction::tag_moderation_url(&detail.wallpaper_id) {
                        Ok(url) => self.open_wallhaven_url(&url),
                        Err(err) => {
                            self.session.set_notice(Some(SessionNotice::warning(err)));
                        }
                    }
                }
                Task::none()
            }
            Message::DetailDownloadOriginal => self.start_detail_save_dialog(),
            Message::DetailSavePathPicked {
                op_id,
                scope,
                id,
                path,
            } => self.start_detail_download(op_id, scope, id, path),
            Message::DetailDownloadProgress {
                op_id,
                scope,
                id,
                bytes_written,
                total_bytes,
            } => {
                if scope == self.session.scope()
                    && let Some(detail) = &mut self.detail
                    && detail.wallpaper_id == id
                    && let DetailDownloadState::Downloading {
                        op_id: active_op,
                        scope: active_scope,
                        bytes_written: current_written,
                        total_bytes: current_total,
                        ..
                    } = &mut detail.download
                    && *active_op == op_id
                    && *active_scope == scope
                {
                    *current_written = bytes_written;
                    *current_total = total_bytes;
                }
                Task::none()
            }
            Message::DetailDownloadFinished {
                op_id,
                scope,
                id,
                result,
            } => {
                if scope != self.session.scope() {
                    return Task::none();
                }
                let matches_downloading = self.detail.as_ref().is_some_and(|d| {
                    d.wallpaper_id == id
                        && matches!(
                            &d.download,
                            DetailDownloadState::Downloading {
                                op_id: active_op,
                                scope: active_scope,
                                ..
                            } if *active_op == op_id && *active_scope == scope
                        )
                });
                if !matches_downloading {
                    return Task::none();
                }
                if let Some(detail) = &mut self.detail {
                    detail.download = match result {
                        Ok(outcome) => DetailDownloadState::Saved {
                            path: outcome.path,
                            bytes_written: outcome.bytes_written,
                            collision_renamed: outcome.collision_renamed,
                        },
                        Err(DownloadError::Cancelled { .. }) => DetailDownloadState::Idle,
                        Err(err) => DetailDownloadState::Failed {
                            message: err.user_message(),
                            part_path: err.part_path().map(PathBuf::from),
                        },
                    };
                }
                Task::none()
            }
            Message::DetailCancelDownload => {
                self.cancel_detail_download();
                Task::none()
            }
            Message::DetailOpenFavoriteHandOff => {
                if let Some(detail) = &self.detail {
                    match CollectionUnsupportedAction::favorite_url(&detail.wallpaper_id) {
                        Ok(url) => self.open_wallhaven_url(&url),
                        Err(err) => {
                            self.session.set_notice(Some(SessionNotice::warning(err)));
                        }
                    }
                }
                Task::none()
            }
            Message::CollectionsSelectOwn(collection_id) => {
                match self
                    .collections
                    .controller
                    .select_own_collection(collection_id)
                {
                    Ok(req) => self.reload_collections_items(req),
                    Err(_) => {
                        self.collections.clear_items_work();
                        Task::none()
                    }
                }
            }
            Message::CollectionsUsernameChanged(username) => {
                self.collections_username_input = username;
                self.collections_username_error = None;
                Task::none()
            }
            Message::CollectionsLoadUser => {
                let input = self.collections_username_input.clone();
                match self.collections.controller.load_user_collections(&input) {
                    Ok(req) => {
                        self.collections_username_error = None;
                        if self.collections.controller.selected_collection().is_none() {
                            self.collections.clear_items_work();
                        }
                        self.spawn_collection_list_request(req)
                    }
                    Err(err) => {
                        self.collections_username_error = Some(err.to_string());
                        Task::none()
                    }
                }
            }
            Message::CollectionsRetryUserList => {
                match self.collections.controller.retry_user_collections() {
                    Some(req) => self.spawn_collection_list_request(req),
                    None => Task::none(),
                }
            }
            Message::CollectionsUserListLoaded(response) => {
                let response = *response;
                let resp_gen = response.generation;
                let resp_scope = response.scope.clone();
                let previous = self.session.scope();
                match self
                    .collections
                    .controller
                    .apply_list_response(&mut self.session, response)
                {
                    CollectionListOutcome::Stale => Task::none(),
                    CollectionListOutcome::Revoked(_) => self
                        .retire_changed_scope(&previous)
                        .unwrap_or_else(Task::none),
                    CollectionListOutcome::Applied { .. } | CollectionListOutcome::Failed(_) => {
                        if self
                            .collections
                            .list_in_flight
                            .as_ref()
                            .is_some_and(|op| op.generation == resp_gen && op.scope == resp_scope)
                        {
                            self.collections.list_in_flight = None;
                        }
                        Task::none()
                    }
                }
            }
            Message::CollectionsSelectUserCollection(collection_id) => {
                match self
                    .collections
                    .controller
                    .select_user_collection(collection_id)
                {
                    Ok(req) => {
                        self.collections_username_error = None;
                        self.reload_collections_items(req)
                    }
                    Err(err) => {
                        self.collections.clear_items_work();
                        self.collections_username_error = Some(err.to_string());
                        Task::none()
                    }
                }
            }
            Message::CollectionsTogglePurity(purity) => {
                let next = toggle_purity_mask(self.collections.controller.purity(), purity);
                match self.collections.controller.set_purity(next) {
                    Some(req) => self.reload_collections_items(req),
                    None => Task::none(),
                }
            }
            Message::CollectionsItemsPageLoaded(response) => {
                let response = *response;
                let resp_generation = response.generation;
                let resp_scope = response.scope.clone();
                let resp_page = response.page;
                let previous = self.session.scope();
                let outcome = self
                    .collections
                    .controller
                    .apply_items_response(&mut self.session, response);
                match outcome {
                    ApplyOutcome::Stale => Task::none(),
                    ApplyOutcome::Revoked(_) => self
                        .retire_changed_scope(&previous)
                        .unwrap_or_else(Task::none),
                    ApplyOutcome::Applied { .. } => {
                        if self.collections.page_in_flight.as_ref().is_some_and(|op| {
                            op.generation == resp_generation
                                && op.scope == resp_scope
                                && op.page == resp_page
                        }) {
                            self.collections.page_in_flight = None;
                        }
                        if self.active_gallery_kind() == Some(GalleryKind::Collections) {
                            self.sync_gallery_thumbnails(GalleryKind::Collections)
                        } else {
                            Task::none()
                        }
                    }
                    ApplyOutcome::Failed(_) => {
                        if self.collections.page_in_flight.as_ref().is_some_and(|op| {
                            op.generation == resp_generation
                                && op.scope == resp_scope
                                && op.page == resp_page
                        }) {
                            self.collections.page_in_flight = None;
                        }
                        Task::none()
                    }
                }
            }
            Message::CollectionsOpenManageHandOff => {
                let username = self
                    .collections
                    .controller
                    .own_account()
                    .map(|a| a.username.as_str());
                let url = CollectionUnsupportedAction::manage_collections_url(username);
                self.open_wallhaven_url(&url);
                Task::none()
            }
            Message::TagBack => {
                let target = self.nav_stack.pop().unwrap_or(Route::Home);
                self.restore_route(target)
            }
            Message::TagRetryMetadata => match self.tag.controller.retry_metadata() {
                Some(req) => self.spawn_tag_metadata_request(req),
                None => Task::none(),
            },
            Message::TagMetadataLoaded(response) => {
                let response = *response;
                let resp_gen = response.generation;
                let resp_scope = response.scope.clone();
                let resp_tag_id = response.tag_id;
                let previous = self.session.scope();
                match self
                    .tag
                    .controller
                    .apply_metadata_response(&mut self.session, response)
                {
                    TagMetadataOutcome::Stale => Task::none(),
                    TagMetadataOutcome::Revoked(_) => self
                        .retire_changed_scope(&previous)
                        .unwrap_or_else(Task::none),
                    TagMetadataOutcome::Applied | TagMetadataOutcome::Failed(_) => {
                        if self.tag.metadata_in_flight.as_ref().is_some_and(|op| {
                            op.generation == resp_gen
                                && op.scope == resp_scope
                                && op.tag_id == resp_tag_id
                        }) {
                            self.tag.metadata_in_flight = None;
                        }
                        Task::none()
                    }
                }
            }
            Message::TagPageLoaded(response) => {
                let response = *response;
                let resp_gen = response.generation;
                let resp_scope = response.scope.clone();
                let resp_page = response.page;
                let previous = self.session.scope();
                let outcome = self
                    .tag
                    .controller
                    .apply_page_response(&mut self.session, response);
                match outcome {
                    ApplyOutcome::Stale => Task::none(),
                    ApplyOutcome::Revoked(_) => self
                        .retire_changed_scope(&previous)
                        .unwrap_or_else(Task::none),
                    ApplyOutcome::Applied { .. } => {
                        if self.tag.page_in_flight.as_ref().is_some_and(|op| {
                            op.generation == resp_gen
                                && op.scope == resp_scope
                                && op.page == resp_page
                        }) {
                            self.tag.page_in_flight = None;
                        }
                        if self.active_gallery_kind() == Some(GalleryKind::Tag) {
                            self.sync_gallery_thumbnails(GalleryKind::Tag)
                        } else {
                            Task::none()
                        }
                    }
                    ApplyOutcome::Failed(_) => {
                        if self.tag.page_in_flight.as_ref().is_some_and(|op| {
                            op.generation == resp_gen
                                && op.scope == resp_scope
                                && op.page == resp_page
                        }) {
                            self.tag.page_in_flight = None;
                        }
                        Task::none()
                    }
                }
            }
            Message::TagTogglePurity(purity) => {
                let next = toggle_purity_mask(self.tag.controller.purity(), purity);
                match self.tag.controller.set_purity(next) {
                    Some(req) => self.reload_tag_items(req),
                    None => Task::none(),
                }
            }
            Message::TagSelectSorting(sorting) => match self.tag.controller.set_sorting(sorting) {
                Some(req) => self.reload_tag_items(req),
                None => Task::none(),
            },
            Message::TagSelectOrder(order) => match self.tag.controller.set_order(Some(order)) {
                Some(req) => self.reload_tag_items(req),
                None => Task::none(),
            },
            Message::TagSelectTopRange(range) => match self.tag.controller.set_top_range(range) {
                Some(req) => self.reload_tag_items(req),
                None => Task::none(),
            },
            Message::TagSearchThisTag => {
                let Some(query) = self.tag.controller.search_query_string() else {
                    return Task::none();
                };
                self.tag.cancel_in_flight_thumbnails();
                self.open_search_with_query(query)
            }
            Message::TagOpenDirectoryHandOff => {
                self.open_wallhaven_url(ProfileTagUnsupportedAction::tag_directory_url());
                Task::none()
            }
            Message::UploaderBack => {
                let target = self.nav_stack.pop().unwrap_or(Route::Home);
                self.restore_route(target)
            }
            Message::UploaderPageLoaded(response) => {
                let response = *response;
                let resp_gen = response.generation;
                let resp_scope = response.scope.clone();
                let resp_page = response.page;
                let previous = self.session.scope();
                let outcome = self
                    .uploader
                    .controller
                    .apply_response(&mut self.session, response);
                match outcome {
                    ApplyOutcome::Stale => Task::none(),
                    ApplyOutcome::Revoked(_) => self
                        .retire_changed_scope(&previous)
                        .unwrap_or_else(Task::none),
                    ApplyOutcome::Applied { .. } => {
                        if self.uploader.page_in_flight.as_ref().is_some_and(|op| {
                            op.generation == resp_gen
                                && op.scope == resp_scope
                                && op.page == resp_page
                        }) {
                            self.uploader.page_in_flight = None;
                        }
                        if self.active_gallery_kind() == Some(GalleryKind::Uploader) {
                            self.sync_gallery_thumbnails(GalleryKind::Uploader)
                        } else {
                            Task::none()
                        }
                    }
                    ApplyOutcome::Failed(_) => {
                        if self.uploader.page_in_flight.as_ref().is_some_and(|op| {
                            op.generation == resp_gen
                                && op.scope == resp_scope
                                && op.page == resp_page
                        }) {
                            self.uploader.page_in_flight = None;
                        }
                        Task::none()
                    }
                }
            }
            Message::UploaderTogglePurity(purity) => {
                let next = toggle_purity_mask(self.uploader.controller.purity(), purity);
                match self.uploader.controller.set_purity(next) {
                    Some(req) => self.reload_uploader_items(req),
                    None => Task::none(),
                }
            }
            Message::UploaderSelectSorting(sorting) => {
                match self.uploader.controller.set_sorting(sorting) {
                    Some(req) => self.reload_uploader_items(req),
                    None => Task::none(),
                }
            }
            Message::UploaderSelectOrder(order) => {
                match self.uploader.controller.set_order(Some(order)) {
                    Some(req) => self.reload_uploader_items(req),
                    None => Task::none(),
                }
            }
            Message::UploaderSelectTopRange(range) => {
                match self.uploader.controller.set_top_range(range) {
                    Some(req) => self.reload_uploader_items(req),
                    None => Task::none(),
                }
            }
            Message::UploaderViewPublicCollections => {
                let Some(username) = self.uploader.controller.username().map(str::to_string) else {
                    return Task::none();
                };
                self.uploader.cancel_in_flight_thumbnails();
                self.nav_stack.clear();
                self.route = Route::Collections;
                self.collections_username_input = username.clone();
                self.collections_username_error = None;
                match self.collections.controller.load_user_collections(&username) {
                    Ok(req) => {
                        if self.collections.controller.selected_collection().is_none() {
                            self.collections.clear_items_work();
                        }
                        self.spawn_collection_list_request(req)
                    }
                    Err(err) => {
                        self.collections_username_error = Some(err.to_string());
                        Task::none()
                    }
                }
            }
            Message::UploaderSearchUploads => {
                let Some(query) = self.uploader.controller.search_query_string() else {
                    return Task::none();
                };
                self.uploader.cancel_in_flight_thumbnails();
                self.open_search_with_query(query)
            }
            Message::UploaderOpenProfileHandOff => {
                if let Some(username) = self.uploader.controller.username() {
                    match ProfileTagUnsupportedAction::user_profile_url(username) {
                        Ok(url) => self.open_wallhaven_url(&url),
                        Err(err) => {
                            self.session.set_notice(Some(SessionNotice::warning(err)));
                        }
                    }
                }
                Task::none()
            }
        }
    }

    fn spawn_wallpaper_fetch(
        &self,
        id: WallpaperId,
        op_id: u64,
        scope: SessionScope,
    ) -> (Task<Message>, Handle) {
        let client = Arc::clone(&self.client);
        let msg_scope = scope.clone();
        let msg_id = id.clone();
        let (task, handle) = Task::perform(
            async move { client.wallpaper(id, scope).await },
            move |result| Message::DetailMetadataLoaded {
                op_id,
                scope: msg_scope,
                id: msg_id,
                result: Box::new(result),
            },
        )
        .abortable();
        (task, handle)
    }

    fn open_detail(
        &mut self,
        id: WallpaperId,
        summary: Option<Wallpaper>,
        return_to: GalleryKind,
    ) -> Task<Message> {
        self.clear_detail();
        match return_to {
            GalleryKind::Home => self.home.cancel_in_flight_thumbnails(),
            GalleryKind::Search => self.search.cancel_in_flight_thumbnails(),
            GalleryKind::Collections => self.collections.cancel_in_flight_thumbnails(),
            GalleryKind::Tag => self.tag.cancel_in_flight_thumbnails(),
            GalleryKind::Uploader => self.uploader.cancel_in_flight_thumbnails(),
        }
        self.route = Route::Detail(id.clone());
        let op_id = self.allocate_op_id();
        let scope = self.session.scope();
        let (task, handle) = self.spawn_wallpaper_fetch(id.clone(), op_id, scope);
        self.detail = Some(DetailState {
            wallpaper_id: id,
            return_to,
            metadata: DetailMetadataState::Loading {
                summary,
                op_id,
                handle: handle.abort_on_drop(),
            },
            preview: DetailPreviewState::Idle,
            download: DetailDownloadState::Idle,
            copy_feedback: None,
        });
        task
    }

    fn open_tag_route(&mut self, tag_id: u64, hint_name: Option<String>) -> Task<Message> {
        let reqs = match self.tag.controller.open_tag(tag_id, hint_name) {
            Ok(reqs) => reqs,
            Err(err) => {
                self.session.set_notice(Some(SessionNotice::warning(err)));
                return Task::none();
            }
        };
        let previous = self.route.clone();
        self.push_nav_stack(previous.clone());
        self.cancel_active_route_view_work(&previous);
        self.route = Route::Tag(tag_id);
        self.reload_tag_all(reqs)
    }

    fn open_uploader_route(&mut self, raw_username: &str) -> Task<Message> {
        let clean = match validate_username(raw_username) {
            Ok(clean) => clean,
            Err(err) => {
                self.session.set_notice(Some(SessionNotice::warning(err)));
                return Task::none();
            }
        };
        let req = match self.uploader.controller.open_uploader(&clean) {
            Ok(req) => req,
            Err(err) => {
                self.session.set_notice(Some(SessionNotice::warning(err)));
                return Task::none();
            }
        };
        let previous = self.route.clone();
        self.push_nav_stack(previous.clone());
        self.cancel_active_route_view_work(&previous);
        self.route = Route::Uploader(clean);
        self.reload_uploader_items(req)
    }

    fn restore_gallery_view(&mut self, kind: GalleryKind) -> Task<Message> {
        let scroll_y = self.gallery_metrics(kind).scroll_y;
        let scroll_task = if scroll_y > 0.0 {
            operation::scroll_to(
                self.gallery_scrollable_id(kind).clone(),
                AbsoluteOffset {
                    x: Some(0.0),
                    y: Some(scroll_y),
                },
            )
        } else {
            Task::none()
        };
        let thumb_task = self.sync_gallery_thumbnails(kind);
        if scroll_task.units() == 0 {
            thumb_task
        } else if thumb_task.units() == 0 {
            scroll_task
        } else {
            Task::batch([scroll_task, thumb_task])
        }
    }

    fn restore_route(&mut self, target: Route) -> Task<Message> {
        let previous = self.route.clone();
        self.cancel_active_route_view_work(&previous);
        match target {
            Route::Home => {
                self.route = Route::Home;
                self.restore_gallery_view(GalleryKind::Home)
            }
            Route::Search => {
                self.route = Route::Search;
                self.restore_gallery_view(GalleryKind::Search)
            }
            Route::Collections => {
                self.route = Route::Collections;
                self.restore_gallery_view(GalleryKind::Collections)
            }
            Route::Tag(id) => {
                self.route = Route::Tag(id);
                let already_loaded = self.tag.controller.tag_id() == Some(id)
                    && (*self.tag.controller.items_status() != DiscoveryStatus::Idle
                        || self.tag.controller.is_loading_items()
                        || *self.tag.controller.metadata_status() != TagMetadataStatus::Idle
                        || self.tag.controller.is_loading_metadata());
                if already_loaded {
                    self.restore_gallery_view(GalleryKind::Tag)
                } else {
                    self.ensure_route_loaded(Route::Tag(id))
                }
            }
            Route::Uploader(username) => {
                self.route = Route::Uploader(username.clone());
                let already_loaded = self.uploader.controller.username() == Some(username.as_str())
                    && (*self.uploader.controller.status() != DiscoveryStatus::Idle
                        || self.uploader.controller.is_loading());
                if already_loaded {
                    self.restore_gallery_view(GalleryKind::Uploader)
                } else {
                    self.ensure_route_loaded(Route::Uploader(username))
                }
            }
            Route::Detail(id) => {
                let return_to = match self.nav_stack.last() {
                    Some(Route::Search) => GalleryKind::Search,
                    Some(Route::Collections) => GalleryKind::Collections,
                    Some(Route::Tag(_)) => GalleryKind::Tag,
                    Some(Route::Uploader(_)) => GalleryKind::Uploader,
                    _ => GalleryKind::Home,
                };
                let summary = self
                    .gallery_items(return_to)
                    .iter()
                    .find(|w| w.id == id)
                    .cloned();
                self.open_detail(id, summary, return_to)
            }
            Route::Account => {
                self.route = Route::Account;
                Task::none()
            }
            Route::Settings => {
                self.route = Route::Settings;
                self.request_cache_stats()
            }
        }
    }

    fn detail_back(&mut self) -> Task<Message> {
        let fallback = match self
            .detail
            .as_ref()
            .map_or(GalleryKind::Home, |d| d.return_to)
        {
            GalleryKind::Home => Route::Home,
            GalleryKind::Search => Route::Search,
            GalleryKind::Collections => Route::Collections,
            GalleryKind::Tag => self.tag.controller.tag_id().map_or(Route::Home, Route::Tag),
            GalleryKind::Uploader => self
                .uploader
                .controller
                .username()
                .map_or(Route::Home, |u| Route::Uploader(u.to_string())),
        };
        let target = self.nav_stack.pop().unwrap_or(fallback);
        self.restore_route(target)
    }

    fn retry_detail_metadata(&mut self) -> Task<Message> {
        let Some(detail) = &self.detail else {
            return Task::none();
        };
        let summary = detail.wallpaper().cloned();
        let id = detail.wallpaper_id.clone();
        let op_id = self.allocate_op_id();
        let scope = self.session.scope();
        let (task, handle) = self.spawn_wallpaper_fetch(id, op_id, scope);
        if let Some(detail) = &mut self.detail {
            if let DetailMetadataState::Loading { handle: old, .. } = &detail.metadata {
                old.abort();
            }
            detail.metadata = DetailMetadataState::Loading {
                summary,
                op_id,
                handle: handle.abort_on_drop(),
            };
        }
        task
    }

    fn spawn_detail_preview(&mut self, force_fallback: bool) -> Task<Message> {
        let Some(detail) = &self.detail else {
            return Task::none();
        };
        let Some(wallpaper) = detail.wallpaper().cloned() else {
            return Task::none();
        };
        let fallback_used = force_fallback || preview_exceeds_budget(&wallpaper);
        let raw_url = if fallback_used {
            select_fallback_preview_url(&wallpaper.thumbs)
        } else if !wallpaper.path.is_empty() {
            wallpaper.path.as_str()
        } else {
            select_fallback_preview_url(&wallpaper.thumbs)
        };
        let scope = self.session.scope();
        let request = match ImageRequest::preview_url(raw_url, scope.clone()) {
            Ok(req) => req,
            Err(err) => {
                if let Some(detail) = &mut self.detail {
                    detail.cancel_preview();
                    detail.preview = DetailPreviewState::Failed {
                        message: thumbnail_error_message(&err),
                    };
                }
                return Task::none();
            }
        };

        let op_id = self.allocate_op_id();
        let cancel = CancelToken::new();
        let cancel_for_task = cancel.clone();
        let loader = self.thumbnail_loader.clone();
        let id = wallpaper.id.clone();
        let msg_scope = scope.clone();

        let (task, handle) = Task::perform(
            async move { loader.load(request, cancel_for_task).await },
            move |result| Message::DetailPreviewLoaded {
                op_id,
                scope: msg_scope,
                id,
                fallback_used,
                result,
            },
        )
        .abortable();

        if let Some(detail) = &mut self.detail {
            detail.cancel_preview();
            detail.preview = DetailPreviewState::Loading {
                op_id,
                scope,
                fallback_used,
                cancel,
                handle: handle.abort_on_drop(),
            };
        }
        task
    }

    fn open_search_with_query(&mut self, query: String) -> Task<Message> {
        self.nav_stack.clear();
        self.clear_detail();
        self.route = Route::Search;
        let defaults = self.session.browse_defaults();
        let default_filters = DiscoveryFilters::from_defaults(&defaults);
        self.search_query_input = query.clone();
        self.search_atleast_input.clear();
        self.search_resolutions_input = format_resolutions(&defaults.resolutions);
        self.search_ratios_input = format_ratios(&defaults.aspect_ratios);
        self.search_colors_input.clear();
        self.search_filter_error = None;

        let terms_req = self.search.controller.set_terms(SearchTerms::raw(query));
        let filters_req = self.search.controller.set_filters(default_filters);
        let req = filters_req
            .or(terms_req)
            .unwrap_or_else(|| self.search.controller.refresh());
        self.reload_gallery(GalleryKind::Search, req)
    }

    fn start_detail_save_dialog(&mut self) -> Task<Message> {
        let Some(detail) = &self.detail else {
            return Task::none();
        };
        if matches!(
            detail.download,
            DetailDownloadState::PickingLocation { .. } | DetailDownloadState::Downloading { .. }
        ) {
            return Task::none();
        }
        let Some(wallpaper) = detail.full_wallpaper() else {
            return Task::none();
        };
        let default_name = default_download_filename(wallpaper);
        let id = detail.wallpaper_id.clone();
        let op_id = self.allocate_op_id();
        let scope = self.session.scope();
        let msg_scope = scope.clone();

        let (task, handle) = Task::perform(pick_download_destination(default_name), move |path| {
            Message::DetailSavePathPicked {
                op_id,
                scope: msg_scope,
                id,
                path,
            }
        })
        .abortable();

        if let Some(detail) = &mut self.detail {
            detail.download = DetailDownloadState::PickingLocation {
                op_id,
                scope,
                handle: handle.abort_on_drop(),
            };
        }
        task
    }

    fn start_detail_download(
        &mut self,
        op_id: u64,
        scope: SessionScope,
        id: WallpaperId,
        path: Option<PathBuf>,
    ) -> Task<Message> {
        if scope != self.session.scope() {
            return Task::none();
        }
        let matches_picking = self.detail.as_ref().is_some_and(|d| {
            d.wallpaper_id == id
                && matches!(
                    &d.download,
                    DetailDownloadState::PickingLocation {
                        op_id: active_op,
                        scope: active_scope,
                        ..
                    } if *active_op == op_id && *active_scope == scope
                )
        });
        if !matches_picking {
            return Task::none();
        }

        let Some(destination) = path else {
            if let Some(detail) = &mut self.detail {
                detail.download = DetailDownloadState::Idle;
            }
            return Task::none();
        };

        let request = {
            let Some(detail) = &self.detail else {
                return Task::none();
            };
            let Some(wallpaper) = detail.full_wallpaper().or_else(|| detail.wallpaper()) else {
                return Task::none();
            };
            match DownloadRequest::for_wallpaper(wallpaper, destination.clone()) {
                Ok(req) => req,
                Err(err) => {
                    if let Some(detail) = &mut self.detail {
                        detail.download = DetailDownloadState::Failed {
                            message: err.user_message(),
                            part_path: err.part_path().map(PathBuf::from),
                        };
                    }
                    return Task::none();
                }
            }
        };

        let dl_op_id = self.allocate_op_id();
        let total_bytes = request.expected_file_size;
        let cancel = CancelToken::new();
        let cancel_for_task = cancel.clone();
        let downloader = OriginalDownloader::new(
            Arc::clone(&self.client),
            self.image_guard.clone(),
            Arc::clone(&self.file_publisher),
        );
        let msg_scope = scope.clone();
        let msg_id = id.clone();

        let (task, handle) = Task::stream(iced::stream::channel(
            16,
            move |mut output: iced::futures::channel::mpsc::Sender<Message>| async move {
                let (prog_tx, mut prog_rx) = tokio::sync::mpsc::unbounded_channel();
                let download_fut =
                    downloader.download_with_progress(request, cancel_for_task, Some(prog_tx));
                tokio::pin!(download_fut);

                let result = loop {
                    tokio::select! {
                        res = &mut download_fut => break res,
                        Some(prog) = prog_rx.recv() => {
                            let _ = output.try_send(Message::DetailDownloadProgress {
                                op_id: dl_op_id,
                                scope: msg_scope.clone(),
                                id: msg_id.clone(),
                                bytes_written: prog.bytes_written,
                                total_bytes: prog.total_bytes,
                            });
                        }
                    }
                };

                let _ = output
                    .send(Message::DetailDownloadFinished {
                        op_id: dl_op_id,
                        scope: msg_scope,
                        id: msg_id,
                        result,
                    })
                    .await;
            },
        ))
        .abortable();

        if let Some(detail) = &mut self.detail {
            detail.download = DetailDownloadState::Downloading {
                op_id: dl_op_id,
                scope,
                target: destination,
                bytes_written: 0,
                total_bytes,
                cancel,
                handle: handle.abort_on_drop(),
            };
        }
        task
    }

    fn cancel_detail_download(&mut self) {
        let Some(detail) = &mut self.detail else {
            return;
        };
        if let DetailDownloadState::Downloading { cancel, handle, .. } = &detail.download {
            cancel.cancel();
            handle.abort();
            detail.download = DetailDownloadState::Idle;
        }
    }

    fn open_wallhaven_url(&mut self, url: &str) {
        if let Err(err) = (self.launch_browser)(url) {
            self.session.set_notice(Some(SessionNotice::warning(format!(
                "Could not open wallhaven.cc in browser: {err}"
            ))));
        }
    }

    fn handle_window_resized(&mut self, size: Size) -> Task<Message> {
        self.home.metrics.set_window_size(size.width, size.height);
        self.search.metrics.set_window_size(size.width, size.height);
        self.collections.metrics.set_window_size(
            (size.width - COLLECTIONS_SIDEBAR_WIDTH - 16.0).max(320.0),
            size.height,
        );
        self.tag.metrics.set_window_size(size.width, size.height);
        self.uploader
            .metrics
            .set_window_size(size.width, size.height);
        match self.active_gallery_kind() {
            Some(kind) => self.sync_gallery_thumbnails(kind),
            None => Task::none(),
        }
    }

    fn apply_gallery_key(&mut self, kind: GalleryKind, key: GridKey) -> Task<Message> {
        if key == GridKey::Activate {
            if let Some(idx) = self.gallery_selected(kind)
                && let Some(wallpaper) = self.gallery_items(kind).get(idx).cloned()
            {
                let current = self.route.clone();
                self.push_nav_stack(current);
                return self.open_detail(wallpaper.id.clone(), Some(wallpaper), kind);
            }
            return Task::none();
        }
        let len = self.gallery_items(kind).len();
        let next = self
            .gallery_metrics(kind)
            .move_selection(self.gallery_selected(kind), len, key);
        let Some(idx) = next else {
            return Task::none();
        };
        let prev = self.gallery_selected(kind);
        self.set_gallery_selected(kind, Some(idx));
        if prev == Some(idx) {
            return Task::none();
        }

        let mut tasks = Vec::new();
        if let Some(target_y) = self.gallery_metrics(kind).scroll_target_for(idx) {
            self.gallery_metrics_mut(kind).scroll_y = target_y;
            tasks.push(operation::scroll_to(
                self.gallery_scrollable_id(kind).clone(),
                AbsoluteOffset {
                    x: Some(0.0),
                    y: Some(target_y),
                },
            ));
        }

        let thumb_task = self.sync_gallery_thumbnails(kind);
        if thumb_task.units() > 0 {
            tasks.push(thumb_task);
        }

        let near_bottom = self.gallery_metrics(kind).is_near_bottom(len, None);
        if near_bottom && let Some(next_task) = self.spawn_next_gallery_page(kind) {
            tasks.push(next_task);
        }

        Task::batch(tasks)
    }

    fn apply_search_filters(&mut self, filters: DiscoveryFilters) -> Task<Message> {
        match self.search.controller.set_filters(filters) {
            Some(req) => self.reload_gallery(GalleryKind::Search, req),
            None => Task::none(),
        }
    }

    fn parse_search_filter_inputs(
        &self,
        base: &DiscoveryFilters,
    ) -> Result<DiscoveryFilters, String> {
        let atleast = parse_atleast_input(&self.search_atleast_input)?;
        let resolutions = parse_resolutions_input(&self.search_resolutions_input)?;
        let ratios = parse_ratios_input(&self.search_ratios_input)?;
        let colors = parse_color_input(&self.search_colors_input)?;
        Ok(DiscoveryFilters {
            atleast,
            resolutions,
            ratios,
            colors,
            ..base.clone()
        })
    }

    fn submit_search(&mut self) -> Task<Message> {
        let base = self.search.controller.filters().clone();
        let next_filters = match self.parse_search_filter_inputs(&base) {
            Ok(filters) => {
                self.search_filter_error = None;
                filters
            }
            Err(err) => {
                self.search_filter_error = Some(err);
                return Task::none();
            }
        };

        let terms = SearchTerms::raw(self.search_query_input.clone());
        let terms_req = self.search.controller.set_terms(terms);
        let filters_req = self.search.controller.set_filters(next_filters);
        let req = filters_req.or(terms_req).or_else(|| {
            matches!(
                self.search.controller.status(),
                DiscoveryStatus::Idle | DiscoveryStatus::Failed { .. }
            )
            .then(|| self.search.controller.refresh())
        });
        match req {
            Some(req) => self.reload_gallery(GalleryKind::Search, req),
            None => Task::none(),
        }
    }

    fn clear_search(&mut self) -> Task<Message> {
        let defaults = self.session.browse_defaults();
        self.sync_search_inputs_to_defaults(&defaults);
        let terms_req = self.search.controller.set_terms(SearchTerms::default());
        let filters_req = self
            .search
            .controller
            .set_filters(DiscoveryFilters::from_defaults(&defaults));
        let req = filters_req.or(terms_req).or_else(|| {
            matches!(
                self.search.controller.status(),
                DiscoveryStatus::Idle | DiscoveryStatus::Failed { .. }
            )
            .then(|| self.search.controller.refresh())
        });
        match req {
            Some(req) => self.reload_gallery(GalleryKind::Search, req),
            None => Task::none(),
        }
    }

    fn cache_cap_feedback_error(&mut self, message: impl Into<String>) {
        self.cache_cap_feedback = Some((true, message.into()));
    }

    fn apply_cache_cap(&mut self) -> Task<Message> {
        if self.cache_cap_in_flight.is_some() {
            return Task::none();
        }
        let cap_bytes = match parse_cache_cap_gb(&self.cache_cap_input) {
            Ok(bytes) => bytes,
            Err(message) => {
                self.cache_cap_feedback_error(message);
                return Task::none();
            }
        };
        if let Err(err) = SessionManager::validate_cache_cap_request(cap_bytes) {
            self.cache_cap_feedback_error(describe_cache_cap_error(&err));
            return Task::none();
        }
        let Some(cache) = self.cache.clone() else {
            // No live cache: keep the preference for the next time the cache opens.
            match self.session.commit_cache_cap(cap_bytes) {
                Ok(applied) => {
                    self.cache_cap_input = format_cap_gb(applied);
                    self.cache_cap_feedback = Some((
                        false,
                        format!(
                            "Saved {} GB. It applies the next time the image cache opens.",
                            format_cap_gb(applied)
                        ),
                    ));
                }
                Err(err) => self.cache_cap_feedback_error(err.to_string()),
            }
            return Task::none();
        };
        let op_id = self.allocate_op_id();
        self.cache_cap_in_flight = Some(op_id);
        self.cache_cap_feedback = None;
        Task::perform(
            async move { cache.set_cap(cap_bytes).await },
            move |result| Message::CacheCapApplied {
                op_id,
                cap_bytes,
                result,
            },
        )
    }

    fn finish_cache_cap(
        &mut self,
        op_id: u64,
        cap_bytes: u64,
        result: Result<(), CacheError>,
    ) -> Task<Message> {
        if self.cache_cap_in_flight != Some(op_id) {
            return Task::none();
        }
        self.cache_cap_in_flight = None;
        match result {
            Ok(()) => {
                let previous = self.session.local_preferences().cache_cap_bytes;
                match self.session.commit_cache_cap(cap_bytes) {
                    Ok(applied) => {
                        self.cache_cap_input = format_cap_gb(applied);
                        self.cache_cap_feedback = Some((
                            false,
                            format!("Image cache size set to {} GB.", format_cap_gb(applied)),
                        ));
                        self.request_cache_stats()
                    }
                    Err(err) => {
                        self.cache_cap_feedback_error(format!(
                            "{err}. The previous size ({} GB) stays in effect.",
                            format_cap_gb(previous)
                        ));
                        // The actor already accepted the new cap; put the old one back.
                        match self.cache.clone() {
                            Some(cache) => Task::perform(
                                async move {
                                    let _ = cache.set_cap(previous).await;
                                },
                                |()| Message::RefreshCacheStats,
                            ),
                            None => Task::none(),
                        }
                    }
                }
            }
            Err(CacheError::CapReductionRejected { requested, charged }) => {
                self.cache_cap_feedback_error(format!(
                    "Cannot lower the cache to {} yet: {} is still in use by active writes or pending deletions. Try again shortly.",
                    format_size(requested),
                    format_size(charged)
                ));
                self.request_cache_stats()
            }
            Err(CacheError::CapExceedsHardMax { .. }) => {
                self.cache_cap_feedback_error(describe_cache_cap_error(
                    &CacheCapError::ExceedsHardMax {
                        requested: cap_bytes,
                        hard_max: HARD_MAX_CACHE_CAP_BYTES,
                    },
                ));
                Task::none()
            }
            Err(err) => {
                self.cache_cap_feedback_error(format!("Could not change the cache size: {err}"));
                Task::none()
            }
        }
    }

    fn detail_view_state(&self, is_guest: bool) -> DetailViewState<'_> {
        let Some(detail) = &self.detail else {
            unreachable!("Route::Detail always holds DetailState");
        };
        let (metadata_loading, metadata_error) = match &detail.metadata {
            DetailMetadataState::Loading { .. } => (true, None),
            DetailMetadataState::Loaded(_) => (false, None),
            DetailMetadataState::Failed { error, .. } => (false, Some(error)),
        };
        let preview = match &detail.preview {
            DetailPreviewState::Idle => DetailPreviewView::Loading {
                fallback_used: false,
            },
            DetailPreviewState::Loading { fallback_used, .. } => DetailPreviewView::Loading {
                fallback_used: *fallback_used,
            },
            DetailPreviewState::Loaded {
                handle,
                stale,
                fallback_used,
            } => DetailPreviewView::Loaded {
                handle,
                stale: *stale,
                fallback_used: *fallback_used,
            },
            DetailPreviewState::Failed { message } => DetailPreviewView::Failed {
                message: message.as_str(),
            },
        };
        let download = match &detail.download {
            DetailDownloadState::Idle => DetailDownloadView::Idle,
            DetailDownloadState::PickingLocation { .. } => DetailDownloadView::PickingLocation,
            DetailDownloadState::Downloading {
                target,
                bytes_written,
                total_bytes,
                ..
            } => DetailDownloadView::Downloading {
                target: target.as_path(),
                bytes_written: *bytes_written,
                total_bytes: *total_bytes,
            },
            DetailDownloadState::Saved {
                path,
                bytes_written,
                collision_renamed,
            } => DetailDownloadView::Saved {
                path: path.as_path(),
                bytes_written: *bytes_written,
                collision_renamed: *collision_renamed,
            },
            DetailDownloadState::Failed { message, part_path } => DetailDownloadView::Failed {
                message: message.as_str(),
                part_path: part_path.as_deref(),
            },
        };
        let wallpaper = detail.wallpaper();
        let default_filename = detail.full_wallpaper().map(default_download_filename);
        DetailViewState {
            wallpaper_id: &detail.wallpaper_id,
            return_to: detail.return_to,
            metadata_loading,
            metadata_error,
            wallpaper,
            has_full_metadata: detail.full_wallpaper().is_some(),
            is_guest,
            preview,
            download,
            default_filename,
            copy_feedback: detail.copy_feedback.as_deref(),
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        let nav_buttons = [
            Route::Home,
            Route::Search,
            Route::Collections,
            Route::Account,
            Route::Settings,
        ]
        .into_iter()
        .fold(row![].spacing(8), |r, route| {
            let active = self.route == route;
            let btn = button(text(route.label()).size(14))
                .padding([8, 16])
                .on_press(Message::Navigate(route.clone()))
                .style(move |theme, status| {
                    let base = if active {
                        button::primary(theme, status)
                    } else {
                        button::secondary(theme, status)
                    };
                    button::Style {
                        background: Some(Background::Color(if active {
                            palette::ACCENT
                        } else {
                            surface_color()
                        })),
                        text_color: if active {
                            palette::CANVAS
                        } else {
                            palette::TEXT_PRIMARY
                        },
                        border: Border {
                            radius: 6.0.into(),
                            width: 1.0,
                            color: if active {
                                palette::ACCENT
                            } else {
                                palette::MUTED
                            },
                        },
                        ..base
                    }
                });
            r.push(btn)
        });

        let session_badge = match self.session.active_account() {
            Some(account) => text(format!(
                "{} ({})",
                account.username,
                account.verification.label()
            ))
            .size(13)
            .color(if account.verification.is_verified() {
                palette::SUCCESS
            } else {
                palette::WARNING
            }),
            None => text("Guest").size(13).color(palette::TEXT_SECONDARY),
        };

        let header = container(
            row![
                text("Wallora").size(20).color(palette::ACCENT),
                nav_buttons,
                session_badge,
            ]
            .spacing(24)
            .align_y(Alignment::Center),
        )
        .padding([12, 20])
        .width(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(Background::Color(surface_color())),
            ..Default::default()
        });

        let scope = self.session.scope();
        let is_guest = matches!(scope.kind, SessionKind::Guest);
        let selectable = selectable_purity(&scope, self.session.user_settings());
        let body = match &self.route {
            Route::Home => views::home::view(views::home::HomeViewState {
                controller: &self.home.controller,
                scrollable_id: &self.home.scrollable_id,
                metrics: &self.home.metrics,
                selected: self.home.selected,
                is_guest,
                thumbnail_state: |wallpaper| self.home.thumbnail_state(&wallpaper.id),
            }),
            Route::Search => views::search::view(views::search::SearchViewState {
                query_input: &self.search_query_input,
                atleast_input: &self.search_atleast_input,
                resolutions_input: &self.search_resolutions_input,
                ratios_input: &self.search_ratios_input,
                colors_input: &self.search_colors_input,
                filter_error: self.search_filter_error.as_deref(),
                filters_expanded: self.search_filters_expanded,
                input_ids: &self.search_input_ids,
                selectable_purity: selectable,
                is_guest,
                controller: &self.search.controller,
                scrollable_id: &self.search.scrollable_id,
                metrics: &self.search.metrics,
                selected: self.search.selected,
                thumbnail_state: |wallpaper| self.search.thumbnail_state(&wallpaper.id),
            }),
            Route::Collections => {
                views::collections::view(views::collections::CollectionsViewState {
                    controller: &self.collections.controller,
                    username_input: &self.collections_username_input,
                    username_input_id: &self.collections_username_input_id,
                    username_error: self.collections_username_error.as_deref(),
                    selectable_purity: selectable,
                    is_guest,
                    refresh_busy: self.refresh_in_flight.is_some(),
                    scrollable_id: &self.collections.scrollable_id,
                    metrics: &self.collections.metrics,
                    selected_tile: self.collections.selected,
                    thumbnail_state: |wallpaper| self.collections.thumbnail_state(&wallpaper.id),
                })
            }
            Route::Tag(_) => views::tag::view(views::tag::TagViewState {
                controller: &self.tag.controller,
                back_label: self.back_label_for_stack(),
                selectable_purity: selectable,
                is_guest,
                scrollable_id: &self.tag.scrollable_id,
                metrics: &self.tag.metrics,
                selected: self.tag.selected,
                thumbnail_state: |wallpaper| self.tag.thumbnail_state(&wallpaper.id),
            }),
            Route::Uploader(_) => views::uploader::view(views::uploader::UploaderViewState {
                controller: &self.uploader.controller,
                back_label: self.back_label_for_stack(),
                selectable_purity: selectable,
                is_guest,
                scrollable_id: &self.uploader.scrollable_id,
                metrics: &self.uploader.metrics,
                selected: self.uploader.selected,
                thumbnail_state: |wallpaper| self.uploader.thumbnail_state(&wallpaper.id),
            }),
            Route::Detail(_) => views::detail::view(self.detail_view_state(is_guest)),
            Route::Account => views::account::view(views::account::AccountViewState {
                session: &self.session,
                username_input: &self.username_input,
                api_key_input: &self.api_key_input,
                sign_in_busy: self.sign_in_in_flight.is_some(),
                refresh_busy: self.refresh_in_flight.is_some(),
            }),
            Route::Settings => views::settings::view(views::settings::SettingsViewState {
                paths: &self.paths,
                session: &self.session,
                proxy_mode: self.proxy_mode,
                proxy_url_input: &self.proxy_url_input,
                proxy_feedback: self.proxy_feedback.as_ref(),
                cache_available: self.cache.is_some(),
                cache_cap_input: &self.cache_cap_input,
                cache_cap_feedback: self.cache_cap_feedback.as_ref(),
                cache_cap_busy: self.cache_cap_in_flight.is_some(),
                cache_stats: self.cache_stats.as_ref(),
                image_guard: &self.image_guard,
            }),
        };

        let content = container(body)
            .padding(24)
            .width(Length::Fill)
            .height(Length::Fill);

        let mut shell = column![header];
        if let Some(notice) = self.session.notice() {
            let color = match notice.level {
                NoticeLevel::Info => palette::SUCCESS,
                NoticeLevel::Warning => palette::WARNING,
                NoticeLevel::Error => palette::DANGER,
            };
            shell = shell.push(
                container(text(notice.message.as_str()).size(13).color(color))
                    .padding([12, 24])
                    .width(Length::Fill),
            );
        }
        shell
            .push(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use wallora_core::domain::{
        AccountId, AspectRatio, CategoryMask, CollectionSummary, Dimensions, IdentityCheckOutcome,
        InMemoryCredentialStore, LocalPreferences, PurityMask, SearchMeta, SearchPage, SessionKind,
        SortOrder, Tag, Thumbs, UserSettings, UsernameVerification, Wallpaper,
    };
    use wallora_core::provider::{ImageClass, ProviderError};
    use wallora_core::services::cache::{CacheConnectivity, CacheKey, crc32_ieee};
    use wallora_core::services::{
        AccountRefreshPayload, CollectionItemsResponse, CollectionListResponse,
        CollectionListStatus, CollectionSource, CollectionUnsupportedAction,
        DetailUnsupportedAction, DownloadOutcome, OwnItemsBlockedReason, PageResponse, SignInProbe,
        TagMetadataResponse, UnsupportedFeature,
    };

    use crate::message::DetailCopyTarget;

    struct TestPaths(AppPaths);

    impl TestPaths {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("wallora-app-test-{}", AccountId::generate()));
            Self(AppPaths::from_roots(dir.join("config"), dir))
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.local_dir);
        }
    }

    fn test_app(paths: &TestPaths) -> WalloraApp {
        let storage = Arc::new(SqliteStore::open(&paths.0.db_path()).expect("file-backed sqlite"));
        let creds = Arc::new(InMemoryCredentialStore::new());
        let session = SessionManager::restore(storage, creds.clone());
        let client = Arc::new(WallhavenClient::new(creds).expect("client"));
        let (app, _) = WalloraApp::from_runtime(paths.0.clone(), session, client, None);
        app
    }

    fn cache_config(paths: &TestPaths) -> CacheConfig {
        CacheConfig::new(paths.0.cache_dir.clone(), SessionScope::guest(0))
    }

    /// Builds an app whose session is wired to a real cache actor, the same way `boot` does.
    fn test_app_with_cache(paths: &TestPaths, host: &Arc<CacheHost>) -> WalloraApp {
        let storage = Arc::new(SqliteStore::open(&paths.0.db_path()).expect("file-backed sqlite"));
        let creds = Arc::new(InMemoryCredentialStore::new());
        let cache = host.open(cache_config(paths)).expect("open cache");
        let session =
            SessionManager::restore_with_cache(storage, creds.clone(), Arc::new(cache.clone()));
        let client = Arc::new(WallhavenClient::new(creds).expect("client"));
        let (app, _) = WalloraApp::from_runtime(paths.0.clone(), session, client, Some(cache));
        app
    }

    fn cache_stats(host: &CacheHost, app: &WalloraApp) -> CacheStats {
        let cache = app.cache.clone().expect("app has a cache");
        host.block_on(async move { cache.stats().await })
            .expect("stats task")
            .expect("cache stats")
    }

    /// Cache scope updates travel over a channel, so wait briefly for the actor to apply them.
    fn wait_for_cache_scope(host: &CacheHost, app: &WalloraApp) -> CacheStats {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stats = cache_stats(host, app);
            if stats.current_scope == app.session.scope() || Instant::now() > deadline {
                return stats;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn sample_settings() -> UserSettings {
        UserSettings {
            thumb_size: "orig".to_string(),
            per_page: 32,
            purity: vec!["sfw".to_string(), "sketchy".to_string(), "nsfw".to_string()],
            categories: vec!["general".to_string(), "anime".to_string()],
            resolutions: vec!["2560x1440".to_string()],
            aspect_ratios: vec!["16x9".to_string()],
            toplist_range: Some("1w".to_string()),
            tag_blacklist: vec!["spoiler".to_string()],
            user_blacklist: Vec::new(),
            ai_art_filter: Some(1),
        }
    }

    #[test]
    fn sqlite_open_failure_keeps_app_guest_and_blocks_probes() {
        let paths = TestPaths::new();
        let db_path = paths.0.db_path();
        std::fs::create_dir_all(&db_path).unwrap();
        let host = CacheHost::new().expect("cache runtime");
        let (mut app, startup) = WalloraApp::boot(paths.0.clone(), Some(host.clone()));
        assert_eq!(startup.units(), 0);
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert!(
            app.session
                .notice()
                .unwrap()
                .message
                .contains("Sign-in is unavailable")
        );
        assert_eq!(app.update(Message::SubmitSignIn).units(), 0);
        assert!(app.sign_in_in_flight.is_none());
        drop(app);
        host.shutdown();
        std::fs::remove_dir(db_path).unwrap();
    }

    #[test]
    fn navigation_switches_routes_including_account() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert_eq!(app.route, Route::Home);

        let _ = app.update(Message::Navigate(Route::Search));
        assert_eq!(app.route, Route::Search);

        let _ = app.update(Message::Navigate(Route::Account));
        assert_eq!(app.route, Route::Account);

        let _ = app.update(Message::Navigate(Route::Settings));
        assert_eq!(app.route, Route::Settings);

        let _ = app.update(Message::Navigate(Route::Home));
        assert_eq!(app.route, Route::Home);
    }

    #[test]
    fn message_debug_redacts_api_key_input() {
        let secret = "wh_synthetic_secret_never_log_me";
        let msg = Message::ApiKeyChanged(secret.to_string());
        let formatted = format!("{msg:?}");
        assert!(!formatted.contains(secret));
        assert!(formatted.contains("***"));
    }

    fn start_sign_in(app: &mut WalloraApp) -> (u64, SignInProbe) {
        assert!(app.update(Message::SubmitSignIn).units() > 0);
        let op = app.sign_in_in_flight.as_ref().unwrap();
        (
            op.id,
            SignInProbe {
                start_scope: op.scope.clone(),
                username: "alice".into(),
                api_key: SecretString::new("wh_synthetic_key_1"),
                user_settings: Some(sample_settings()),
                own_collections: Some(vec![CollectionSummary {
                    id: 1,
                    label: "Default".into(),
                    views: 0,
                    public: true,
                    count: 2,
                }]),
                verification: UsernameVerification::Verified,
            },
        )
    }

    fn sign_in(app: &mut WalloraApp, username: &str) {
        let (op_id, mut probe) = start_sign_in(app);
        probe.username = username.into();
        let _ = app.update(Message::SignInProbeFinished {
            op_id,
            result: Ok(probe),
        });
        assert_eq!(app.session.active_account().unwrap().username, username);
    }

    fn refresh_result(scope: SessionScope, unauthorized: bool) -> AccountRefreshPayload {
        AccountRefreshPayload {
            request_scope: scope,
            user_settings: if unauthorized {
                Err(ProviderError::Unauthorized)
            } else {
                Ok(sample_settings())
            },
            own_collections: if unauthorized {
                Err(ProviderError::Unauthorized)
            } else {
                Ok(Vec::new())
            },
            identity_outcome: None,
        }
    }

    #[test]
    fn successful_sign_in_retires_an_old_refresh_and_ignores_its_delayed_results() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        sign_in(&mut app, "alice");
        let refresh = Message::RefreshAccount { reverify: true };
        assert!(app.update(refresh.clone()).units() > 0);
        let old = app.refresh_in_flight.as_ref().unwrap();
        let (old_id, old_scope, old_handle) = (old.id, old.scope.clone(), old.handle.clone());

        sign_in(&mut app, "bob");
        assert!(old_handle.is_aborted());
        assert!(app.refresh_in_flight.is_none());
        assert!(app.update(refresh).units() > 0);
        let bob_op = app.refresh_in_flight.as_ref().unwrap().id;
        let bob_scope = app.session.scope();
        for unauthorized in [false, true] {
            let _ = app.update(Message::AccountRefreshFinished {
                op_id: old_id,
                payload: refresh_result(old_scope.clone(), unauthorized),
            });
            assert_eq!(app.session.scope(), bob_scope);
            assert_eq!(app.session.own_collections().unwrap().len(), 1);
            assert_eq!(app.refresh_in_flight.as_ref().unwrap().id, bob_op);
        }

        // A current upstream 401 still revokes Bob's session.
        let _ = app.update(Message::AccountRefreshFinished {
            op_id: bob_op,
            payload: refresh_result(bob_scope.clone(), true),
        });
        assert_eq!(
            app.session.scope(),
            SessionScope::guest(bob_scope.generation + 1)
        );
        assert!(app.refresh_in_flight.is_none());
    }

    #[test]
    fn failed_switch_preserves_the_current_refresh_owner() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        sign_in(&mut app, "alice");
        let refresh = Message::RefreshAccount { reverify: true };
        assert!(app.update(refresh.clone()).units() > 0);
        let op = app.refresh_in_flight.as_ref().unwrap();
        let (op_id, scope, handle) = (op.id, op.scope.clone(), op.handle.clone());
        let _ = app.update(Message::SwitchAccount(AccountId::generate()));
        assert_eq!(app.refresh_in_flight.as_ref().unwrap().id, op_id);
        assert!(!handle.is_aborted());
        assert_eq!(app.update(refresh).units(), 0);
        let _ = app.update(Message::AccountRefreshFinished {
            op_id,
            payload: refresh_result(scope.clone(), false),
        });
        assert_eq!(app.session.scope(), scope);
        assert!(app.refresh_in_flight.is_none());
        assert!(app.session.own_collections().unwrap().is_empty());
    }

    #[test]
    fn sign_out_during_probe_aborts_it_and_ignores_a_queued_completion() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        sign_in(&mut app, "alice");
        let (op_id, probe) = start_sign_in(&mut app);
        let handle = app.sign_in_in_flight.as_ref().unwrap().handle.clone();
        let _ = app.update(Message::SignOut);
        assert!(handle.is_aborted());
        assert!(app.sign_in_in_flight.is_none());
        assert!(app.api_key_input.is_empty());
        let _ = app.update(Message::SignInProbeFinished {
            op_id,
            result: Ok(probe),
        });
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert!(app.session.accounts().is_empty());
    }

    #[test]
    fn settings_browser_failure_reaches_the_shell_notice() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        app.launch_browser = |url| {
            assert_eq!(url, WALLHAVEN_SETTINGS_URL);
            Err("synthetic launch failure".into())
        };
        let _ = app.update(Message::Navigate(Route::Settings));
        let _ = app.update(Message::OpenUpstreamSettings);
        assert_eq!(app.route, Route::Settings);
        assert!(
            app.session
                .notice()
                .unwrap()
                .message
                .contains("synthetic launch failure")
        );
        let _ = app.view();
    }

    #[test]
    fn proxy_update_keeps_single_client() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        let initial_client_ptr = Arc::as_ptr(&app.client);
        let _ = app.update(Message::ProxyModeSelected(ProxyModeChoice::Manual));
        let _ = app.update(Message::ProxyUrlChanged("http://127.0.0.1:8888".into()));
        let _ = app.update(Message::ApplyProxy);
        assert_eq!(Arc::as_ptr(&app.client), initial_client_ptr);
        assert_eq!(
            app.client.proxy(),
            ProxyConfig::Manual("http://127.0.0.1:8888/".into())
        );
    }

    #[test]
    fn boot_opens_the_cache_at_cache_dir_with_the_persisted_cap() {
        let paths = TestPaths::new();
        let host = CacheHost::new().expect("cache runtime");
        {
            let storage = SqliteStore::open(&paths.0.db_path()).expect("sqlite");
            storage
                .save_local_preferences(&LocalPreferences {
                    cache_cap_bytes: 3_000_000_000,
                    ..LocalPreferences::default()
                })
                .expect("save preferences");
        }

        let (app, startup) = WalloraApp::boot(paths.0.clone(), Some(host.clone()));
        assert_eq!(startup.units(), 0);
        assert!(app.cache.is_some());
        assert!(paths.0.cache_dir.join("cache.lock").exists());
        assert_eq!(app.cache_cap_input, "3");
        let stats = wait_for_cache_scope(&host, &app);
        assert_eq!(stats.cap_bytes, 3_000_000_000);
        assert_eq!(stats.current_scope, app.session.scope());
        assert!(app.session.notice().is_none());
        assert_eq!(
            app.image_guard.decoded_bytes_budget(),
            wallora_core::services::DEFAULT_DECODED_BYTES_BUDGET
        );

        drop(app);
        host.shutdown();
    }

    #[test]
    fn session_scope_changes_reach_the_cache_through_the_wired_sink() {
        let paths = TestPaths::new();
        let host = CacheHost::new().expect("cache runtime");
        let mut app = test_app_with_cache(&paths, &host);
        assert_eq!(
            wait_for_cache_scope(&host, &app).current_scope,
            app.session.scope()
        );

        sign_in(&mut app, "alice");
        let signed_in = app.session.scope();
        assert!(matches!(signed_in.kind, SessionKind::Account(_)));
        assert_eq!(wait_for_cache_scope(&host, &app).current_scope, signed_in);

        let _ = app.update(Message::SignOut);
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert_eq!(
            wait_for_cache_scope(&host, &app).current_scope,
            app.session.scope()
        );

        drop(app);
        host.shutdown();
    }

    fn finish_with(app: &mut WalloraApp, host: &CacheHost, cap_bytes: u64) {
        let op_id = app.cache_cap_in_flight.expect("cap change in flight");
        let cache = app.cache.clone().expect("cache");
        let result = host
            .block_on(async move { cache.set_cap(cap_bytes).await })
            .expect("set_cap task");
        let _ = app.update(Message::CacheCapApplied {
            op_id,
            cap_bytes,
            result,
        });
    }

    #[test]
    fn cache_cap_changes_validate_persist_apply_and_reject_over_ten_gb() {
        let paths = TestPaths::new();
        let host = CacheHost::new().expect("cache runtime");
        let mut app = test_app_with_cache(&paths, &host);
        assert_eq!(app.cache_cap_input, "2");

        let _ = app.update(Message::CacheCapInputChanged("1.5".into()));
        assert!(app.update(Message::ApplyCacheCap).units() > 0);
        assert!(app.cache_cap_in_flight.is_some());
        // A second apply while one is in flight does nothing.
        assert_eq!(app.update(Message::ApplyCacheCap).units(), 0);
        finish_with(&mut app, &host, 1_500_000_000);
        assert!(app.cache_cap_in_flight.is_none());
        assert_eq!(
            app.session.local_preferences().cache_cap_bytes,
            1_500_000_000
        );
        assert_eq!(cache_stats(&host, &app).cap_bytes, 1_500_000_000);
        assert_eq!(app.cache_cap_input, "1.5");
        assert_eq!(
            app.cache_cap_feedback.as_ref().map(|(err, _)| *err),
            Some(false)
        );

        for (input, expected) in [
            ("10.000000001", "cannot exceed 10 GB"),
            ("999999999999", "cannot exceed 10 GB"),
            ("0", "greater than zero"),
            ("abc", "Enter the cache size"),
            ("", "Enter the cache size"),
        ] {
            let _ = app.update(Message::CacheCapInputChanged(input.into()));
            assert_eq!(app.update(Message::ApplyCacheCap).units(), 0, "{input:?}");
            assert!(app.cache_cap_in_flight.is_none());
            let (is_error, message) = app.cache_cap_feedback.clone().expect("feedback");
            assert!(
                is_error && message.contains(expected),
                "{input:?}: {message}"
            );
            assert_eq!(
                app.session.local_preferences().cache_cap_bytes,
                1_500_000_000
            );
            assert_eq!(cache_stats(&host, &app).cap_bytes, 1_500_000_000);
        }

        // Exactly 10 GB is the hard maximum and is accepted.
        let _ = app.update(Message::CacheCapInputChanged("10".into()));
        assert!(app.update(Message::ApplyCacheCap).units() > 0);
        finish_with(&mut app, &host, 10_000_000_000);
        assert_eq!(
            app.session.local_preferences().cache_cap_bytes,
            10_000_000_000
        );
        assert_eq!(cache_stats(&host, &app).cap_bytes, 10_000_000_000);
        let _ = app.view();

        drop(app);
        host.shutdown();
    }

    #[test]
    fn live_cap_rejection_and_stale_completions_leave_the_preference_alone() {
        let paths = TestPaths::new();
        let host = CacheHost::new().expect("cache runtime");
        let mut app = test_app_with_cache(&paths, &host);

        let _ = app.update(Message::CacheCapInputChanged("0.0000005".into()));
        assert!(app.update(Message::ApplyCacheCap).units() > 0);
        let op_id = app.cache_cap_in_flight.unwrap();

        // A completion for another operation is ignored.
        let _ = app.update(Message::CacheCapApplied {
            op_id: op_id + 100,
            cap_bytes: 500,
            result: Ok(()),
        });
        assert_eq!(app.cache_cap_in_flight, Some(op_id));
        assert_eq!(
            app.session.local_preferences().cache_cap_bytes,
            2_000_000_000
        );

        let _ = app.update(Message::CacheCapApplied {
            op_id,
            cap_bytes: 500,
            result: Err(CacheError::CapReductionRejected {
                requested: 500,
                charged: 700,
            }),
        });
        assert!(app.cache_cap_in_flight.is_none());
        let (is_error, message) = app.cache_cap_feedback.clone().expect("feedback");
        assert!(is_error && message.contains("still in use"), "{message}");
        assert_eq!(
            app.session.local_preferences().cache_cap_bytes,
            2_000_000_000
        );
        assert_eq!(cache_stats(&host, &app).cap_bytes, 2_000_000_000);

        drop(app);
        host.shutdown();
    }

    #[test]
    fn cache_open_failure_is_a_non_fatal_warning_and_the_cap_still_saves() {
        let paths = TestPaths::new();
        let host = CacheHost::new().expect("cache runtime");
        // Another cache (for example a second window) already owns the root lock.
        let _owner = host.open(cache_config(&paths)).expect("first cache owner");

        let (mut app, startup) = WalloraApp::boot(paths.0.clone(), Some(host.clone()));
        assert_eq!(startup.units(), 0);
        assert!(app.cache.is_none());
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert!(
            app.session
                .notice()
                .unwrap()
                .message
                .contains("Another Wallora window is using the image cache")
        );
        assert_eq!(app.session.notice().unwrap().level, NoticeLevel::Warning);
        assert_eq!(app.update(Message::Navigate(Route::Settings)).units(), 0);
        let _ = app.view();

        let _ = app.update(Message::CacheCapInputChanged("2.5".into()));
        assert_eq!(app.update(Message::ApplyCacheCap).units(), 0);
        assert_eq!(
            app.session.local_preferences().cache_cap_bytes,
            2_500_000_000
        );
        let (is_error, message) = app.cache_cap_feedback.clone().expect("feedback");
        assert!(!is_error && message.contains("next time"), "{message}");
        let _ = app.update(Message::CacheCapInputChanged("11".into()));
        let _ = app.update(Message::ApplyCacheCap);
        assert_eq!(
            app.session.local_preferences().cache_cap_bytes,
            2_500_000_000
        );

        drop(app);
        host.shutdown();
    }

    #[test]
    fn missing_cache_runtime_is_also_only_a_warning() {
        let paths = TestPaths::new();
        let (app, startup) = WalloraApp::boot(paths.0.clone(), None);
        assert_eq!(startup.units(), 0);
        assert!(app.cache.is_none());
        assert!(
            app.session
                .notice()
                .unwrap()
                .message
                .contains("could not be started")
        );
    }

    #[test]
    fn host_shutdown_stops_the_cache_and_releases_the_root_lock() {
        let paths = TestPaths::new();
        let host = CacheHost::new().expect("cache runtime");
        let app = test_app_with_cache(&paths, &host);
        let cache = app.cache.clone().expect("cache");

        host.shutdown();
        host.shutdown();
        let probe = cache.clone();
        let after = host
            .block_on(async move { probe.stats().await })
            .expect("stats task");
        assert_eq!(after, Err(CacheError::ShuttingDown));

        // The orderly shutdown released the lock, so a fresh start can take it again.
        let second = CacheHost::new().expect("second runtime");
        let reopened = second.open(cache_config(&paths)).expect("lock released");
        drop(reopened);
        second.shutdown();
    }

    #[test]
    fn settings_view_shows_cache_status_and_tolerates_stat_failures() {
        let paths = TestPaths::new();
        let host = CacheHost::new().expect("cache runtime");
        let mut app = test_app_with_cache(&paths, &host);
        let _ = app.update(Message::Navigate(Route::Settings));
        let _ = app.view();

        let stats = cache_stats(&host, &app);
        let _ = app.update(Message::CacheStatsLoaded(Ok(stats.clone())));
        assert_eq!(app.cache_stats, Some(stats.clone()));
        let _ = app.update(Message::CacheStatsLoaded(Err(CacheError::ShuttingDown)));
        assert_eq!(app.cache_stats, Some(stats));
        let _ = app.view();

        drop(app);
        host.shutdown();
    }

    fn sample_wallpaper(id: &str, purity: Purity) -> Wallpaper {
        Wallpaper {
            id: WallpaperId::new(id),
            url: format!("https://wallhaven.cc/w/{id}"),
            short_url: None,
            uploader: None,
            views: 10,
            favorites: 2,
            source: String::new(),
            purity,
            category: Category::General,
            dimension_x: 1920,
            dimension_y: 1080,
            resolution: "1920x1080".to_string(),
            ratio: "1.78".to_string(),
            file_size: 1234,
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

    fn sample_page(
        ids: &[&str],
        current_page: u32,
        last_page: u32,
        seed: Option<&str>,
    ) -> SearchPage {
        SearchPage {
            data: ids
                .iter()
                .map(|id| sample_wallpaper(id, Purity::Sfw))
                .collect(),
            meta: SearchMeta {
                current_page,
                last_page,
                per_page: ids.len().max(1) as u32,
                total: u64::from(last_page) * 24,
                query: None,
                seed: seed.map(str::to_string),
            },
        }
    }

    fn deliver_page(
        app: &mut WalloraApp,
        kind: GalleryKind,
        page: u32,
        result: Result<SearchPage, ProviderError>,
    ) -> Task<Message> {
        let generation = app.gallery(kind).controller.generation();
        let scope = app.session.scope();
        app.update(Message::DiscoveryPageLoaded {
            kind,
            response: Box::new(PageResponse {
                generation,
                scope,
                page,
                result,
            }),
        })
    }

    fn sample_loaded_thumb(stale: bool) -> LoadedThumbnail {
        LoadedThumbnail {
            handle: iced_image::Handle::from_rgba(1, 1, vec![255, 128, 0, 255]),
            stale,
        }
    }

    fn deliver_thumb(
        app: &mut WalloraApp,
        kind: GalleryKind,
        id: &str,
        result: Result<LoadedThumbnail, ImagePipelineError>,
    ) -> Task<Message> {
        let (generation, _) = app.gallery_generation_and_thumbnails_mut(kind);
        let scope = app.session.scope();
        app.update(Message::ThumbnailLoaded {
            kind,
            generation,
            scope,
            id: WallpaperId::new(id),
            result,
        })
    }

    #[test]
    fn boot_interactive_starts_home_latest_load() {
        let paths = TestPaths::new();
        let (app, startup) = WalloraApp::boot_interactive(paths.0.clone(), None);
        assert!(startup.units() > 0);
        assert_eq!(app.route, Route::Home);
        assert_eq!(app.home.controller.view(), DiscoveryView::Latest);
        assert_eq!(*app.home.controller.status(), DiscoveryStatus::Loading);
        assert_eq!(app.home.page_in_flight.as_ref().map(|p| p.page), Some(1));
        let q = app.home.controller.query_for_page(1);
        assert_eq!(q.sorting, Some(Sorting::DateAdded));
        assert_eq!(q.purity, Some(PurityMask::SFW_ONLY));
        assert_eq!(q.categories, Some(CategoryMask::ALL));
        assert_eq!(*app.search.controller.status(), DiscoveryStatus::Idle);
    }

    #[test]
    fn home_switches_latest_toplist_range_and_random_seed_lifecycle() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let latest_gen = app.home.controller.generation();
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["l1", "l2"], 1, 3, None)),
        );
        assert_eq!(app.home.controller.items().len(), 2);
        let _ = app.view();

        // Switch to Toplist (defaults to 1M for Guest).
        assert!(app.update(Message::HomeSelectToplist).units() > 0);
        assert!(app.home.controller.generation() > latest_gen);
        assert_eq!(
            app.home.controller.view(),
            DiscoveryView::Toplist {
                range: TopRange::OneMonth
            }
        );
        let toplist_q = app.home.controller.query_for_page(1);
        assert_eq!(toplist_q.sorting, Some(Sorting::Toplist));
        assert_eq!(toplist_q.top_range, Some(TopRange::OneMonth));
        assert!(app.home.controller.items().is_empty());

        // Change Toplist range to 1w.
        assert!(
            app.update(Message::HomeSelectTopRange(TopRange::OneWeek))
                .units()
                > 0
        );
        assert_eq!(app.home_toplist_range, TopRange::OneWeek);
        assert_eq!(
            app.home.controller.query_for_page(1).top_range,
            Some(TopRange::OneWeek)
        );
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["t1", "t2"], 1, 2, None)),
        );
        let _ = app.view();

        // Switch to Random: page 1 has no seed, adopts meta.seed, keeps it for page 2, resets on refresh.
        assert!(app.update(Message::HomeSelectRandom).units() > 0);
        assert_eq!(app.home.controller.view(), DiscoveryView::Random);
        assert!(app.home.controller.query_for_page(1).seed.is_none());
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["r1", "r2"], 1, 3, Some("seed42"))),
        );
        assert_eq!(app.home.controller.seed(), Some("seed42"));
        assert_eq!(
            app.home.controller.query_for_page(2).seed.as_deref(),
            Some("seed42")
        );
        let _ = app.view();

        let prev_gen = app.home.controller.generation();
        assert!(
            app.update(Message::GalleryRefresh(GalleryKind::Home))
                .units()
                > 0
        );
        assert!(app.home.controller.generation() > prev_gen);
        assert!(app.home.controller.seed().is_none());
        assert!(app.home.controller.query_for_page(1).seed.is_none());
    }

    #[test]
    fn search_passes_raw_query_unchanged_applies_all_filters_and_validates_inputs() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert!(app.update(Message::Navigate(Route::Search)).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            1,
            Ok(sample_page(&["s1"], 1, 2, None)),
        );

        // Typing raw text does not submit until Enter/SearchSubmit.
        let raw_query = "  cat -\"red hair\" +night @bob id:9 type:png like:ab12  ";
        let gen_before = app.search.controller.generation();
        assert_eq!(
            app.update(Message::SearchQueryChanged(raw_query.to_string()))
                .units(),
            0
        );
        assert_eq!(app.search.controller.generation(), gen_before);

        assert!(app.update(Message::SearchSubmit).units() > 0);
        assert_eq!(
            app.search.controller.query_for_page(1).q.as_deref(),
            Some(raw_query)
        );
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            1,
            Ok(sample_page(&["s2"], 1, 2, None)),
        );

        // Apply every documented filter control.
        assert!(
            app.update(Message::SearchToggleCategory(Category::General))
                .units()
                > 0
        );
        assert!(
            app.update(Message::SearchTogglePurity(Purity::Sketchy))
                .units()
                > 0
        );
        assert!(
            app.update(Message::SearchTogglePurity(Purity::Nsfw))
                .units()
                > 0
        );
        assert!(
            app.update(Message::SearchSelectSorting(Sorting::Toplist))
                .units()
                > 0
        );
        assert!(
            app.update(Message::SearchSelectOrder(SortOrder::Asc))
                .units()
                > 0
        );
        assert!(
            app.update(Message::SearchSelectTopRange(TopRange::ThreeDays))
                .units()
                > 0
        );

        // Invalid filter text blocks submission and shows an error without touching the controller.
        let _ = app.update(Message::SearchAtleastChanged("bad-dim".into()));
        assert_eq!(app.update(Message::SearchSubmit).units(), 0);
        assert!(app.search_filter_error.is_some());
        let _ = app.view();

        let _ = app.update(Message::SearchAtleastChanged("1920x1080".into()));
        let _ = app.update(Message::SearchResolutionsChanged(
            "2560x1440, 3840x2160".into(),
        ));
        let _ = app.update(Message::SearchRatiosChanged("16x9, landscape".into()));
        let _ = app.update(Message::SearchColorsChanged("#3366AA".into()));
        assert!(app.update(Message::SearchSubmit).units() > 0);
        assert!(app.search_filter_error.is_none());

        let q = app.search.controller.query_for_page(1);
        assert_eq!(q.q.as_deref(), Some(raw_query));
        assert_eq!(
            q.categories,
            Some(CategoryMask {
                general: false,
                anime: true,
                people: true,
            })
        );
        assert_eq!(q.purity, Some(PurityMask::ALL));
        assert_eq!(q.sorting, Some(Sorting::Toplist));
        assert_eq!(q.order, Some(SortOrder::Asc));
        assert_eq!(q.top_range, Some(TopRange::ThreeDays));
        assert_eq!(q.atleast, Some(Dimensions::new(1920, 1080)));
        assert_eq!(
            q.resolutions,
            vec![Dimensions::new(2560, 1440), Dimensions::new(3840, 2160)]
        );
        assert_eq!(
            q.ratios,
            vec![AspectRatio::new("16x9"), AspectRatio::new("landscape")]
        );
        assert_eq!(q.colors.as_deref(), Some("3366aa"));

        // Guest selecting NSFW and receiving 401 surfaces upstream's error without revocation.
        let guest_scope = app.session.scope();
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            1,
            Err(ProviderError::Unauthorized),
        );
        assert_eq!(app.session.scope(), guest_scope);
        assert_eq!(
            *app.search.controller.status(),
            DiscoveryStatus::Failed {
                page: 1,
                error: ProviderError::Unauthorized,
            }
        );
        let _ = app.view();

        // Clear resets query & filters back to session defaults.
        assert!(app.update(Message::SearchClear).units() > 0);
        assert!(app.search_query_input.is_empty());
        assert!(app.search_atleast_input.is_empty());
        assert!(app.search_colors_input.is_empty());
        let reset_q = app.search.controller.query_for_page(1);
        assert!(reset_q.q.is_none());
        assert_eq!(reset_q.purity, Some(PurityMask::SFW_ONLY));
        assert_eq!(reset_q.categories, Some(CategoryMask::ALL));
        let _ = app.update(Message::SearchToggleFiltersPanel);
        assert!(!app.search_filters_expanded);
        let _ = app.view();
    }

    #[test]
    fn superseded_page_and_stale_thumbnail_responses_are_dropped() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let gen1 = app.home.controller.generation();
        let handle1 = app.home.page_in_flight.as_ref().unwrap().handle.clone();

        // Switch feed before gen 1 finishes: handle1 is aborted and late gen 1 page is ignored.
        assert!(app.update(Message::HomeSelectToplist).units() > 0);
        assert!(handle1.is_aborted());
        let gen2 = app.home.controller.generation();
        assert!(gen2 > gen1);

        let _ = app.update(Message::DiscoveryPageLoaded {
            kind: GalleryKind::Home,
            response: Box::new(PageResponse {
                generation: gen1,
                scope: app.session.scope(),
                page: 1,
                result: Ok(sample_page(&["old1"], 1, 1, None)),
            }),
        });
        assert!(app.home.controller.items().is_empty());
        assert_eq!(*app.home.controller.status(), DiscoveryStatus::Loading);

        // Deliver gen 2 page 1; thumbnails start InFlight.
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["new1", "new2"], 1, 1, None)),
        );
        assert!(matches!(
            app.home.thumbnails.get(&WallpaperId::new("new1")),
            Some(ThumbnailEntry::InFlight { .. })
        ));

        // Stale generation and stale scope thumbnail completions are dropped.
        let _ = app.update(Message::ThumbnailLoaded {
            kind: GalleryKind::Home,
            generation: gen1,
            scope: app.session.scope(),
            id: WallpaperId::new("new1"),
            result: Ok(sample_loaded_thumb(false)),
        });
        let _ = app.update(Message::ThumbnailLoaded {
            kind: GalleryKind::Home,
            generation: gen2,
            scope: SessionScope::guest(app.session.scope().generation + 99),
            id: WallpaperId::new("new1"),
            result: Ok(sample_loaded_thumb(false)),
        });
        assert!(matches!(
            app.home.thumbnails.get(&WallpaperId::new("new1")),
            Some(ThumbnailEntry::InFlight { .. })
        ));

        // Matching completion with stale = true lands and renders cleanly.
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Home,
            "new1",
            Ok(sample_loaded_thumb(true)),
        );
        assert!(matches!(
            app.home.thumbnails.get(&WallpaperId::new("new1")),
            Some(ThumbnailEntry::Loaded { stale: true, .. })
        ));
        let _ = app.view();
    }

    #[test]
    fn session_transitions_cancel_in_flight_work_and_clear_previous_textures() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        sign_in(&mut app, "alice");
        let alice_id = app.session.active_account().unwrap().id.clone();
        sign_in(&mut app, "bob");

        // Signing in on Route::Home already started Bob's Home load; deliver page 1 and
        // populate one loaded texture + one in-flight thumbnail, then switch to Search and
        // populate a loaded texture + an in-flight page 2 fetch.
        assert_eq!(*app.home.controller.status(), DiscoveryStatus::Loading);
        assert_eq!(app.home.page_in_flight.as_ref().map(|p| p.page), Some(1));
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["bob-h1", "bob-h2"], 1, 2, None)),
        );
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Home,
            "bob-h1",
            Ok(sample_loaded_thumb(false)),
        );
        let (home_cancel, home_thumb_handle) = match app
            .home
            .thumbnails
            .get(&WallpaperId::new("bob-h2"))
            .unwrap()
        {
            ThumbnailEntry::InFlight { cancel, handle, .. } => (cancel.clone(), handle.clone()),
            _ => panic!("bob-h2 should be in flight"),
        };

        assert!(app.update(Message::Navigate(Route::Search)).units() > 0);
        let _ = app.update(Message::SearchQueryChanged("secret".into()));
        assert!(app.update(Message::SearchSubmit).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            1,
            Ok(sample_page(&["bob-s1"], 1, 3, None)),
        );
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Search,
            "bob-s1",
            Ok(sample_loaded_thumb(false)),
        );
        let next_req = app.search.controller.request_next_page().unwrap();
        let _ = app.spawn_page_request(GalleryKind::Search, next_req);
        let search_page_handle = app.search.page_in_flight.as_ref().unwrap().handle.clone();

        // SwitchAccount clears both galleries, cancels/aborts in-flight work, and reloads Search.
        assert!(app.update(Message::SwitchAccount(alice_id)).units() > 0);
        assert!(home_cancel.is_cancelled());
        assert!(home_thumb_handle.is_aborted());
        assert!(search_page_handle.is_aborted());
        assert!(app.home.thumbnails.is_empty());
        assert!(app.search.thumbnails.is_empty());
        assert!(app.home.controller.items().is_empty());
        assert!(app.search_query_input.is_empty());
        assert_eq!(app.search.page_in_flight.as_ref().unwrap().page, 1);

        // Mid-session 401 revocation on DiscoveryPageLoaded clears textures and transitions to Guest.
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            1,
            Ok(sample_page(&["alice-s1"], 1, 2, None)),
        );
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Search,
            "alice-s1",
            Ok(sample_loaded_thumb(false)),
        );
        let p2_req = app.search.controller.request_next_page().unwrap();
        let _ = app.spawn_page_request(GalleryKind::Search, p2_req);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            2,
            Err(ProviderError::Unauthorized),
        );
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert!(app.search.thumbnails.is_empty());
        assert!(app.home.thumbnails.is_empty());
        assert_eq!(
            app.search.page_in_flight.as_ref().map(|p| &p.scope.kind),
            Some(&SessionKind::Guest)
        );

        // Sign-in from Guest and Sign-out both clear textures and reset active route.
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            1,
            Ok(sample_page(&["guest-s1"], 1, 1, None)),
        );
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Search,
            "guest-s1",
            Ok(sample_loaded_thumb(false)),
        );
        sign_in(&mut app, "carol");
        assert!(app.search.thumbnails.is_empty());
        assert_eq!(
            app.search.page_in_flight.as_ref().map(|p| &p.scope),
            Some(&app.session.scope())
        );

        let carol_page_handle = app.search.page_in_flight.as_ref().unwrap().handle.clone();
        let _ = app.update(Message::SwitchToGuest);
        assert!(carol_page_handle.is_aborted());
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
    }

    #[test]
    fn scroll_away_cancels_out_of_viewport_thumbnail_loads() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let ids: Vec<String> = (0..40).map(|i| format!("w{i:02}")).collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&id_refs, 1, 1, None)),
        );

        // At scroll_y = 0 (viewport 1200x580 -> 4 cols, visible rows 0..=4 => 0..20), w00 is in
        // flight and w39 is not requested yet.
        let (w00_cancel, w00_handle) = match app.home.thumbnails.get(&WallpaperId::new("w00")) {
            Some(ThumbnailEntry::InFlight { cancel, handle, .. }) => {
                (cancel.clone(), handle.clone())
            }
            _ => panic!("w00 should start in flight"),
        };
        assert!(!w00_cancel.is_cancelled());
        assert!(!w00_handle.is_aborted());
        assert!(!app.home.thumbnails.contains_key(&WallpaperId::new("w39")));

        // Scroll to the bottom rows so row 0 leaves the visible + overscan window.
        let content_h = app.home.metrics.total_content_height(40);
        let _ = app.update(Message::GalleryScrolled {
            kind: GalleryKind::Home,
            offset_y: 1600.0,
            viewport_width: 1200.0,
            viewport_height: 450.0,
            content_height: content_h,
        });

        assert!(w00_cancel.is_cancelled());
        assert!(w00_handle.is_aborted());
        assert!(!app.home.thumbnails.contains_key(&WallpaperId::new("w00")));
        assert!(matches!(
            app.home.thumbnails.get(&WallpaperId::new("w39")),
            Some(ThumbnailEntry::InFlight { .. })
        ));
    }

    #[test]
    fn infinite_scroll_deduplicates_and_stops_at_last_page() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["p1a", "p1b", "p1c", "p1d"], 1, 2, None)),
        );

        let content_h = app.home.metrics.total_content_height(4);
        assert!(
            app.update(Message::GalleryScrolled {
                kind: GalleryKind::Home,
                offset_y: 200.0,
                viewport_width: 1200.0,
                viewport_height: 580.0,
                content_height: content_h,
            })
            .units()
                > 0
        );
        assert_eq!(app.home.page_in_flight.as_ref().map(|p| p.page), Some(2));
        let p2_handle = app.home.page_in_flight.as_ref().unwrap().handle.clone();

        // Scrolling again while page 2 is in flight does not start a duplicate fetch.
        assert_eq!(
            app.update(Message::GalleryScrolled {
                kind: GalleryKind::Home,
                offset_y: 210.0,
                viewport_width: 1200.0,
                viewport_height: 580.0,
                content_height: content_h,
            })
            .units(),
            0
        );
        assert!(!p2_handle.is_aborted());
        assert_eq!(app.home.page_in_flight.as_ref().map(|p| p.page), Some(2));

        // Deliver page 2 (last_page = 2); scrolling near bottom no longer starts page 3.
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            2,
            Ok(sample_page(&["p2a", "p2b"], 2, 2, None)),
        );
        assert!(!app.home.controller.can_load_more());
        let content_h = app.home.metrics.total_content_height(6);
        let _ = app.update(Message::GalleryScrolled {
            kind: GalleryKind::Home,
            offset_y: 300.0,
            viewport_width: 1200.0,
            viewport_height: 580.0,
            content_height: content_h,
        });
        assert!(app.home.page_in_flight.is_none());
    }

    #[test]
    fn discovery_error_and_retry_recover_pages_and_failed_thumbnails() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let _ = deliver_page(&mut app, GalleryKind::Home, 1, Err(ProviderError::Network));
        assert_eq!(
            *app.home.controller.status(),
            DiscoveryStatus::Failed {
                page: 1,
                error: ProviderError::Network,
            }
        );
        let _ = app.view();

        assert!(app.update(Message::GalleryRetry(GalleryKind::Home)).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["a", "b"], 1, 2, None)),
        );
        assert_eq!(*app.home.controller.status(), DiscoveryStatus::Ready);

        // Page 2 error leaves page 1 visible and blocks further scroll hammer until Retry.
        let req2 = app.home.controller.request_next_page().unwrap();
        let _ = app.spawn_page_request(GalleryKind::Home, req2);
        let _ = deliver_page(&mut app, GalleryKind::Home, 2, Err(ProviderError::Timeout));
        assert_eq!(app.home.controller.items().len(), 2);
        let _ = app.update(Message::GalleryScrolled {
            kind: GalleryKind::Home,
            offset_y: 200.0,
            viewport_width: 1200.0,
            viewport_height: 580.0,
            content_height: 400.0,
        });
        assert!(app.home.page_in_flight.is_none());

        assert!(app.update(Message::GalleryRetry(GalleryKind::Home)).units() > 0);
        assert_eq!(app.home.page_in_flight.as_ref().map(|p| p.page), Some(2));
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            2,
            Ok(sample_page(&["c"], 2, 2, None)),
        );
        assert_eq!(app.home.controller.items().len(), 3);

        // Failed thumbnail transitions to Failed and RetryThumbnail re-queues it.
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Home,
            "a",
            Err(ImagePipelineError::Provider(ProviderError::Timeout)),
        );
        assert!(matches!(
            app.home.thumbnails.get(&WallpaperId::new("a")),
            Some(ThumbnailEntry::Failed { .. })
        ));
        let _ = app.view();
        assert!(
            app.update(Message::RetryThumbnail {
                kind: GalleryKind::Home,
                id: WallpaperId::new("a"),
            })
            .units()
                > 0
        );
        assert!(matches!(
            app.home.thumbnails.get(&WallpaperId::new("a")),
            Some(ThumbnailEntry::InFlight { .. })
        ));
    }

    #[test]
    fn keyboard_grid_navigation_moves_selection_and_respects_search_input_focus() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let ids: Vec<String> = (0..20).map(|i| format!("k{i:02}")).collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&id_refs, 1, 1, None)),
        );

        let _ = app.update(Message::RuntimeGalleryKey(GridKey::Down));
        assert_eq!(app.home.selected, Some(0));
        let _ = app.update(Message::RuntimeGalleryKey(GridKey::Right));
        assert_eq!(app.home.selected, Some(1));
        let _ = app.update(Message::RuntimeGalleryKey(GridKey::Down));
        assert_eq!(app.home.selected, Some(5));
        let _ = app.update(Message::RuntimeGalleryKey(GridKey::Left));
        assert_eq!(app.home.selected, Some(4));
        let _ = app.update(Message::RuntimeGalleryKey(GridKey::Up));
        assert_eq!(app.home.selected, Some(0));
        let _ = app.update(Message::RuntimeGalleryKey(GridKey::PageDown));
        assert!(app.home.selected.unwrap() > 0);
        let _ = app.update(Message::RuntimeGalleryKey(GridKey::PageUp));
        assert_eq!(app.home.selected, Some(0));
        let _ = app.update(Message::RuntimeGalleryKey(GridKey::End));
        assert_eq!(app.home.selected, Some(19));
        assert!(app.home.metrics.scroll_y > 0.0);
        let _ = app.update(Message::RuntimeGalleryKey(GridKey::Home));
        assert_eq!(app.home.selected, Some(0));
        assert_eq!(app.home.metrics.scroll_y, 0.0);

        // On Search, RuntimeGalleryKey queries input focus first; focused inputs suppress grid nav.
        assert!(app.update(Message::Navigate(Route::Search)).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            1,
            Ok(sample_page(&["s0", "s1"], 1, 1, None)),
        );
        assert!(
            app.update(Message::RuntimeGalleryKey(GridKey::Down))
                .units()
                > 0
        );
        assert_eq!(app.search.selected, None);

        assert_eq!(
            app.update(Message::SearchKeyFocusChecked {
                key: GridKey::Down,
                any_input_focused: true,
            })
            .units(),
            0
        );
        assert_eq!(app.search.selected, None);

        let _ = app.update(Message::SearchKeyFocusChecked {
            key: GridKey::Down,
            any_input_focused: false,
        });
        assert_eq!(app.search.selected, Some(0));
    }

    #[test]
    fn unsupported_feature_handoffs_open_exact_wallhaven_urls_and_report_errors() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        static LAUNCHED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        LAUNCHED.lock().unwrap().clear();
        app.launch_browser = |url| {
            wallora_windows::validate_wallhaven_handoff_url(url)?;
            LAUNCHED.lock().unwrap().push(url.to_string());
            Ok(())
        };

        let _ = app.update(Message::OpenUnsupportedFeature(
            UnsupportedFeature::HotSorting,
        ));
        let _ = app.update(Message::OpenUnsupportedFeature(
            UnsupportedFeature::AiArtFilter,
        ));
        assert_eq!(
            *LAUNCHED.lock().unwrap(),
            vec![
                "https://wallhaven.cc/hot".to_string(),
                "https://wallhaven.cc/search".to_string(),
            ]
        );

        app.launch_browser = |_| Err("browser launch blocked".into());
        let _ = app.update(Message::OpenUnsupportedFeature(
            UnsupportedFeature::HotSorting,
        ));
        assert!(
            app.session
                .notice()
                .unwrap()
                .message
                .contains("browser launch blocked")
        );
    }

    #[test]
    fn thumbnail_loader_uses_cache_pipeline_when_open_and_uncached_fallback_otherwise() {
        let paths = TestPaths::new();
        let host = CacheHost::new().expect("cache runtime");
        let app = test_app_with_cache(&paths, &host);
        assert!(matches!(app.thumbnail_loader, ThumbnailLoader::Cached(_)));

        let url_str = "https://th.wallhaven.cc/lg/ab/cached-thumb.png";
        let scope = app.session.scope();
        let cache = app.cache.clone().unwrap();
        let png_bytes = {
            fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
                out.extend_from_slice(&(data.len() as u32).to_be_bytes());
                out.extend_from_slice(kind);
                out.extend_from_slice(data);
                let mut crc_input = Vec::with_capacity(4 + data.len());
                crc_input.extend_from_slice(kind);
                crc_input.extend_from_slice(data);
                out.extend_from_slice(&crc32_ieee(&crc_input).to_be_bytes());
            }

            let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
            push_chunk(&mut out, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
            let raw = [0u8, 10, 20, 30, 255];
            let mut s1 = 1u32;
            let mut s2 = 0u32;
            for &b in &raw {
                s1 = (s1 + u32::from(b)) % 65521;
                s2 = (s2 + s1) % 65521;
            }
            let adler = (s2 << 16) | s1;
            let mut idat = vec![0x78, 0x01, 0x01, 0x05, 0x00, 0xFA, 0xFF];
            idat.extend_from_slice(&raw);
            idat.extend_from_slice(&adler.to_be_bytes());
            push_chunk(&mut out, b"IDAT", &idat);
            push_chunk(&mut out, b"IEND", &[]);
            out
        };
        let key = CacheKey::new(ImageClass::Thumbnail, url_str).unwrap();
        let loader = app.thumbnail_loader.clone();
        let (loaded, cancelled_err) = host
            .block_on(async move {
                cache
                    .put(key, scope.clone(), Some(png_bytes.len() as u64), png_bytes)
                    .await
                    .unwrap();
                cache
                    .set_connectivity(CacheConnectivity::Offline)
                    .await
                    .unwrap();
                let loaded = loader
                    .load(
                        ImageRequest::thumbnail_url(url_str, scope.clone()).unwrap(),
                        CancelToken::new(),
                    )
                    .await
                    .expect("offline cache hit decodes");
                let token = CancelToken::new();
                token.cancel();
                let err = loader
                    .load(ImageRequest::thumbnail_url(url_str, scope).unwrap(), token)
                    .await
                    .unwrap_err();
                (loaded, err)
            })
            .expect("cache host task");
        assert!(loaded.stale);
        assert_eq!(cancelled_err, ImagePipelineError::Cancelled);

        drop(app);
        host.shutdown();

        let uncached_paths = TestPaths::new();
        let uncached_app = test_app(&uncached_paths);
        assert!(matches!(
            uncached_app.thumbnail_loader,
            ThumbnailLoader::Uncached { .. }
        ));
        let uncached_loader = uncached_app.thumbnail_loader.clone();
        let scope = uncached_app.session.scope();
        let second_host = CacheHost::new().expect("second runtime");
        let err = second_host
            .block_on(async move {
                let token = CancelToken::new();
                token.cancel();
                uncached_loader
                    .load(ImageRequest::thumbnail_url(url_str, scope).unwrap(), token)
                    .await
                    .unwrap_err()
            })
            .expect("uncached cancel task");
        assert_eq!(err, ImagePipelineError::Cancelled);
        second_host.shutdown();
    }

    fn sample_detail_wallpaper(id: &str, purity: Purity) -> Wallpaper {
        let mut wp = sample_wallpaper(id, purity);
        wp.short_url = Some(format!("https://whvn.cc/{id}"));
        wp.uploader = Some(wallora_core::domain::Uploader {
            username: "artist_one".to_string(),
            group: "Moderator".to_string(),
            avatar: None,
        });
        wp.source = "https://example.invalid/art".to_string();
        wp.colors = vec!["#1e1c1a".to_string(), "#f37316".to_string()];
        wp.file_size = 524_288;
        wp.file_type = "image/png".to_string();
        wp.path = format!("https://w.wallhaven.cc/full/ab/wallhaven-{id}.png");
        wp.tags = vec![wallora_core::domain::Tag {
            id: 42,
            name: "landscape".to_string(),
            alias: String::new(),
            category_id: 2,
            category: "Nature".to_string(),
            purity: Purity::Sfw,
            created_at: "2020-01-01 00:00:00".to_string(),
        }];
        wp
    }

    #[test]
    fn detail_opens_via_click_and_enter_and_back_preserves_gallery_state() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["d1", "d2"], 1, 2, None)),
        );
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Home,
            "d1",
            Ok(sample_loaded_thumb(false)),
        );
        app.home.metrics.scroll_y = 240.0;

        // Pressing Enter with no selection does nothing.
        assert_eq!(
            app.update(Message::RuntimeGalleryKey(GridKey::Activate))
                .units(),
            0
        );
        assert_eq!(app.route, Route::Home);

        // Clicking tile 1 opens Detail and retains return_to = Home.
        assert!(
            app.update(Message::GallerySelectTile {
                kind: GalleryKind::Home,
                index: 1,
            })
            .units()
                > 0
        );
        assert_eq!(app.route, Route::Detail(WallpaperId::new("d2")));
        assert_eq!(app.home.selected, Some(1));
        assert!(app.detail.is_some());

        // Back restores Home with items, scroll offset, selected index, and cached thumbnail intact.
        let back_task = app.update(Message::DetailBack);
        assert!(back_task.units() > 0);
        assert_eq!(app.route, Route::Home);
        assert!(app.detail.is_none());
        assert_eq!(app.home.controller.items().len(), 2);
        assert_eq!(app.home.metrics.scroll_y, 240.0);
        assert_eq!(app.home.selected, Some(1));
        assert!(matches!(
            app.home.thumbnails.get(&WallpaperId::new("d1")),
            Some(ThumbnailEntry::Loaded { .. })
        ));

        // Search gallery: focused input suppresses Enter; unfocused Enter opens Detail for selected tile.
        assert!(app.update(Message::Navigate(Route::Search)).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            1,
            Ok(sample_page(&["s1", "s2"], 1, 1, None)),
        );
        app.search.selected = Some(0);
        assert_eq!(
            app.update(Message::SearchKeyFocusChecked {
                key: GridKey::Activate,
                any_input_focused: true,
            })
            .units(),
            0
        );
        assert_eq!(app.route, Route::Search);

        assert!(
            app.update(Message::SearchKeyFocusChecked {
                key: GridKey::Activate,
                any_input_focused: false,
            })
            .units()
                > 0
        );
        assert_eq!(app.route, Route::Detail(WallpaperId::new("s1")));
        let _ = app.update(Message::DetailBack);
        assert_eq!(app.route, Route::Search);
    }

    #[test]
    fn detail_metadata_stale_dropping_guest_401_and_preview_fallback_work() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["m1"], 1, 1, None)),
        );

        assert!(
            app.update(Message::GallerySelectTile {
                kind: GalleryKind::Home,
                index: 0,
            })
            .units()
                > 0
        );
        let op_id = match &app.detail.as_ref().unwrap().metadata {
            DetailMetadataState::Loading { op_id, .. } => *op_id,
            _ => panic!("expected loading metadata"),
        };
        let scope = app.session.scope();
        let full = sample_detail_wallpaper("m1", Purity::Sfw);

        // Stale op_id and stale scope are dropped cleanly.
        let _ = app.update(Message::DetailMetadataLoaded {
            op_id: op_id + 99,
            scope: scope.clone(),
            id: WallpaperId::new("m1"),
            result: Box::new(Ok(full.clone())),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().metadata,
            DetailMetadataState::Loading { .. }
        ));
        let _ = app.update(Message::DetailMetadataLoaded {
            op_id,
            scope: SessionScope::guest(scope.generation + 5),
            id: WallpaperId::new("m1"),
            result: Box::new(Ok(full.clone())),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().metadata,
            DetailMetadataState::Loading { .. }
        ));

        // Delivering valid full metadata starts preview loading and renders cleanly.
        assert!(
            app.update(Message::DetailMetadataLoaded {
                op_id,
                scope: scope.clone(),
                id: WallpaperId::new("m1"),
                result: Box::new(Ok(full.clone())),
            })
            .units()
                > 0
        );
        assert!(app.detail.as_ref().unwrap().full_wallpaper().is_some());
        let (preview_op, fallback_used) = match &app.detail.as_ref().unwrap().preview {
            DetailPreviewState::Loading {
                op_id,
                fallback_used,
                ..
            } => (*op_id, *fallback_used),
            _ => panic!("expected preview loading"),
        };
        assert!(!fallback_used);
        let _ = app.view();

        // Reactive preview budget error falls back to thumbnail preview.
        assert!(
            app.update(Message::DetailPreviewLoaded {
                op_id: preview_op,
                scope: scope.clone(),
                id: WallpaperId::new("m1"),
                fallback_used: false,
                result: Err(ImagePipelineError::Provider(ProviderError::ImageTooLarge {
                    limit: PREVIEW_BYTE_LIMIT,
                })),
            })
            .units()
                > 0
        );
        let fallback_op = match &app.detail.as_ref().unwrap().preview {
            DetailPreviewState::Loading {
                op_id,
                fallback_used: true,
                ..
            } => *op_id,
            _ => panic!("expected fallback preview loading"),
        };
        let _ = app.update(Message::DetailPreviewLoaded {
            op_id: fallback_op,
            scope: scope.clone(),
            id: WallpaperId::new("m1"),
            fallback_used: true,
            result: Ok(sample_loaded_thumb(false)),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().preview,
            DetailPreviewState::Loaded {
                fallback_used: true,
                ..
            }
        ));
        let _ = app.view();

        // Proactive preview budget check uses fallback immediately when file_size > 64 MB.
        let mut oversized = full;
        oversized.file_size = (PREVIEW_BYTE_LIMIT as u64) + 1;
        let _ = app.update(Message::DetailRetryMetadata);
        let retry_op = match &app.detail.as_ref().unwrap().metadata {
            DetailMetadataState::Loading { op_id, .. } => *op_id,
            _ => panic!("expected loading metadata on retry"),
        };
        let _ = app.update(Message::DetailMetadataLoaded {
            op_id: retry_op,
            scope: scope.clone(),
            id: WallpaperId::new("m1"),
            result: Box::new(Ok(oversized)),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().preview,
            DetailPreviewState::Loading {
                fallback_used: true,
                ..
            }
        ));

        // Guest 401 surfaces upstream's error without revoking Guest session.
        let _ = app.update(Message::DetailRetryMetadata);
        let nsfw_op = match &app.detail.as_ref().unwrap().metadata {
            DetailMetadataState::Loading { op_id, .. } => *op_id,
            _ => panic!("expected loading metadata"),
        };
        let _ = app.update(Message::DetailMetadataLoaded {
            op_id: nsfw_op,
            scope: scope.clone(),
            id: WallpaperId::new("m1"),
            result: Box::new(Err(ProviderError::Unauthorized)),
        });
        assert_eq!(app.session.scope(), scope);
        assert!(matches!(
            app.detail.as_ref().unwrap().metadata,
            DetailMetadataState::Failed {
                error: ProviderError::Unauthorized,
                ..
            }
        ));
        let _ = app.view();
    }

    #[test]
    fn detail_scope_transitions_and_401_revocation_clear_state_and_cancel_tasks() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        sign_in(&mut app, "alice");
        assert_eq!(*app.home.controller.status(), DiscoveryStatus::Loading);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["a1"], 1, 1, None)),
        );

        let _ = app.update(Message::GallerySelectTile {
            kind: GalleryKind::Home,
            index: 0,
        });
        let scope = app.session.scope();
        let meta_op = match &app.detail.as_ref().unwrap().metadata {
            DetailMetadataState::Loading { op_id, .. } => *op_id,
            _ => panic!("expected metadata loading"),
        };
        let _ = app.update(Message::DetailMetadataLoaded {
            op_id: meta_op,
            scope: scope.clone(),
            id: WallpaperId::new("a1"),
            result: Box::new(Ok(sample_detail_wallpaper("a1", Purity::Sfw))),
        });
        let preview_cancel = match &app.detail.as_ref().unwrap().preview {
            DetailPreviewState::Loading { cancel, .. } => cancel.clone(),
            _ => panic!("expected preview loading"),
        };

        // SignOut cancels preview task, drops Detail state/texture, and exits Detail route.
        let _ = app.update(Message::SignOut);
        assert!(preview_cancel.is_cancelled());
        assert!(app.detail.is_none());
        assert_eq!(app.route, Route::Home);
        assert_eq!(app.session.scope().kind, SessionKind::Guest);

        // Account 401 revocation during Detail metadata load clears Detail and bumps generation.
        sign_in(&mut app, "bob");
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["b1"], 1, 1, None)),
        );
        let _ = app.update(Message::GallerySelectTile {
            kind: GalleryKind::Home,
            index: 0,
        });
        let bob_scope = app.session.scope();
        let bob_op = match &app.detail.as_ref().unwrap().metadata {
            DetailMetadataState::Loading { op_id, .. } => *op_id,
            _ => panic!("expected metadata loading"),
        };
        let _ = app.update(Message::DetailMetadataLoaded {
            op_id: bob_op,
            scope: bob_scope.clone(),
            id: WallpaperId::new("b1"),
            result: Box::new(Err(ProviderError::Unauthorized)),
        });
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert!(app.session.scope().generation > bob_scope.generation);
        assert!(app.detail.is_none());
        assert_eq!(app.route, Route::Home);
    }

    #[test]
    fn detail_tag_like_copy_handoff_and_download_lifecycle_work() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        static DETAIL_LAUNCHES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        DETAIL_LAUNCHES.lock().unwrap().clear();
        app.launch_browser = |url| {
            wallora_windows::validate_wallhaven_handoff_url(url)?;
            DETAIL_LAUNCHES.lock().unwrap().push(url.to_string());
            Ok(())
        };

        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["z9k2"], 1, 1, None)),
        );
        let _ = app.update(Message::GallerySelectTile {
            kind: GalleryKind::Home,
            index: 0,
        });
        let scope = app.session.scope();
        let meta_op = match &app.detail.as_ref().unwrap().metadata {
            DetailMetadataState::Loading { op_id, .. } => *op_id,
            _ => panic!("expected metadata loading"),
        };
        let wp = sample_detail_wallpaper("z9k2", Purity::Sfw);
        let _ = app.update(Message::DetailMetadataLoaded {
            op_id: meta_op,
            scope: scope.clone(),
            id: WallpaperId::new("z9k2"),
            result: Box::new(Ok(wp.clone())),
        });

        // Copy links (page, original, short) emit clipboard tasks and confirmation text.
        for (target, url, expected_text) in [
            (
                DetailCopyTarget::Page,
                wp.url.clone(),
                "Copied page URL to clipboard.",
            ),
            (
                DetailCopyTarget::Original,
                wp.path.clone(),
                "Copied original image URL to clipboard.",
            ),
            (
                DetailCopyTarget::Short,
                wp.short_url.clone().unwrap(),
                "Copied short URL to clipboard.",
            ),
        ] {
            let task = app.update(Message::DetailCopyLink { target, url });
            assert!(task.units() > 0);
            assert_eq!(
                app.detail.as_ref().unwrap().copy_feedback.as_deref(),
                Some(expected_text)
            );
        }

        // Crop & Scale and image-similar hand-offs open https://wallhaven.cc/w/z9k2.
        for action in DetailUnsupportedAction::ALL {
            assert_eq!(action.label(), "Open on wallhaven.cc");
            let _ = app.update(Message::DetailOpenHandOff(action));
        }
        assert_eq!(
            *DETAIL_LAUNCHES.lock().unwrap(),
            vec![
                "https://wallhaven.cc/w/z9k2".to_string(),
                "https://wallhaven.cc/w/z9k2".to_string(),
            ]
        );

        // Download flow: pick cancelled -> Idle; pick accepted -> progress -> saved (incl. collision note).
        assert!(app.update(Message::DetailDownloadOriginal).units() > 0);
        let pick_op = match &app.detail.as_ref().unwrap().download {
            DetailDownloadState::PickingLocation { op_id, .. } => *op_id,
            _ => panic!("expected PickingLocation"),
        };
        let _ = app.update(Message::DetailSavePathPicked {
            op_id: pick_op,
            scope: scope.clone(),
            id: WallpaperId::new("z9k2"),
            path: None,
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().download,
            DetailDownloadState::Idle
        ));

        assert!(app.update(Message::DetailDownloadOriginal).units() > 0);
        let pick_op2 = match &app.detail.as_ref().unwrap().download {
            DetailDownloadState::PickingLocation { op_id, .. } => *op_id,
            _ => panic!("expected PickingLocation"),
        };
        let chosen = paths.0.local_dir.join("wallhaven-z9k2.png");
        assert!(
            app.update(Message::DetailSavePathPicked {
                op_id: pick_op2,
                scope: scope.clone(),
                id: WallpaperId::new("z9k2"),
                path: Some(chosen.clone()),
            })
            .units()
                > 0
        );
        let dl_op = match &app.detail.as_ref().unwrap().download {
            DetailDownloadState::Downloading { op_id, .. } => *op_id,
            _ => panic!("expected Downloading"),
        };
        let _ = app.update(Message::DetailDownloadProgress {
            op_id: dl_op,
            scope: scope.clone(),
            id: WallpaperId::new("z9k2"),
            bytes_written: 1024,
            total_bytes: Some(524_288),
        });
        let _ = app.view();

        let renamed = paths.0.local_dir.join("wallhaven-z9k2 (1).png");
        let _ = app.update(Message::DetailDownloadFinished {
            op_id: dl_op,
            scope: scope.clone(),
            id: WallpaperId::new("z9k2"),
            result: Ok(DownloadOutcome {
                path: renamed.clone(),
                bytes_written: 524_288,
                dimensions: (1920, 1080),
                collision_renamed: true,
            }),
        });
        assert!(matches!(
            &app.detail.as_ref().unwrap().download,
            DetailDownloadState::Saved {
                path,
                collision_renamed: true,
                ..
            } if *path == renamed
        ));
        let _ = app.view();

        // Download failure keeps actionable message and partial path visible, and Cancel resets to Idle.
        assert!(app.update(Message::DetailDownloadOriginal).units() > 0);
        let pick_op3 = match &app.detail.as_ref().unwrap().download {
            DetailDownloadState::PickingLocation { op_id, .. } => *op_id,
            _ => panic!("expected PickingLocation"),
        };
        let _ = app.update(Message::DetailSavePathPicked {
            op_id: pick_op3,
            scope: scope.clone(),
            id: WallpaperId::new("z9k2"),
            path: Some(chosen.clone()),
        });
        let dl_op3 = match &app.detail.as_ref().unwrap().download {
            DetailDownloadState::Downloading { op_id, .. } => *op_id,
            _ => panic!("expected Downloading"),
        };
        let dl_cancel = match &app.detail.as_ref().unwrap().download {
            DetailDownloadState::Downloading { cancel, .. } => cancel.clone(),
            _ => panic!("expected cancel token"),
        };
        let _ = app.update(Message::DetailCancelDownload);
        assert!(dl_cancel.is_cancelled());
        assert!(matches!(
            app.detail.as_ref().unwrap().download,
            DetailDownloadState::Idle
        ));
        // Stale completion after cancel is dropped cleanly.
        let _ = app.update(Message::DetailDownloadFinished {
            op_id: dl_op3,
            scope: scope.clone(),
            id: WallpaperId::new("z9k2"),
            result: Err(DownloadError::DiskFull {
                detail: "disk full".to_string(),
                part_path: Some(paths.0.local_dir.join(".z9k2.part")),
            }),
        });
        assert!(matches!(
            app.detail.as_ref().unwrap().download,
            DetailDownloadState::Idle
        ));

        // Active download failure renders error and .part path cleanly.
        let _ = app.update(Message::DetailDownloadOriginal);
        let pick_op4 = match &app.detail.as_ref().unwrap().download {
            DetailDownloadState::PickingLocation { op_id, .. } => *op_id,
            _ => panic!("expected PickingLocation"),
        };
        let _ = app.update(Message::DetailSavePathPicked {
            op_id: pick_op4,
            scope: scope.clone(),
            id: WallpaperId::new("z9k2"),
            path: Some(chosen),
        });
        let dl_op4 = match &app.detail.as_ref().unwrap().download {
            DetailDownloadState::Downloading { op_id, .. } => *op_id,
            _ => panic!("expected Downloading"),
        };
        let part_file = paths.0.local_dir.join(".wallhaven-z9k2.png.abc.part");
        let _ = app.update(Message::DetailDownloadFinished {
            op_id: dl_op4,
            scope,
            id: WallpaperId::new("z9k2"),
            result: Err(DownloadError::DiskFull {
                detail: "disk full".to_string(),
                part_path: Some(part_file.clone()),
            }),
        });
        assert!(matches!(
            &app.detail.as_ref().unwrap().download,
            DetailDownloadState::Failed {
                part_path: Some(p),
                ..
            } if *p == part_file
        ));
        let _ = app.view();

        // Tag click opens Route::Tag(42), and "Search this tag" navigates to Search with id:42.
        assert!(
            app.update(Message::DetailSelectTag {
                id: 42,
                name: "landscape".to_string(),
            })
            .units()
                > 0
        );
        assert_eq!(app.route, Route::Tag(42));
        assert!(app.detail.is_none());
        assert_eq!(app.tag.controller.tag_id(), Some(42));
        assert!(app.update(Message::TagSearchThisTag).units() > 0);
        assert_eq!(app.route, Route::Search);
        assert_eq!(app.search_query_input, "id:42");
        assert_eq!(
            app.search.controller.query_for_page(1).q.as_deref(),
            Some("id:42")
        );

        // Open Detail from Search and click More like this -> navigates to Search with like:{id}.
        let _ = deliver_page(
            &mut app,
            GalleryKind::Search,
            1,
            Ok(sample_page(&["z9k2"], 1, 1, None)),
        );
        let _ = app.update(Message::GallerySelectTile {
            kind: GalleryKind::Search,
            index: 0,
        });
        assert_eq!(app.route, Route::Detail(WallpaperId::new("z9k2")));
        assert!(
            app.update(Message::DetailMoreLikeThis(WallpaperId::new("z9k2")))
                .units()
                > 0
        );
        assert_eq!(app.route, Route::Search);
        assert!(app.detail.is_none());
        assert_eq!(app.search_query_input, "like:z9k2");
        assert_eq!(
            app.search.controller.query_for_page(1).q.as_deref(),
            Some("like:z9k2")
        );
    }

    fn sign_in_with_collections(
        app: &mut WalloraApp,
        username: &str,
        verification: UsernameVerification,
        collections: Vec<CollectionSummary>,
    ) {
        let (op_id, mut probe) = start_sign_in(app);
        probe.username = username.into();
        probe.verification = verification;
        probe.own_collections = Some(collections);
        let _ = app.update(Message::SignInProbeFinished {
            op_id,
            result: Ok(probe),
        });
    }

    fn deliver_collection_list(
        app: &mut WalloraApp,
        username: &str,
        result: Result<Vec<CollectionSummary>, ProviderError>,
    ) -> Task<Message> {
        let generation = app.collections.controller.list_generation();
        let scope = app.session.scope();
        app.update(Message::CollectionsUserListLoaded(Box::new(
            CollectionListResponse {
                generation,
                scope,
                username: username.to_string(),
                result,
            },
        )))
    }

    fn deliver_collection_items(
        app: &mut WalloraApp,
        page: u32,
        result: Result<SearchPage, ProviderError>,
    ) -> Task<Message> {
        let generation = app.collections.controller.items_generation();
        let scope = app.session.scope();
        app.update(Message::CollectionsItemsPageLoaded(Box::new(
            CollectionItemsResponse {
                generation,
                scope,
                page,
                result,
            },
        )))
    }

    #[test]
    fn collections_route_navigation_and_unverified_own_item_gating() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);

        let _ = app.update(Message::Navigate(Route::Collections));
        assert_eq!(app.route, Route::Collections);
        assert_eq!(app.active_gallery_kind(), Some(GalleryKind::Collections));
        let _ = app.view();

        for verification in [
            UsernameVerification::UnverifiablePrivate,
            UsernameVerification::UnverifiableEmpty,
            UsernameVerification::Inconclusive,
        ] {
            let own = if verification == UsernameVerification::UnverifiableEmpty {
                Vec::new()
            } else {
                vec![CollectionSummary {
                    id: 1,
                    label: "Default".into(),
                    views: 0,
                    public: false,
                    count: 2,
                }]
            };
            sign_in_with_collections(&mut app, "alice", verification, own.clone());
            assert_eq!(
                app.collections.controller.own_collections(),
                Some(own.as_slice())
            );
            assert_eq!(
                app.update(Message::CollectionsSelectOwn(1)).units(),
                0,
                "unverified account must not spawn own collection item fetches"
            );
            assert!(app.collections.page_in_flight.is_none());
            assert!(app.collections.controller.items().is_empty());
            assert_eq!(
                app.collections.controller.items_blocked(),
                Some(OwnItemsBlockedReason::Unverified(verification))
            );
            let _ = app.view();
        }

        // Verified account shows "Default" (unresolved Favorites) and loads own items.
        sign_in_with_collections(
            &mut app,
            "alice",
            UsernameVerification::Verified,
            vec![CollectionSummary {
                id: 1,
                label: "Default".into(),
                views: 2,
                public: true,
                count: 2,
            }],
        );
        let summary = &app.collections.controller.own_collections().unwrap()[0];
        assert_eq!(
            app.collections
                .controller
                .display_own_collection_label(summary),
            "Default"
        );
        assert!(app.update(Message::CollectionsSelectOwn(1)).units() > 0);
        assert_eq!(
            app.collections.page_in_flight.as_ref().map(|p| p.page),
            Some(1)
        );
        let _ =
            deliver_collection_items(&mut app, 1, Ok(sample_page(&["own1", "own2"], 1, 1, None)));
        assert_eq!(app.collections.controller.items().len(), 2);
        assert!(matches!(
            app.collections
                .controller
                .selected_collection()
                .map(|s| &s.source),
            Some(CollectionSource::Own { .. })
        ));
        let _ = app.view();
    }

    #[test]
    fn collections_browse_user_pagination_detail_back_and_keyboard_focus() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        let _ = app.update(Message::Navigate(Route::Collections));

        // Invalid username validation blocks network calls.
        let _ = app.update(Message::CollectionsUsernameChanged("../bad".into()));
        assert_eq!(app.update(Message::CollectionsLoadUser).units(), 0);
        assert!(app.collections_username_error.is_some());
        let _ = app.view();

        // Load public collections for another user, select one, toggle purity, and paginate.
        let _ = app.update(Message::CollectionsUsernameChanged("curator".into()));
        assert!(app.update(Message::CollectionsLoadUser).units() > 0);
        assert_eq!(
            app.collections.controller.user_collections_status(),
            &CollectionListStatus::Loading
        );
        let _ = deliver_collection_list(
            &mut app,
            "curator",
            Ok(vec![CollectionSummary {
                id: 77,
                label: "Landscapes".into(),
                views: 15,
                public: true,
                count: 6,
            }]),
        );
        assert_eq!(
            app.collections.controller.user_collections_status(),
            &CollectionListStatus::Ready
        );
        let _ = app.view();

        assert!(
            app.update(Message::CollectionsSelectUserCollection(77))
                .units()
                > 0
        );
        let _ = deliver_collection_items(
            &mut app,
            1,
            Ok(sample_page(&["c1", "c2", "c3", "c4"], 1, 2, None)),
        );
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Collections,
            "c1",
            Ok(sample_loaded_thumb(false)),
        );

        // Toggle purity reloads page 1 with Sketchy enabled.
        assert!(
            app.update(Message::CollectionsTogglePurity(Purity::Sketchy))
                .units()
                > 0
        );
        assert!(app.collections.controller.purity().sketchy);
        let _ = deliver_collection_items(
            &mut app,
            1,
            Ok(sample_page(&["c1", "c2", "c3", "c4"], 1, 2, None)),
        );

        // Infinite scroll loads page 2 and stops at last_page = 2.
        let content_h = app.collections.metrics.total_content_height(4);
        assert!(
            app.update(Message::GalleryScrolled {
                kind: GalleryKind::Collections,
                offset_y: 220.0,
                viewport_width: 900.0,
                viewport_height: 580.0,
                content_height: content_h,
            })
            .units()
                > 0
        );
        let _ = deliver_collection_items(&mut app, 2, Ok(sample_page(&["c5", "c6"], 2, 2, None)));
        assert!(!app.collections.controller.can_load_more());
        let _ = app.view();

        // Keyboard focus suppression and Enter -> Detail -> Back to Collections.
        assert!(
            app.update(Message::RuntimeGalleryKey(GridKey::Down))
                .units()
                > 0
        );
        assert_eq!(
            app.update(Message::CollectionsKeyFocusChecked {
                key: GridKey::Down,
                input_focused: true,
            })
            .units(),
            0
        );
        assert_eq!(app.collections.selected, None);

        let _ = app.update(Message::CollectionsKeyFocusChecked {
            key: GridKey::Down,
            input_focused: false,
        });
        assert_eq!(app.collections.selected, Some(0));

        assert!(
            app.update(Message::CollectionsKeyFocusChecked {
                key: GridKey::Activate,
                input_focused: false,
            })
            .units()
                > 0
        );
        assert_eq!(app.route, Route::Detail(WallpaperId::new("c1")));
        let _ = app.view();

        let _ = app.update(Message::DetailBack);
        assert_eq!(app.route, Route::Collections);
        assert_eq!(app.collections.controller.items().len(), 6);
        assert_eq!(app.collections.selected, Some(0));
    }

    #[test]
    fn collections_and_detail_handoffs_open_validated_urls_and_report_errors() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        static HANDOFF_URLS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        HANDOFF_URLS.lock().unwrap().clear();
        app.launch_browser = |url| {
            wallora_windows::validate_wallhaven_handoff_url(url)?;
            HANDOFF_URLS.lock().unwrap().push(url.to_string());
            Ok(())
        };

        assert_eq!(
            CollectionUnsupportedAction::FavoriteToggle.label(),
            "Open on wallhaven.cc"
        );
        assert_eq!(
            CollectionUnsupportedAction::ManageCollections.label(),
            "Open on wallhaven.cc"
        );

        // Guest manage handoff points to wallhaven.cc root; account points to /user/{name}/favorites.
        let _ = app.update(Message::Navigate(Route::Collections));
        let _ = app.update(Message::CollectionsOpenManageHandOff);
        sign_in_with_collections(
            &mut app,
            "alice",
            UsernameVerification::Verified,
            Vec::new(),
        );
        let _ = app.update(Message::CollectionsOpenManageHandOff);

        // Detail favorite handoff opens https://wallhaven.cc/w/{id}.
        let _ = app.update(Message::Navigate(Route::Home));
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["fav9"], 1, 1, None)),
        );
        let _ = app.update(Message::GallerySelectTile {
            kind: GalleryKind::Home,
            index: 0,
        });
        let _ = app.update(Message::DetailOpenFavoriteHandOff);
        assert_eq!(
            *HANDOFF_URLS.lock().unwrap(),
            vec![
                "https://wallhaven.cc/".to_string(),
                "https://wallhaven.cc/user/alice/favorites".to_string(),
                "https://wallhaven.cc/w/fav9".to_string(),
            ]
        );

        app.launch_browser = |_| Err("browser unavailable".into());
        let _ = app.update(Message::DetailOpenFavoriteHandOff);
        assert!(
            app.session
                .notice()
                .unwrap()
                .message
                .contains("browser unavailable")
        );
    }

    #[test]
    fn collections_scope_transitions_and_refresh_downgrade_cancel_and_clear_state() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        let _ = app.update(Message::Navigate(Route::Collections));

        sign_in_with_collections(
            &mut app,
            "alice",
            UsernameVerification::Verified,
            vec![CollectionSummary {
                id: 5,
                label: "Secret".into(),
                views: 0,
                public: false,
                count: 2,
            }],
        );
        let alice_id = app.session.active_account().unwrap().id.clone();

        // Populate loaded items, loaded thumb, in-flight thumb, in-flight page 2, and list lookup.
        let _ = app.update(Message::CollectionsSelectOwn(5));
        let _ = deliver_collection_items(&mut app, 1, Ok(sample_page(&["p1", "p2"], 1, 2, None)));
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Collections,
            "p1",
            Ok(sample_loaded_thumb(false)),
        );
        let (p2_cancel, p2_handle) = match app
            .collections
            .thumbnails
            .get(&WallpaperId::new("p2"))
            .unwrap()
        {
            ThumbnailEntry::InFlight { cancel, handle, .. } => (cancel.clone(), handle.clone()),
            _ => panic!("p2 should be in flight"),
        };
        let next_req = app.collections.controller.request_next_page().unwrap();
        let _ = app.spawn_collection_items_request(next_req);
        let items_handle = app
            .collections
            .page_in_flight
            .as_ref()
            .unwrap()
            .handle
            .clone();
        let _ = app.update(Message::CollectionsUsernameChanged("bob".into()));
        let _ = app.update(Message::CollectionsLoadUser);
        let list_handle = app
            .collections
            .list_in_flight
            .as_ref()
            .unwrap()
            .handle
            .clone();

        // SwitchToGuest cancels everything, clears private collection state, and keeps Alice saved.
        let _ = app.update(Message::SwitchToGuest);
        assert!(p2_cancel.is_cancelled());
        assert!(p2_handle.is_aborted());
        assert!(items_handle.is_aborted());
        assert!(list_handle.is_aborted());
        assert!(app.collections.thumbnails.is_empty());
        assert!(app.collections.controller.items().is_empty());
        assert!(app.collections.controller.selected_collection().is_none());
        assert!(app.collections_username_input.is_empty());
        assert_eq!(app.session.scope().kind, SessionKind::Guest);

        // SwitchAccount to saved Alice -> verify -> load items -> downgrade verification clears items.
        assert!(app.update(Message::SwitchAccount(alice_id)).units() > 0);
        let refresh_op = app.refresh_in_flight.as_ref().unwrap().id;
        let _ = app.update(Message::AccountRefreshFinished {
            op_id: refresh_op,
            payload: AccountRefreshPayload {
                request_scope: app.session.scope(),
                user_settings: Ok(sample_settings()),
                own_collections: Ok(vec![CollectionSummary {
                    id: 5,
                    label: "Secret".into(),
                    views: 0,
                    public: false,
                    count: 2,
                }]),
                identity_outcome: Some(IdentityCheckOutcome::Verified),
            },
        });
        let _ = app.update(Message::CollectionsSelectOwn(5));
        let _ = deliver_collection_items(&mut app, 1, Ok(sample_page(&["a1"], 1, 1, None)));
        assert_eq!(app.collections.controller.items().len(), 1);

        let _ = app.update(Message::RefreshAccount { reverify: true });
        let downgrade_op = app.refresh_in_flight.as_ref().unwrap().id;
        let _ = app.update(Message::AccountRefreshFinished {
            op_id: downgrade_op,
            payload: AccountRefreshPayload {
                request_scope: app.session.scope(),
                user_settings: Ok(sample_settings()),
                own_collections: Ok(vec![CollectionSummary {
                    id: 5,
                    label: "Secret".into(),
                    views: 0,
                    public: false,
                    count: 2,
                }]),
                identity_outcome: Some(IdentityCheckOutcome::UnverifiablePrivate),
            },
        });
        assert!(app.collections.controller.items().is_empty());
        assert!(app.collections.controller.selected_collection().is_none());
        assert_eq!(
            app.collections.controller.items_blocked(),
            Some(OwnItemsBlockedReason::Unverified(
                UsernameVerification::UnverifiablePrivate
            ))
        );

        // SignOut clears active session and collection state; 401 revocation clears private items.
        let _ = app.update(Message::SignOut);
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert!(app.collections.controller.own_collections().is_none());

        sign_in_with_collections(
            &mut app,
            "bob",
            UsernameVerification::Verified,
            vec![CollectionSummary {
                id: 9,
                label: "BobPrivate".into(),
                views: 0,
                public: false,
                count: 1,
            }],
        );
        let _ = app.update(Message::CollectionsSelectOwn(9));
        let _ = deliver_collection_items(&mut app, 1, Ok(sample_page(&["b1"], 1, 2, None)));
        let req = app.collections.controller.request_next_page().unwrap();
        let _ = app.spawn_collection_items_request(req);
        let _ = deliver_collection_items(&mut app, 2, Err(ProviderError::Unauthorized));
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert!(app.collections.controller.items().is_empty());
        assert!(app.collections.controller.selected_collection().is_none());
    }

    fn deliver_tag_metadata(
        app: &mut WalloraApp,
        tag_id: u64,
        result: Result<Tag, ProviderError>,
    ) -> Task<Message> {
        let generation = app.tag.controller.metadata_generation();
        let scope = app.session.scope();
        app.update(Message::TagMetadataLoaded(Box::new(TagMetadataResponse {
            generation,
            scope,
            tag_id,
            result,
        })))
    }

    fn deliver_tag_page(
        app: &mut WalloraApp,
        page: u32,
        result: Result<SearchPage, ProviderError>,
    ) -> Task<Message> {
        let generation = app.tag.controller.items_generation();
        let scope = app.session.scope();
        app.update(Message::TagPageLoaded(Box::new(PageResponse {
            generation,
            scope,
            page,
            result,
        })))
    }

    fn deliver_uploader_page(
        app: &mut WalloraApp,
        page: u32,
        result: Result<SearchPage, ProviderError>,
    ) -> Task<Message> {
        let generation = app.uploader.controller.generation();
        let scope = app.session.scope();
        app.update(Message::UploaderPageLoaded(Box::new(PageResponse {
            generation,
            scope,
            page,
            result,
        })))
    }

    #[test]
    fn detail_to_tag_page_round_trip_filters_handoff_and_search_work() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        static TAG_LAUNCHES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        TAG_LAUNCHES.lock().unwrap().clear();
        app.launch_browser = |url| {
            wallora_windows::validate_wallhaven_handoff_url(url)?;
            TAG_LAUNCHES.lock().unwrap().push(url.to_string());
            Ok(())
        };

        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["h1"], 1, 1, None)),
        );
        let _ = app.update(Message::GallerySelectTile {
            kind: GalleryKind::Home,
            index: 0,
        });
        assert_eq!(app.route, Route::Detail(WallpaperId::new("h1")));
        let _ = app.view();

        // Clicking tag 42 opens Route::Tag(42), starts metadata + id:42 page 1 fetch.
        assert!(
            app.update(Message::DetailSelectTag {
                id: 42,
                name: "landscape".to_string(),
            })
            .units()
                > 0
        );
        assert_eq!(app.route, Route::Tag(42));
        assert_eq!(app.active_gallery_kind(), Some(GalleryKind::Tag));
        assert_eq!(
            app.tag.controller.query_for_page(1).unwrap().q.as_deref(),
            Some("id:42")
        );
        let _ = app.view();

        // Metadata failure + retry, then successful load.
        let _ = deliver_tag_metadata(&mut app, 42, Err(ProviderError::Timeout));
        assert!(matches!(
            app.tag.controller.metadata_status(),
            TagMetadataStatus::Failed(ProviderError::Timeout)
        ));
        let _ = app.view();
        assert!(app.update(Message::TagRetryMetadata).units() > 0);
        let _ = deliver_tag_metadata(
            &mut app,
            42,
            Ok(Tag {
                id: 42,
                name: "landscape".to_string(),
                alias: "scenery".to_string(),
                category_id: 2,
                category: "Nature".to_string(),
                purity: Purity::Sfw,
                created_at: "2020-01-01 00:00:00".to_string(),
            }),
        );
        assert_eq!(
            app.tag.controller.tag().map(|t| t.alias.as_str()),
            Some("scenery")
        );

        // Deliver page 1, load thumbnail, scroll & infinite-scroll page 2.
        let _ = deliver_tag_page(
            &mut app,
            1,
            Ok(sample_page(&["t1", "t2", "t3", "t4"], 1, 2, None)),
        );
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Tag,
            "t1",
            Ok(sample_loaded_thumb(false)),
        );
        let _ = app.view();

        let content_h = app.tag.metrics.total_content_height(4);
        assert!(
            app.update(Message::GalleryScrolled {
                kind: GalleryKind::Tag,
                offset_y: 240.0,
                viewport_width: 1200.0,
                viewport_height: 580.0,
                content_height: content_h,
            })
            .units()
                > 0
        );
        let _ = deliver_tag_page(&mut app, 2, Ok(sample_page(&["t5"], 2, 2, None)));
        assert!(!app.tag.controller.can_load_more());
        assert_eq!(app.tag.controller.items().len(), 5);

        // Keyboard select & Enter opens Detail(t1); Back returns to Tag(42) preserving scroll & items.
        let _ = app.update(Message::RuntimeGalleryKey(GridKey::Home));
        app.tag.metrics.scroll_y = 240.0;
        assert_eq!(app.tag.selected, Some(0));
        assert!(
            app.update(Message::RuntimeGalleryKey(GridKey::Activate))
                .units()
                > 0
        );
        assert_eq!(app.route, Route::Detail(WallpaperId::new("t1")));
        let _ = app.view();

        let back_to_tag = app.update(Message::DetailBack);
        assert!(back_to_tag.units() > 0);
        assert_eq!(app.route, Route::Tag(42));
        assert_eq!(app.tag.controller.items().len(), 5);
        assert_eq!(app.tag.metrics.scroll_y, 240.0);
        assert_eq!(app.tag.selected, Some(0));
        assert!(matches!(
            app.tag.thumbnails.get(&WallpaperId::new("t1")),
            Some(ThumbnailEntry::Loaded { .. })
        ));

        // Purity/sorting/order/toplist controls keep metadata intact and reload items.
        assert!(
            app.update(Message::TagTogglePurity(Purity::Sketchy))
                .units()
                > 0
        );
        assert!(app.tag.controller.purity().sketchy);
        assert!(app.update(Message::TagSelectOrder(SortOrder::Asc)).units() > 0);
        assert_eq!(app.tag.controller.order(), Some(SortOrder::Asc));
        assert!(
            app.update(Message::TagSelectTopRange(TopRange::OneWeek))
                .units()
                > 0
        );
        assert_eq!(app.tag.controller.sorting(), Sorting::Toplist);
        assert_eq!(app.tag.controller.top_range(), TopRange::OneWeek);
        assert!(app.tag.controller.tag().is_some());

        // Empty state renders cleanly.
        let _ = deliver_tag_page(&mut app, 1, Ok(sample_page(&[], 1, 1, None)));
        assert_eq!(*app.tag.controller.items_status(), DiscoveryStatus::Empty);
        let _ = app.view();

        // Tag Directory hand-off opens https://wallhaven.cc/tags.
        let _ = app.update(Message::TagOpenDirectoryHandOff);
        assert_eq!(
            *TAG_LAUNCHES.lock().unwrap(),
            vec!["https://wallhaven.cc/tags".to_string()]
        );

        // TagBack returns to Detail(h1), and DetailBack returns to Home.
        assert!(app.update(Message::TagBack).units() > 0);
        assert_eq!(app.route, Route::Detail(WallpaperId::new("h1")));
        let _ = app.update(Message::DetailBack);
        assert_eq!(app.route, Route::Home);
    }

    #[test]
    fn detail_to_uploader_page_round_trip_collections_handoff_and_search_work() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        static UP_LAUNCHES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        UP_LAUNCHES.lock().unwrap().clear();
        app.launch_browser = |url| {
            wallora_windows::validate_wallhaven_handoff_url(url)?;
            UP_LAUNCHES.lock().unwrap().push(url.to_string());
            Ok(())
        };

        assert!(app.ensure_route_loaded(Route::Home).units() > 0);
        let _ = deliver_page(
            &mut app,
            GalleryKind::Home,
            1,
            Ok(sample_page(&["h2"], 1, 1, None)),
        );
        let _ = app.update(Message::GallerySelectTile {
            kind: GalleryKind::Home,
            index: 0,
        });
        let op_id = match &app.detail.as_ref().unwrap().metadata {
            DetailMetadataState::Loading { op_id, .. } => *op_id,
            _ => panic!("expected detail metadata loading"),
        };
        let scope = app.session.scope();
        let _ = app.update(Message::DetailMetadataLoaded {
            op_id,
            scope,
            id: WallpaperId::new("h2"),
            result: Box::new(Ok(sample_detail_wallpaper("h2", Purity::Sfw))),
        });
        let _ = app.view();

        // Detail tagging/moderation hand-off opens https://wallhaven.cc/w/h2.
        let _ = app.update(Message::DetailOpenTagModerationHandOff);

        // Invalid uploader username warns without leaving Detail.
        assert_eq!(
            app.update(Message::DetailSelectUploader("../bad".to_string()))
                .units(),
            0
        );
        assert_eq!(app.route, Route::Detail(WallpaperId::new("h2")));
        assert!(app.session.notice().is_some());

        // Clicking valid uploader opens Route::Uploader("artist_one") with q=@artist_one.
        assert!(
            app.update(Message::DetailSelectUploader("artist_one".to_string()))
                .units()
                > 0
        );
        assert_eq!(app.route, Route::Uploader("artist_one".to_string()));
        assert_eq!(app.active_gallery_kind(), Some(GalleryKind::Uploader));
        assert_eq!(
            app.uploader
                .controller
                .query_for_page(1)
                .unwrap()
                .q
                .as_deref(),
            Some("@artist_one")
        );

        let _ = deliver_uploader_page(
            &mut app,
            1,
            Ok(sample_page(&["u1", "u2", "u3", "u4"], 1, 2, None)),
        );
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Uploader,
            "u1",
            Ok(sample_loaded_thumb(false)),
        );
        let _ = app.view();

        let content_h = app.uploader.metrics.total_content_height(4);
        assert!(
            app.update(Message::GalleryScrolled {
                kind: GalleryKind::Uploader,
                offset_y: 260.0,
                viewport_width: 1200.0,
                viewport_height: 580.0,
                content_height: content_h,
            })
            .units()
                > 0
        );
        let _ = deliver_uploader_page(&mut app, 2, Ok(sample_page(&["u5"], 2, 2, None)));
        assert_eq!(app.uploader.controller.items().len(), 5);

        // Click tile on Uploader -> Detail -> Back returns to Uploader preserving scroll & items.
        app.uploader.metrics.scroll_y = 260.0;
        assert!(
            app.update(Message::GallerySelectTile {
                kind: GalleryKind::Uploader,
                index: 1,
            })
            .units()
                > 0
        );
        assert_eq!(app.route, Route::Detail(WallpaperId::new("u2")));
        let _ = app.view();

        assert!(app.update(Message::DetailBack).units() > 0);
        assert_eq!(app.route, Route::Uploader("artist_one".to_string()));
        assert_eq!(app.uploader.controller.items().len(), 5);
        assert_eq!(app.uploader.metrics.scroll_y, 260.0);
        assert_eq!(app.uploader.selected, Some(1));

        // Purity/sorting/order controls + Profile hand-off.
        assert!(
            app.update(Message::UploaderTogglePurity(Purity::Sketchy))
                .units()
                > 0
        );
        assert!(app.uploader.controller.purity().sketchy);
        assert!(
            app.update(Message::UploaderSelectSorting(Sorting::Favorites))
                .units()
                > 0
        );
        assert_eq!(app.uploader.controller.sorting(), Sorting::Favorites);
        let _ = app.update(Message::UploaderOpenProfileHandOff);

        assert_eq!(
            *UP_LAUNCHES.lock().unwrap(),
            vec![
                "https://wallhaven.cc/w/h2".to_string(),
                "https://wallhaven.cc/user/artist_one".to_string(),
            ]
        );

        // UploaderBack returns to Detail(h2), then re-enter Uploader and jump to public collections & search.
        assert!(app.update(Message::UploaderBack).units() > 0);
        assert_eq!(app.route, Route::Detail(WallpaperId::new("h2")));
        assert!(
            app.update(Message::DetailSelectUploader("artist_one".to_string()))
                .units()
                > 0
        );
        assert!(app.update(Message::UploaderViewPublicCollections).units() > 0);
        assert_eq!(app.route, Route::Collections);
        assert_eq!(app.collections_username_input, "artist_one");
        assert_eq!(
            app.collections.controller.user_collections_status(),
            &CollectionListStatus::Loading
        );
        assert!(app.collections.list_in_flight.is_some());

        // Jump from Uploader -> Search @artist_one.
        assert!(
            app.update(Message::DetailSelectUploader("artist_one".to_string()))
                .units()
                > 0
        );
        assert!(app.update(Message::UploaderSearchUploads).units() > 0);
        assert_eq!(app.route, Route::Search);
        assert_eq!(app.search_query_input, "@artist_one");
        assert_eq!(
            app.search.controller.query_for_page(1).q.as_deref(),
            Some("@artist_one")
        );
    }

    #[test]
    fn tag_and_uploader_scope_transitions_cancel_tasks_and_clear_textures() {
        let paths = TestPaths::new();
        let mut app = test_app(&paths);
        sign_in(&mut app, "alice");
        let alice_id = app.session.active_account().unwrap().id.clone();
        sign_in(&mut app, "bob");

        // Open Tag(10), populate metadata + loaded texture + in-flight thumb + in-flight page 2 + metadata retry.
        assert!(
            app.update(Message::DetailSelectTag {
                id: 10,
                name: "scenery".to_string(),
            })
            .units()
                > 0
        );
        let _ = deliver_tag_metadata(&mut app, 10, Err(ProviderError::Network));
        assert!(app.update(Message::TagRetryMetadata).units() > 0);
        let tag_meta_handle = app.tag.metadata_in_flight.as_ref().unwrap().handle.clone();

        let _ = deliver_tag_page(&mut app, 1, Ok(sample_page(&["tg1", "tg2"], 1, 2, None)));
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Tag,
            "tg1",
            Ok(sample_loaded_thumb(false)),
        );
        let (tg2_cancel, tg2_handle) = match app.tag.thumbnails.get(&WallpaperId::new("tg2")) {
            Some(ThumbnailEntry::InFlight { cancel, handle, .. }) => {
                (cancel.clone(), handle.clone())
            }
            _ => panic!("tg2 should be in flight"),
        };
        let next_tag_req = app.tag.controller.request_next_page().unwrap();
        let _ = app.spawn_tag_page_request(next_tag_req);
        let tag_page_handle = app.tag.page_in_flight.as_ref().unwrap().handle.clone();

        // Also populate Uploader("curator") in-flight work & texture before returning to Tag(10).
        assert!(
            app.update(Message::DetailSelectUploader("curator".to_string()))
                .units()
                > 0
        );
        let _ = deliver_uploader_page(&mut app, 1, Ok(sample_page(&["up1", "up2"], 1, 2, None)));
        let _ = deliver_thumb(
            &mut app,
            GalleryKind::Uploader,
            "up1",
            Ok(sample_loaded_thumb(false)),
        );
        let (up2_cancel, up2_handle) = match app.uploader.thumbnails.get(&WallpaperId::new("up2")) {
            Some(ThumbnailEntry::InFlight { cancel, handle, .. }) => {
                (cancel.clone(), handle.clone())
            }
            _ => panic!("up2 should be in flight"),
        };
        let next_up_req = app.uploader.controller.request_next_page().unwrap();
        let _ = app.spawn_uploader_page_request(next_up_req);
        let up_page_handle = app.uploader.page_in_flight.as_ref().unwrap().handle.clone();

        // SwitchAccount while on Uploader cancels both Tag and Uploader tasks, clears textures, and reloads Uploader.
        assert!(app.update(Message::SwitchAccount(alice_id)).units() > 0);
        assert!(tag_meta_handle.is_aborted());
        assert!(tag_page_handle.is_aborted());
        assert!(tg2_cancel.is_cancelled());
        assert!(tg2_handle.is_aborted());
        assert!(app.tag.thumbnails.is_empty());
        assert!(app.tag.controller.items().is_empty());
        assert_eq!(
            app.tag.controller.metadata_status(),
            &TagMetadataStatus::Idle
        );

        assert!(up_page_handle.is_aborted());
        assert!(up2_cancel.is_cancelled());
        assert!(up2_handle.is_aborted());
        assert!(app.uploader.thumbnails.is_empty());
        assert_eq!(
            app.uploader.page_in_flight.as_ref().map(|p| &p.scope),
            Some(&app.session.scope())
        );

        // 401 revocation on UploaderPageLoaded transitions to Guest and reloads under Guest.
        let _ = deliver_uploader_page(&mut app, 1, Err(ProviderError::Unauthorized));
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert_eq!(
            app.uploader.page_in_flight.as_ref().map(|p| &p.scope.kind),
            Some(&SessionKind::Guest)
        );

        // Switch to Tag, SignIn, 401 revocation on TagMetadataLoaded, SignOut.
        assert!(
            app.update(Message::DetailSelectTag {
                id: 10,
                name: "scenery".to_string(),
            })
            .units()
                > 0
        );
        sign_in(&mut app, "carol");
        assert_eq!(app.route, Route::Tag(10));
        assert!(app.tag.metadata_in_flight.is_some());
        assert!(app.tag.page_in_flight.is_some());
        let _ = deliver_tag_metadata(&mut app, 10, Err(ProviderError::Unauthorized));
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert_eq!(
            app.tag.metadata_in_flight.as_ref().map(|p| &p.scope.kind),
            Some(&SessionKind::Guest)
        );
        assert_eq!(
            app.tag.page_in_flight.as_ref().map(|p| &p.scope.kind),
            Some(&SessionKind::Guest)
        );

        // SwitchToGuest after SignIn aborts tag requests and reloads as Guest.
        sign_in(&mut app, "dave");
        let dave_meta = app.tag.metadata_in_flight.as_ref().unwrap().handle.clone();
        let dave_page = app.tag.page_in_flight.as_ref().unwrap().handle.clone();
        let _ = app.update(Message::SwitchToGuest);
        assert!(dave_meta.is_aborted());
        assert!(dave_page.is_aborted());
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
    }
}
