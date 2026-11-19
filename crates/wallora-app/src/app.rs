use std::sync::Arc;

use iced::task::Handle;
use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Background, Border, Element, Length, Task, Theme};
use wallora_core::domain::{
    CredentialStore, ProxyConfig, SecretString, SessionScope, WALLHAVEN_SETTINGS_URL,
};
use wallora_core::provider::{WallhavenClient, WallhavenClientConfig};
use wallora_core::services::{
    NoticeLevel, SessionManager, SessionNotice, SignInCommitOutcome, SignInError,
    fetch_account_refresh, probe_sign_in,
};
use wallora_core::storage::{AppPaths, SessionStorage, SqliteStore, StorageError};
use wallora_windows::WindowsCredentialStore;

use crate::message::{Message, ProxyModeChoice, Route};
use crate::theme::{palette, surface_color, wallora_theme};
use crate::views;

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
    launch_browser: fn(&str) -> Result<(), String>,
}

impl WalloraApp {
    pub fn boot(paths: AppPaths) -> (Self, Task<Message>) {
        let mut startup_warning = None;
        let storage: Arc<dyn SessionStorage> = match SqliteStore::open(&paths.db_path()) {
            Ok(db) => Arc::new(db),
            Err(err) => {
                tracing::error!(reason = %err, "failed to open SQLite database; falling back to in-memory storage");
                startup_warning = Some(SessionNotice::warning(match err {
                    StorageError::SessionOwned => {
                        "Another Wallora window owns local account storage. Close it and restart this window to sign in."
                    }
                    _ => {
                        "Could not open local SQLite storage. Sign-in is unavailable in this window."
                    }
                }));
                Arc::new(
                    SqliteStore::open_in_memory()
                        .expect("in-memory SQLite fallback must open cleanly"),
                )
            }
        };

        let credentials: Arc<dyn CredentialStore> = Arc::new(WindowsCredentialStore::new());
        let mut session = SessionManager::restore(storage, Arc::clone(&credentials));
        if session.notice().is_none()
            && let Some(warning) = startup_warning
        {
            session.set_notice(Some(warning));
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

        Self::from_runtime(paths, session, client)
    }

    /// Constructs the app with injected session/client components (used by boot and unit tests).
    pub fn from_runtime(
        paths: AppPaths,
        session: SessionManager,
        client: Arc<WallhavenClient>,
    ) -> (Self, Task<Message>) {
        let (proxy_mode, proxy_url_input) = match &session.local_preferences().proxy {
            ProxyConfig::System => (ProxyModeChoice::System, String::new()),
            ProxyConfig::Direct => (ProxyModeChoice::Direct, String::new()),
            ProxyConfig::Manual(url) => (ProxyModeChoice::Manual, url.clone()),
        };

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
    use wallora_core::domain::{
        AccountId, CollectionSummary, InMemoryCredentialStore, SessionKind, UserSettings,
        UsernameVerification,
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
            for filename in [
                "wallora.db",
                "wallora.db-wal",
                "wallora.db-shm",
                "wallora.session.lock",
            ] {
                let _ = std::fs::remove_file(self.0.data_dir.join(filename));
            }
            let _ = std::fs::remove_dir(&self.0.data_dir);
            let _ = std::fs::remove_dir(&self.0.local_dir);
        }
    }

    fn test_app(paths: &TestPaths) -> WalloraApp {
        let storage = Arc::new(SqliteStore::open(&paths.0.db_path()).expect("file-backed sqlite"));
        let creds = Arc::new(InMemoryCredentialStore::new());
        let session = SessionManager::restore(storage, creds.clone());
        let client = Arc::new(WallhavenClient::new(creds).expect("client"));
        let (app, _) = WalloraApp::from_runtime(paths.0.clone(), session, client);
        app
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
        let (mut app, startup) = WalloraApp::boot(paths.0.clone());
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
}
