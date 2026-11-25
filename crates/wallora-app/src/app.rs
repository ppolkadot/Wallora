use std::sync::Arc;

use iced::task::Handle;
use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Background, Border, Element, Length, Task, Theme};
use wallora_core::domain::{
    CacheCapError, CredentialStore, DEFAULT_CACHE_CAP_BYTES, HARD_MAX_CACHE_CAP_BYTES, ProxyConfig,
    SecretString, SessionScope, WALLHAVEN_SETTINGS_URL,
};
use wallora_core::provider::{WallhavenClient, WallhavenClientConfig};
use wallora_core::services::cache::{
    CacheConfig, CacheError, CacheStats, CacheStore, NoopSessionCacheSink, SessionCacheSink,
};
use wallora_core::services::{
    ImageGuard, NoticeLevel, SessionManager, SessionNotice, SignInCommitOutcome, SignInError,
    fetch_account_refresh, probe_sign_in,
};
use wallora_core::storage::{AppPaths, SessionStorage, SqliteStore, StorageError};
use wallora_windows::WindowsCredentialStore;

use crate::cache_host::CacheHost;
use crate::message::{Message, ProxyModeChoice, Route};
use crate::theme::{palette, surface_color, wallora_theme};
use crate::views;
use crate::views::settings::{
    describe_cache_cap_error, format_cap_gb, format_size, parse_cache_cap_gb,
};

struct InFlight {
    id: u64,
    scope: SessionScope,
    handle: Handle,
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
    cache_cap_input: String,
    cache_cap_feedback: Option<(bool, String)>,
    cache_cap_in_flight: Option<u64>,
    cache_stats: Option<CacheStats>,
    launch_browser: fn(&str) -> Result<(), String>,
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
            image_guard: ImageGuard::new(),
            cache_cap_input,
            cache_cap_feedback: None,
            cache_cap_in_flight: None,
            cache_stats: None,
            launch_browser: wallora_windows::open_wallhaven_in_browser,
        };

        let startup_task = app.spawn_active_account_refresh(true);
        (app, startup_task)
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

    fn retire_changed_scope(&mut self, previous: &SessionScope) -> bool {
        if self.session.scope() == *previous {
            return false;
        }
        if let Some(op) = self.refresh_in_flight.take() {
            op.handle.abort();
        }
        self.cancel_sign_in();
        true
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
                    self.route = route;
                    if route == Route::Settings {
                        return self.request_cache_stats();
                    }
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
                            self.retire_changed_scope(&previous);
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
                    self.session.apply_refresh(payload);
                    self.retire_changed_scope(&previous);
                }
                Task::none()
            }
            Message::SignOut => {
                self.cancel_sign_in();
                let previous = self.session.scope();
                self.session.logout();
                self.retire_changed_scope(&previous);
                Task::none()
            }
            Message::SwitchAccount(target_id) => {
                let previous = self.session.scope();
                match self.session.switch_account(&target_id) {
                    Ok(_) if self.retire_changed_scope(&previous) => {
                        self.spawn_active_account_refresh(true)
                    }
                    Ok(_) => Task::none(),
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
                        self.retire_changed_scope(&previous);
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
                if let Err(err) = (self.launch_browser)(WALLHAVEN_SETTINGS_URL) {
                    self.session.set_notice(Some(SessionNotice::warning(format!(
                        "Could not open wallhaven.cc in browser: {err}"
                    ))));
                }
                Task::none()
            }
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

        let defaults = self.session.browse_defaults();
        let body = match self.route {
            Route::Home => views::home::view(),
            Route::Search => views::search::view(&defaults),
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
        AccountId, CollectionSummary, InMemoryCredentialStore, LocalPreferences, SessionKind,
        UserSettings, UsernameVerification,
    };
    use wallora_core::provider::ProviderError;
    use wallora_core::services::{AccountRefreshPayload, SignInProbe};

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
}
