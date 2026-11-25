use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use iced::task::Handle;
use iced::widget::operation::{self, AbsoluteOffset};
use iced::widget::{Id, button, column, container, image as iced_image, row, text};
use iced::{Alignment, Background, Border, Element, Length, Size, Subscription, Task, Theme};
use wallora_core::domain::{
    BrowseDefaults, CacheCapError, Category, CredentialStore, DEFAULT_CACHE_CAP_BYTES,
    HARD_MAX_CACHE_CAP_BYTES, ProxyConfig, Purity, SearchTerms, SecretString, SessionKind,
    SessionScope, Sorting, TopRange, WALLHAVEN_SETTINGS_URL, WallpaperId,
};
use wallora_core::provider::{WallhavenClient, WallhavenClientConfig};
use wallora_core::services::cache::{
    CacheConfig, CacheError, CacheStats, CacheStore, CancelToken, NoopSessionCacheSink,
    SessionCacheSink,
};
use wallora_core::services::{
    ApplyOutcome, DiscoveryController, DiscoveryFilters, DiscoveryStatus, DiscoveryView,
    ImageGuard, ImagePipeline, ImagePipelineError, ImageRequest, NoticeLevel, PageRequest,
    SessionManager, SessionNotice, SignInCommitOutcome, SignInError, fetch_account_refresh,
    fetch_page, load_uncached, probe_sign_in, selectable_purity,
};
use wallora_core::storage::{AppPaths, SessionStorage, SqliteStore, StorageError};
use wallora_windows::WindowsCredentialStore;

