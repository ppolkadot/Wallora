use std::sync::Arc;

use iced::widget::{button, column, container, row, text};
use iced::{Alignment, Background, Border, Element, Length, Task, Theme};
use wallora_core::domain::{CredentialStore, ProxyConfig, SecretString, WALLHAVEN_SETTINGS_URL};
use wallora_core::provider::{WallhavenClient, WallhavenClientConfig};
use wallora_core::services::{
    RefreshOutcome, SessionManager, SessionNotice, SignInCommitOutcome, fetch_account_refresh,
    probe_sign_in,
};
use wallora_core::storage::{AppPaths, SessionStorage, SqliteStore};
use wallora_windows::WindowsCredentialStore;

use crate::message::{Message, ProxyModeChoice, Route};
use crate::theme::{palette, surface_color, wallora_theme};
use crate::views;

pub struct WalloraApp {
    route: Route,
    paths: AppPaths,
    client: Arc<WallhavenClient>,
    session: SessionManager,
    username_input: String,
    api_key_input: SecretString,
    next_op_id: u64,
    sign_in_in_flight: Option<u64>,
    refresh_in_flight: bool,
    proxy_mode: ProxyModeChoice,
    proxy_url_input: String,
    proxy_feedback: Option<(bool, String)>,
}

impl WalloraApp {
    pub fn boot(paths: AppPaths) -> (Self, Task<Message>) {
        let mut startup_warning = None;
        let storage: Arc<dyn SessionStorage> = match SqliteStore::open(&paths.db_path()) {
            Ok(db) => Arc::new(db),
            Err(err) => {
                tracing::error!(reason = %err, "failed to open SQLite database; falling back to in-memory storage");
                startup_warning = Some(SessionNotice::warning(
                    "Could not open local SQLite storage; running with temporary in-memory session state.",
                ));
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
            refresh_in_flight: false,
            proxy_mode,
            proxy_url_input,
            proxy_feedback: None,
        };

        let startup_task = app.spawn_active_account_refresh(false);
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

    fn spawn_active_account_refresh(&mut self, reverify: bool) -> Task<Message> {
        let Some(account) = self.session.active_account() else {
            return Task::none();
        };
        let scope = self.session.scope();
        let username = account.username.clone();
        let client = Arc::clone(&self.client);
        self.refresh_in_flight = true;

        Task::perform(
            async move { fetch_account_refresh(client.as_ref(), scope, username, reverify).await },
            Message::AccountRefreshFinished,
        )
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
                let op_id = self.next_op_id;
                self.next_op_id = self.next_op_id.saturating_add(1);
                self.sign_in_in_flight = Some(op_id);

                let username = self.username_input.clone();
                let api_key = self.api_key_input.clone();
                let start_scope = self.session.scope();
                let client = Arc::clone(&self.client);

                Task::perform(
                    async move {
                        let result =
                            probe_sign_in(client.as_ref(), username, api_key, start_scope).await;
                        (op_id, result)
                    },
                    |(op_id, result)| Message::SignInProbeFinished { op_id, result },
                )
            }
            Message::SignInProbeFinished { op_id, result } => {
                if self.sign_in_in_flight != Some(op_id) {
                    return Task::none();
                }
                self.sign_in_in_flight = None;
                // Clear typed secret input once the attempt settles.
                self.api_key_input = SecretString::new("");

                match result {
                    Ok(probe) => match self.session.commit_sign_in(probe) {
                        Ok(SignInCommitOutcome::Applied { .. }) => {
                            self.username_input.clear();
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
            Message::AccountRefreshFinished(payload) => {
                let outcome = self.session.apply_refresh(payload);
                match outcome {
                    RefreshOutcome::Applied { .. } | RefreshOutcome::KeyRevoked(_) => {
                        self.refresh_in_flight = false;
                    }
                    RefreshOutcome::DiscardedStale => {}
                }
                Task::none()
            }
            Message::SignOut => {
                self.sign_in_in_flight = None;
                self.refresh_in_flight = false;
                self.api_key_input = SecretString::new("");
                let _ = self.session.logout();
                Task::none()
            }
            Message::SwitchAccount(target_id) => {
                self.sign_in_in_flight = None;
                self.refresh_in_flight = false;
                self.api_key_input = SecretString::new("");
                match self.session.switch_account(&target_id) {
                    Ok(_) => self.spawn_active_account_refresh(false),
                    Err(err) => {
                        self.session
                            .set_notice(Some(SessionNotice::error(err.to_string())));
                        Task::none()
                    }
                }
            }
            Message::SwitchToGuest => {
                self.sign_in_in_flight = None;
                self.refresh_in_flight = false;
                self.api_key_input = SecretString::new("");
                if let Err(err) = self.session.switch_to_guest() {
                    tracing::error!(reason = %err, "failed to switch to guest");
                    self.session.set_notice(Some(SessionNotice::error(
                        "Failed to update active session in local storage.",
                    )));
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
                if let Err(err) = wallora_windows::open_wallhaven_in_browser(WALLHAVEN_SETTINGS_URL)
                {
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
                refresh_busy: self.refresh_in_flight,
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

        column![header, content]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use wallora_core::domain::{
        CollectionSummary, InMemoryCredentialStore, SessionKind, SessionScope, UserSettings,
        UsernameVerification,
    };
    use wallora_core::provider::ProviderError;
    use wallora_core::services::{AccountRefreshPayload, SignInProbe};

    fn test_paths() -> AppPaths {
        AppPaths::from_roots(
            PathBuf::from("C:/tmp/wallora-test/config"),
            PathBuf::from("C:/tmp/wallora-test/local"),
        )
    }

    fn test_app() -> WalloraApp {
        let storage = Arc::new(SqliteStore::open_in_memory().expect("in-memory sqlite"));
        let creds = Arc::new(InMemoryCredentialStore::new());
        let session =
            SessionManager::restore(storage, Arc::clone(&creds) as Arc<dyn CredentialStore>);
        let client = Arc::new(WallhavenClient::new(creds).expect("client"));
        let (app, _) = WalloraApp::from_runtime(test_paths(), session, client);
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
    fn navigation_switches_routes_including_account() {
        let mut app = test_app();
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

    #[test]
    fn sign_in_commit_revocation_and_proxy_update_keep_single_client() {
        let mut app = test_app();
        let initial_client_ptr = Arc::as_ptr(&app.client);

        let _ = app.update(Message::ProxyModeSelected(ProxyModeChoice::Manual));
        let _ = app.update(Message::ProxyUrlChanged(
            "http://127.0.0.1:8888".to_string(),
        ));
        let _ = app.update(Message::ApplyProxy);
        assert_eq!(
            Arc::as_ptr(&app.client),
            initial_client_ptr,
            "applying a manual proxy must reuse the single WallhavenClient/ApiPacer"
        );
        assert_eq!(
            app.client.proxy(),
            ProxyConfig::Manual("http://127.0.0.1:8888/".to_string())
        );

        let start_scope = app.session.scope();
        app.sign_in_in_flight = Some(1);
        let probe = SignInProbe {
            start_scope: start_scope.clone(),
            username: "alice".to_string(),
            api_key: SecretString::new("wh_synthetic_key_1"),
            user_settings: sample_settings(),
            own_collections: Some(vec![CollectionSummary {
                id: 1,
                label: "Default".to_string(),
                views: 0,
                public: true,
                count: 2,
            }]),
            verification: UsernameVerification::Verified,
        };
        let _ = app.update(Message::SignInProbeFinished {
            op_id: 1,
            result: Ok(probe),
        });

        let signed_in_scope = app.session.scope();
        assert!(matches!(signed_in_scope.kind, SessionKind::Account(_)));
        assert_eq!(signed_in_scope.generation, start_scope.generation + 1);
        assert_eq!(app.session.browse_defaults().per_page, 32);

        // Stale 401 from an older generation must NOT revoke the current session.
        let _ = app.update(Message::AccountRefreshFinished(AccountRefreshPayload {
            request_scope: SessionScope::guest(1),
            user_settings: Err(ProviderError::Unauthorized),
            own_collections: Err(ProviderError::Unauthorized),
            identity_outcome: None,
        }));
        assert_eq!(app.session.scope(), signed_in_scope);

        // Current-generation 401 revokes the session, bumps generation, and preserves local proxy.
        let _ = app.update(Message::AccountRefreshFinished(AccountRefreshPayload {
            request_scope: signed_in_scope.clone(),
            user_settings: Err(ProviderError::Unauthorized),
            own_collections: Err(ProviderError::Unauthorized),
            identity_outcome: None,
        }));
        assert_eq!(app.session.scope().kind, SessionKind::Guest);
        assert_eq!(
            app.session.scope().generation,
            signed_in_scope.generation + 1
        );
        assert_eq!(app.session.browse_defaults().per_page, 24);
        assert_eq!(
            app.session.local_preferences().proxy,
            ProxyConfig::Manual("http://127.0.0.1:8888/".to_string())
        );
    }
}