use crate::cache_host::CacheHost;
use crate::message::{GalleryKind, Message, ProxyModeChoice, Route};
use crate::theme::{palette, surface_color, wallora_theme};
use crate::views;
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
        match self.thumbnails.get(id) {
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

    fn cancel_in_flight_thumbnails(&mut self) {
        self.thumbnails.retain(|_, entry| match entry {
            ThumbnailEntry::InFlight { cancel, handle, .. } => {
                cancel.cancel();
                handle.abort();
                false
            }
            ThumbnailEntry::Loaded { .. } | ThumbnailEntry::Failed { .. } => true,
        });
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

    fn evict_excess_loaded(&mut self, visible_range: &std::ops::Range<usize>) {
        let mut loaded_count = self
            .thumbnails
            .values()
            .filter(|e| matches!(e, ThumbnailEntry::Loaded { .. }))
            .count();
        if loaded_count <= MAX_RESIDENT_THUMBNAILS {
            return;
        }

        let items = self.controller.items();
        let mid = (visible_range.start + visible_range.end) / 2;
        let mut candidates: Vec<(usize, WallpaperId)> = items
            .iter()
            .enumerate()
            .filter(|(idx, w)| {
                !visible_range.contains(idx)
                    && matches!(
                        self.thumbnails.get(&w.id),
                        Some(ThumbnailEntry::Loaded { .. })
                    )
            })
            .map(|(idx, w)| (idx.abs_diff(mid), w.id.clone()))
            .collect();
        candidates.sort_by_key(|a| std::cmp::Reverse(a.0));

        for (_, id) in candidates {
            if loaded_count <= MAX_RESIDENT_THUMBNAILS {
                break;
            }
            if matches!(
                self.thumbnails.get(&id),
                Some(ThumbnailEntry::Loaded { .. })
            ) {
                self.thumbnails.remove(&id);
                loaded_count -= 1;
            }
        }
    }

    fn sync_visible_thumbnails(
        &mut self,
        loader: &ThumbnailLoader,
        thumb_size: &str,
    ) -> Task<Message> {
        let items_len = self.controller.items().len();
        if items_len == 0 {
            self.cancel_in_flight_thumbnails();
            return Task::none();
        }

        let visible_range = self.metrics.visible_item_range(items_len);
        let generation = self.controller.generation();
        let scope = self.controller.scope().clone();

        let visible_ids: HashSet<WallpaperId> = self.controller.items()[visible_range.clone()]
            .iter()
            .map(|w| w.id.clone())
            .collect();

        // Cancel and remove any in-flight thumbnails outside the visible + overscan window or
        // belonging to an older scope/generation.
        self.thumbnails.retain(|id, entry| match entry {
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

        self.evict_excess_loaded(&visible_range);

        let mut tasks = Vec::new();
        for wallpaper in &self.controller.items()[visible_range] {
            if self.thumbnails.contains_key(&wallpaper.id) {
                continue;
            }
            let raw_url = select_thumbnail_url(&wallpaper.thumbs, thumb_size);
            let request = match ImageRequest::thumbnail_url(raw_url, scope.clone()) {
                Ok(req) => req,
                Err(err) => {
                    self.thumbnails.insert(
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
            let kind = self.kind;
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

            self.thumbnails.insert(
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

pub struct WalloraApp {
    route: Route,
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
        let search_resolutions_input = format_resolutions(&defaults.resolutions);
        let search_ratios_input = format_ratios(&defaults.aspect_ratios);

        let mut app = Self {
            route: Route::Home,
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
        if matches!(self.route, Route::Home | Route::Search) {
            Subscription::batch([resize, iced::event::listen_with(gallery_keyboard_event)])
        } else {
            resize
        }
    }

    fn gallery(&self, kind: GalleryKind) -> &GalleryState {
        match kind {
            GalleryKind::Home => &self.home,
            GalleryKind::Search => &self.search,
        }
    }

    fn gallery_mut(&mut self, kind: GalleryKind) -> &mut GalleryState {
        match kind {
            GalleryKind::Home => &mut self.home,
            GalleryKind::Search => &mut self.search,
        }
    }

    fn active_gallery_kind(&self) -> Option<GalleryKind> {
        match self.route {
            Route::Home => Some(GalleryKind::Home),
            Route::Search => Some(GalleryKind::Search),
            Route::Account | Route::Settings => None,
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
        self.gallery_mut(kind)
            .sync_visible_thumbnails(&loader, &thumb_size)
    }

    fn ensure_route_loaded(&mut self, route: Route) -> Task<Message> {
        let kind = match route {
            Route::Home => GalleryKind::Home,
            Route::Search => GalleryKind::Search,
            Route::Account | Route::Settings => return Task::none(),
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

        Some(self.ensure_route_loaded(self.route))
    }

    fn apply_updated_browse_defaults(&mut self, defaults: &BrowseDefaults) -> Task<Message> {
        let scope = self.session.scope();
        self.home_toplist_range = defaults.toplist_range;
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
                    let previous_route = self.route;
                    self.route = route;
                    match previous_route {
                        Route::Home => self.home.cancel_in_flight_thumbnails(),
                        Route::Search => self.search.cancel_in_flight_thumbnails(),
                        Route::Account | Route::Settings => {}
                    }
                    return match route {
                        Route::Home | Route::Search => self.ensure_route_loaded(route),
                        Route::Settings => self.request_cache_stats(),
                        Route::Account => Task::none(),
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
                match purity {
                    Purity::Sfw => filters.purity.sfw = !filters.purity.sfw,
                    Purity::Sketchy => filters.purity.sketchy = !filters.purity.sketchy,
                    Purity::Nsfw => filters.purity.nsfw = !filters.purity.nsfw,
                }
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
            Message::GalleryRefresh(kind) => {
                let req = self.gallery_mut(kind).controller.refresh();
                self.reload_gallery(kind, req)
            }
            Message::GalleryRetry(kind) => {
                let Some(req) = self.gallery_mut(kind).controller.retry() else {
                    return Task::none();
                };
                self.spawn_page_request(kind, req)
            }
            Message::GallerySelectTile { kind, index } => {
                let len = self.gallery(kind).controller.items().len();
                if index < len {
                    self.gallery_mut(kind).selected = Some(index);
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
                let (near_bottom, next_req) = {
                    let gallery = self.gallery_mut(kind);
                    gallery
                        .metrics
                        .update_scroll(offset_y, viewport_width, viewport_height);
                    let len = gallery.controller.items().len();
                    let near_bottom = gallery.metrics.is_near_bottom(len, Some(content_height));
                    let next_req = if near_bottom {
                        gallery.controller.request_next_page()
                    } else {
                        None
                    };
                    (near_bottom, next_req)
                };
                let _ = near_bottom;
                let thumb_task = self.sync_gallery_thumbnails(kind);
                match next_req {
                    Some(req) => {
                        let page_task = self.spawn_page_request(kind, req);
                        Task::batch([thumb_task, page_task])
                    }
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
                if scope != self.session.scope() {
                    return Task::none();
                }
                let gallery = self.gallery_mut(kind);
                if generation != gallery.controller.generation() {
                    return Task::none();
                }
                let matches_in_flight = matches!(
                    gallery.thumbnails.get(&id),
                    Some(ThumbnailEntry::InFlight {
                        generation: g,
                        scope: s,
                        ..
                    }) if *g == generation && *s == scope
                );
                if !matches_in_flight {
                    return Task::none();
                }
                match result {
                    Ok(loaded) => {
                        gallery.thumbnails.insert(
                            id,
                            ThumbnailEntry::Loaded {
                                handle: loaded.handle,
                                stale: loaded.stale,
                            },
                        );
                    }
                    Err(ImagePipelineError::Cancelled | ImagePipelineError::ScopeChanged) => {
                        gallery.thumbnails.remove(&id);
                    }
                    Err(err) => {
                        gallery.thumbnails.insert(
                            id,
                            ThumbnailEntry::Failed {
                                message: thumbnail_error_message(&err),
                            },
                        );
                    }
                }
                Task::none()
            }
            Message::RetryThumbnail { kind, id } => {
                let should_retry = matches!(
                    self.gallery(kind).thumbnails.get(&id),
                    Some(ThumbnailEntry::Failed { .. })
                );
                if should_retry {
                    self.gallery_mut(kind).thumbnails.remove(&id);
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
                Route::Account | Route::Settings => Task::none(),
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
        match self.active_gallery_kind() {
            Some(kind) => self.sync_gallery_thumbnails(kind),
            None => Task::none(),
        }
    }

    fn apply_gallery_key(&mut self, kind: GalleryKind, key: GridKey) -> Task<Message> {
        let len = self.gallery(kind).controller.items().len();
        let next = self
            .gallery(kind)
            .metrics
            .move_selection(self.gallery(kind).selected, len, key);
        let Some(idx) = next else {
            return Task::none();
        };
        let prev = self.gallery(kind).selected;
        self.gallery_mut(kind).selected = Some(idx);
        if prev == Some(idx) {
            return Task::none();
        }

        let mut tasks = Vec::new();
        if let Some(target_y) = self.gallery(kind).metrics.scroll_target_for(idx) {
            self.gallery_mut(kind).metrics.scroll_y = target_y;
            tasks.push(operation::scroll_to(
                self.gallery(kind).scrollable_id.clone(),
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

        let near_bottom = self.gallery(kind).metrics.is_near_bottom(len, None);
        if near_bottom && let Some(req) = self.gallery_mut(kind).controller.request_next_page() {
            tasks.push(self.spawn_page_request(kind, req));
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

    pub fn view(&self) -> Element<'_, Message> {
        let nav_buttons = [Route::Home, Route::Search, Route::Account, Route::Settings]
            .into_iter()
            .fold(row![].spacing(8), |r, route| {
                let active = self.route == route;
                let btn = button(text(route.label()).size(14))
                    .padding([8, 16])
                    .on_press(Message::Navigate(route))
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
        let body = match self.route {
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
        AccountId, AspectRatio, CategoryMask, CollectionSummary, Dimensions,
        InMemoryCredentialStore, LocalPreferences, PurityMask, SearchMeta, SearchPage, SessionKind,
        SortOrder, Thumbs, UserSettings, UsernameVerification, Wallpaper,
    };
    use wallora_core::provider::{ImageClass, ProviderError};
    use wallora_core::services::cache::{CacheConnectivity, CacheKey, crc32_ieee};
    use wallora_core::services::{
        AccountRefreshPayload, PageResponse, SignInProbe, UnsupportedFeature,
    };

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
        let generation = app.gallery(kind).controller.generation();
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
}
