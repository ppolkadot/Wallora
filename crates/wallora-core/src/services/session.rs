use std::collections::HashSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;

use crate::domain::{
    AccountId, AccountRecord, BrowseDefaults, CollectionSummary, CredentialStore,
    IdentityCheckOutcome, LocalPreferences, ProxyConfig, ProxyError, SecretString, SessionKind,
    SessionScope, UserSettings, UsernameVerification, validate_username,
};
use crate::provider::{Provider, ProviderError, WallhavenClient};
use crate::storage::{AccountCachedState, SessionStorage, StorageError};

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn require_fresh_verification(mut account: AccountRecord) -> AccountRecord {
    if account.verification.is_verified() {
        account.verification = UsernameVerification::Inconclusive;
    }
    account
}

/// Evaluates username ownership by comparing the keyed `/collections` list against the unkeyed
/// public `/collections/{username}` list.
///
/// Note: A `401 Unauthorized` on the keyed own list must be handled as an invalid/revoked key
/// before calling this function. A generic `404 NotFound` on the public list is treated as
/// `Inconclusive` (never proof of mismatch on its own) because Wallhaven returns 404 for real
/// accounts whose collections are all private.
pub fn evaluate_username_verification(
    own: Result<&[CollectionSummary], &ProviderError>,
    public: Result<&[CollectionSummary], &ProviderError>,
) -> IdentityCheckOutcome {
    let Ok(own_list) = own else {
        return IdentityCheckOutcome::Inconclusive;
    };

    let Ok(public_list) = public else {
        return IdentityCheckOutcome::Inconclusive;
    };

    let own_ids: HashSet<u64> = own_list.iter().map(|c| c.id).collect();
    if public_list.iter().any(|c| !own_ids.contains(&c.id)) {
        return IdentityCheckOutcome::Mismatch;
    }

    if !public_list.is_empty() {
        return IdentityCheckOutcome::Verified;
    }

    if own_list.is_empty() {
        return IdentityCheckOutcome::UnverifiableEmpty;
    }

    let own_has_public = own_list.iter().any(|c| c.public);
    if !own_has_public {
        IdentityCheckOutcome::UnverifiablePrivate
    } else {
        IdentityCheckOutcome::Inconclusive
    }
}

/// Errors returned during sign-in validation or persistence. Never embeds credential secrets.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SignInError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("API key was rejected by Wallhaven (401). Check your key and try again.")]
    InvalidApiKey,
    #[error("username does not match this API key")]
    UsernameMismatch,
    #[error("unable to verify sign-in right now: {0}")]
    TemporaryFailure(String),
    #[error("failed to save API key to Windows Credential Manager")]
    CredentialStore,
    #[error("Sign-in requires an available local SQLite database.")]
    StorageUnavailable,
    #[error("failed to save account metadata to local storage")]
    Storage,
}

/// Validated candidate sign-in payload awaiting synchronous commit under scope/generation guard.
#[derive(Debug, Clone)]
pub struct SignInProbe {
    pub start_scope: SessionScope,
    pub username: String,
    pub api_key: SecretString,
    pub user_settings: Option<UserSettings>,
    pub own_collections: Option<Vec<CollectionSummary>>,
    pub verification: UsernameVerification,
}

/// Async Phase 1 of sign-in: validates the API key via `/settings`, then (only on 200) checks
/// username ownership via keyed `/collections` vs unkeyed `/collections/{username}`.
///
/// Performs no writes to `CredentialStore` or SQLite; commit happens separately after checking
/// that the session scope/generation has not changed while the probe was in flight.
pub async fn probe_sign_in<P: Provider>(
    provider: &P,
    username: String,
    api_key: SecretString,
    start_scope: SessionScope,
) -> Result<SignInProbe, SignInError> {
    let clean_username =
        validate_username(&username).map_err(|msg| SignInError::InvalidInput(msg.to_string()))?;

    let trimmed_key = SecretString::new(api_key.expose_secret().trim());
    if trimmed_key.is_empty() {
        return Err(SignInError::InvalidInput(
            "Enter your Wallhaven API key.".to_string(),
        ));
    }

    // Step 1: Key validity via keyed GET /api/v1/settings.
    let user_settings = match provider.user_settings_with_key(trimmed_key.clone()).await {
        Ok(settings) => settings,
        Err(ProviderError::Unauthorized) => return Err(SignInError::InvalidApiKey),
        Err(ProviderError::InvalidCredential) => {
            return Err(SignInError::InvalidInput("Enter a valid API key.".into()));
        }
        Err(err) => return Err(SignInError::TemporaryFailure(err.user_message())),
    };

    // Step 2: Username verification (only reached for a valid key).
    let own_res = provider.own_collections_with_key(trimmed_key.clone()).await;
    let (own_collections, verification) = match own_res {
        Err(ProviderError::Unauthorized) => return Err(SignInError::InvalidApiKey),
        Err(_) => (None, UsernameVerification::Inconclusive),
        Ok(own) => {
            let public_res = provider
                .public_collections(clean_username.clone(), start_scope.generation)
                .await;
            let outcome = evaluate_username_verification(Ok(own.as_slice()), public_res.as_deref());
            match outcome.into_accepted() {
                Some(verification) => (Some(own), verification),
                None => return Err(SignInError::UsernameMismatch),
            }
        }
    };

    Ok(SignInProbe {
        start_scope,
        username: clean_username,
        api_key: trimmed_key,
        user_settings,
        own_collections,
        verification,
    })
}

/// Result of refreshing an already-signed-in account's upstream settings, own collections, and
/// (optionally) username verification state.
#[derive(Debug, Clone)]
pub struct AccountRefreshPayload {
    pub request_scope: SessionScope,
    pub user_settings: Result<UserSettings, ProviderError>,
    pub own_collections: Result<Vec<CollectionSummary>, ProviderError>,
    pub identity_outcome: Option<IdentityCheckOutcome>,
}

/// Async refresh for an active signed-in account. Always uses the existing `Provider` seam and
/// keeps `/collections/{username}` unkeyed.
pub async fn fetch_account_refresh<P: Provider>(
    provider: &P,
    scope: SessionScope,
    username: String,
    reverify_username: bool,
) -> AccountRefreshPayload {
    let user_settings = provider.user_settings(scope.clone()).await;
    if let Err(error) = &user_settings
        && (*error == ProviderError::Unauthorized || error.is_local_credential_error())
    {
        return AccountRefreshPayload {
            request_scope: scope,
            own_collections: Err(error.clone()),
            user_settings,
            identity_outcome: None,
        };
    }

    let own_collections = provider.own_collections(scope.clone()).await;
    if matches!(own_collections, Err(ProviderError::Unauthorized))
        || own_collections
            .as_ref()
            .is_err_and(ProviderError::is_local_credential_error)
    {
        return AccountRefreshPayload {
            request_scope: scope,
            user_settings,
            own_collections,
            identity_outcome: None,
        };
    }

    let identity_outcome = if reverify_username {
        match &own_collections {
            Ok(own) => {
                let public_res = provider
                    .public_collections(username, scope.generation)
                    .await;
                Some(evaluate_username_verification(
                    Ok(own.as_slice()),
                    public_res.as_deref(),
                ))
            }
            Err(_) => Some(IdentityCheckOutcome::Inconclusive),
        }
    } else {
        None
    };

    AccountRefreshPayload {
        request_scope: scope,
        user_settings,
        own_collections,
        identity_outcome,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionNotice {
    pub level: NoticeLevel,
    pub message: String,
}

impl SessionNotice {
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            level: NoticeLevel::Info,
            message: message.into(),
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: NoticeLevel::Warning,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: NoticeLevel::Error,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInCommitOutcome {
    Applied {
        scope: SessionScope,
        verification: UsernameVerification,
    },
    DiscardedStale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    Applied { warning: Option<SessionNotice> },
    KeyRevoked(SessionNotice),
    DiscardedStale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopedResponse<T> {
    Ready(Result<T, ProviderError>),
    Revoked(SessionNotice),
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogoutOutcome {
    pub scope: SessionScope,
    pub logged_out_account: Option<AccountId>,
    pub warning: Option<SessionNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchOutcome {
    pub scope: SessionScope,
    pub previous_account: Option<AccountId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SwitchError {
    #[error("selected account was not found")]
    AccountNotFound,
    #[error("the stored API key for this account is missing; sign in again")]
    MissingCredential,
    #[error("Windows Credential Manager could not be read; try again")]
    CredentialStoreUnavailable,
    #[error("failed to update the active account in local storage")]
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProxyUpdateError {
    #[error("{0}")]
    Invalid(#[from] ProxyError),
    #[error("failed to save local proxy preference")]
    Storage,
}

/// Authoritative session and preference coordinator shared by the Iced shell and integration tests.
///
/// Tracks `SessionScope` (guest vs active `AccountId` plus monotonic generation), enforces
/// scope/generation checks at async completion, manages SQLite metadata and Task 3 account cache
/// purges, and keeps local proxy preferences completely separate from upstream account settings.
pub struct SessionManager {
    storage: Arc<dyn SessionStorage>,
    credentials: Arc<dyn CredentialStore>,
    scope: SessionScope,
    accounts: Vec<AccountRecord>,
    active_account: Option<AccountRecord>,
    user_settings: Option<UserSettings>,
    own_collections: Option<Vec<CollectionSummary>>,
    local_preferences: LocalPreferences,
    notice: Option<SessionNotice>,
}

impl SessionManager {
    /// Restores saved local preferences and active session from SQLite + `CredentialStore`.
    ///
    /// Reconciles unfinished vault work before loading account metadata. The startup refresh
    /// must obtain current account data before showing it, even if the vault still has a key.
    pub fn restore(
        storage: Arc<dyn SessionStorage>,
        credentials: Arc<dyn CredentialStore>,
    ) -> Self {
        let mut notice = None;

        let local_preferences = match storage.load_local_preferences() {
            Ok(prefs) => prefs,
            Err(_) => {
                notice = Some(SessionNotice::warning(
                    "Could not load saved local proxy preferences; using system proxy defaults.",
                ));
                LocalPreferences::default()
            }
        };

        let mut accounts = match storage.pending_credential_cleanup() {
            Ok(pending) => {
                let mut cleanup_failed = false;
                for id in pending {
                    cleanup_failed |= !Self::clean_account(&*storage, &*credentials, &id);
                }
                if cleanup_failed {
                    notice = Some(Self::pending_cleanup_notice());
                }
                storage.list_accounts()
            }
            Err(err) => Err(err),
        }
        .unwrap_or_else(|_| {
            notice = Some(SessionNotice::warning(
                "Could not read saved accounts from local storage; starting in Guest mode.",
            ));
            Vec::new()
        });

        let mut scope = SessionScope::guest(1);
        let mut active_account = None;
        if let Some(candidate) = accounts.iter().find(|a| a.is_active).cloned() {
            match credentials.load(&candidate.id) {
                Ok(Some(secret)) if !secret.is_empty() => {
                    drop(secret);
                    scope = SessionScope::account(candidate.id.clone(), 1);
                    active_account = Some(require_fresh_verification(candidate));
                }
                Ok(_) => {
                    if let Err(err) = storage.set_active_account(None) {
                        tracing::warn!(%err, "could not purge cache for missing credential");
                    }
                    notice = Some(SessionNotice::warning(
                        "Stored API key is missing from Windows Credential Manager. Sign in again.",
                    ));
                }
                Err(_) => {
                    notice = Some(SessionNotice::warning(
                        "Windows Credential Manager could not be read. Try switching to the saved account again.",
                    ));
                }
            }
        }

        accounts = accounts
            .into_iter()
            .map(require_fresh_verification)
            .collect();
        Self {
            storage,
            credentials,
            scope,
            accounts,
            active_account,
            user_settings: None,
            own_collections: None,
            local_preferences,
            notice,
        }
    }

    pub fn scope(&self) -> SessionScope {
        self.scope.clone()
    }

    pub fn accounts(&self) -> &[AccountRecord] {
        &self.accounts
    }

    pub fn active_account(&self) -> Option<&AccountRecord> {
        self.active_account.as_ref()
    }

    pub fn user_settings(&self) -> Option<&UserSettings> {
        self.user_settings.as_ref()
    }

    pub fn own_collections(&self) -> Option<&[CollectionSummary]> {
        self.own_collections.as_deref()
    }

    pub fn local_preferences(&self) -> &LocalPreferences {
        &self.local_preferences
    }

    pub fn notice(&self) -> Option<&SessionNotice> {
        self.notice.as_ref()
    }

    pub fn set_notice(&mut self, notice: Option<SessionNotice>) {
        self.notice = notice;
    }

    pub fn can_sign_in(&self) -> bool {
        self.storage.is_durable()
    }

    /// Returns the effective browse defaults for the current session: upstream `UserSettings`
    /// when signed in and loaded, or audited Guest defaults otherwise.
    pub fn browse_defaults(&self) -> BrowseDefaults {
        match (&self.scope.kind, &self.user_settings) {
            (SessionKind::Account(_), Some(settings)) => settings.to_browse_defaults(),
            _ => BrowseDefaults::guest(),
        }
    }

    /// Records a durable cleanup intent before saving the key, then commits activation and
    /// clears that intent in the same SQLite transaction.
    pub fn commit_sign_in(
        &mut self,
        probe: SignInProbe,
    ) -> Result<SignInCommitOutcome, SignInError> {
        if probe.start_scope != self.scope {
            tracing::info!(
                start_gen = probe.start_scope.generation,
                current_gen = self.scope.generation,
                "discarded stale sign-in completion after session changed"
            );
            return Ok(SignInCommitOutcome::DiscardedStale);
        }

        if !self.can_sign_in() {
            return Err(SignInError::StorageUnavailable);
        }
        let account_id = AccountId::generate();
        let now = unix_now();
        let record = AccountRecord {
            id: account_id.clone(),
            username: probe.username,
            verification: probe.verification,
            is_active: true,
            created_at_unix: now,
            updated_at_unix: now,
        };
        let initial_cache = AccountCachedState {
            user_settings: probe.user_settings.clone(),
            own_collections: probe.own_collections.clone(),
            updated_at_unix: now,
        };

        self.storage
            .queue_credential_cleanup(&account_id)
            .map_err(|_| SignInError::Storage)?;
        if let Err(err) = self.credentials.save(&account_id, &probe.api_key) {
            tracing::error!(reason = %err, "failed to store credential during sign-in");
            Self::clean_account(&*self.storage, &*self.credentials, &account_id);
            return Err(SignInError::CredentialStore);
        }

        if let Err(err) =
            self.storage
                .save_and_activate_account(&record, Some(&initial_cache), true)
        {
            tracing::error!(reason = %err, "SQLite save failed during sign-in");
            // Check the durable intent before compensating, including if SQLite reported an
            // ambiguous commit error. A committed account no longer has an intent.
            if self
                .storage
                .pending_credential_cleanup()
                .is_ok_and(|ids| ids.contains(&account_id))
            {
                Self::clean_account(&*self.storage, &*self.credentials, &account_id);
            }
            return Err(SignInError::Storage);
        }

        self.scope =
            SessionScope::account(account_id.clone(), self.scope.generation.saturating_add(1));
        self.active_account = Some(record);
        self.user_settings = probe.user_settings;
        self.own_collections = probe.own_collections;
        self.accounts = self
            .storage
            .list_accounts()
            .map(|accounts| {
                accounts
                    .into_iter()
                    .map(|account| {
                        if account.id == account_id {
                            account
                        } else {
                            require_fresh_verification(account)
                        }
                    })
                    .collect()
            })
            .unwrap_or_else(|_| self.active_account.clone().into_iter().collect());
        self.notice = Some(match probe.verification {
            UsernameVerification::Verified => {
                SessionNotice::info("Signed in and username verified.")
            }
            UsernameVerification::UnverifiablePrivate => SessionNotice::warning(
                "Signed in with a valid API key. Username could not be verified because all owned collections are private.",
            ),
            UsernameVerification::UnverifiableEmpty => SessionNotice::warning(
                "Signed in with a valid API key. Username could not be verified because this account has no collections.",
            ),
            UsernameVerification::Inconclusive => SessionNotice::warning(
                "Signed in with a valid API key. Username verification was inconclusive; retry later.",
            ),
        });

        if self.user_settings.is_none()
            && let Some(notice) = &mut self.notice
        {
            notice.level = NoticeLevel::Warning;
            notice
                .message
                .push_str(" Wallhaven settings could not be read; using Guest browse defaults.");
        }

        Ok(SignInCommitOutcome::Applied {
            scope: self.scope.clone(),
            verification: probe.verification,
        })
    }

    /// Applies an async account refresh response after verifying scope/generation.
    pub fn apply_refresh(&mut self, payload: AccountRefreshPayload) -> RefreshOutcome {
        if payload.request_scope != self.scope {
            tracing::debug!(
                req_gen = payload.request_scope.generation,
                current_gen = self.scope.generation,
                "discarded stale account refresh response"
            );
            return RefreshOutcome::DiscardedStale;
        }

        let Some(account_id) = self.scope.account_id().cloned() else {
            return RefreshOutcome::DiscardedStale;
        };

        if matches!(payload.user_settings, Err(ProviderError::Unauthorized))
            || matches!(payload.own_collections, Err(ProviderError::Unauthorized))
        {
            let notice = self.revoke_active_account(&account_id);
            return RefreshOutcome::KeyRevoked(notice);
        }

        let mut warning: Option<SessionNotice> = None;
        for err in [
            payload.user_settings.as_ref().err(),
            payload.own_collections.as_ref().err(),
        ]
        .into_iter()
        .flatten()
        {
            if err.is_local_credential_error() {
                let warning = SessionNotice::warning(err.user_message());
                self.notice = Some(warning.clone());
                return RefreshOutcome::Applied {
                    warning: Some(warning),
                };
            }
        }

        let user_settings = match payload.user_settings {
            Ok(settings) => Some(settings),
            Err(err) => {
                warning = Some(SessionNotice::warning(format!(
                    "Could not refresh upstream settings: {}",
                    err.user_message()
                )));
                self.user_settings.clone()
            }
        };
        let own_collections = match payload.own_collections {
            Ok(collections) => Some(collections),
            Err(err) => {
                if warning.is_none() {
                    warning = Some(SessionNotice::warning(format!(
                        "Could not refresh own collections: {}",
                        err.user_message()
                    )));
                }
                self.own_collections.clone()
            }
        };

        let verification = payload.identity_outcome.map(|outcome| {
            outcome.into_accepted().unwrap_or_else(|| {
                warning = Some(SessionNotice::warning(
                    "Username verification detected a mismatch with public collections; username remains unverified.",
                ));
                UsernameVerification::Inconclusive
            })
        });
        let now = unix_now();
        let cache = AccountCachedState {
            user_settings,
            own_collections,
            updated_at_unix: now,
        };
        let persisted = self
            .storage
            .save_refresh(&account_id, verification, &cache)
            .is_ok();

        if persisted {
            self.user_settings = cache.user_settings;
            self.own_collections = cache.own_collections;
        } else {
            let detail = "Could not save the refresh to local storage.";
            match &mut warning {
                Some(warning) => {
                    warning.message.push(' ');
                    warning.message.push_str(detail);
                }
                None => warning = Some(SessionNotice::warning(detail)),
            }
        }
        if let Some(verification) = verification {
            let effective = if persisted || !verification.is_verified() {
                verification
            } else {
                UsernameVerification::Inconclusive
            };
            if let Some(active) = &mut self.active_account {
                active.verification = effective;
                active.updated_at_unix = now;
            }
            if let Some(account) = self.accounts.iter_mut().find(|a| a.id == account_id) {
                account.verification = effective;
                account.updated_at_unix = now;
            }
        }

        if let Some(w) = &warning {
            self.notice = Some(w.clone());
        } else {
            self.notice = Some(SessionNotice::info(
                "Account settings and collections refreshed.",
            ));
        }

        RefreshOutcome::Applied { warning }
    }

    /// Guards any provider response (search, wallpaper, etc.) with scope/generation checking and
    /// mid-session key-revocation handling.
    ///
    /// - Stale scope/generation -> `ScopedResponse::Stale` (discarded, never revokes active session).
    /// - `401 Unauthorized` on an active account -> bumps generation, purges caches, signs out,
    ///   and returns `ScopedResponse::Revoked(notice)`.
    /// - `401 Unauthorized` on a Guest request (e.g. guest requesting NSFW) -> returns
    ///   `ScopedResponse::Ready(Err(ProviderError::Unauthorized))` so upstream's restriction is
    ///   shown without adding client-side filtering or triggering account revocation.
    pub fn handle_provider_result<T>(
        &mut self,
        request_scope: &SessionScope,
        result: Result<T, ProviderError>,
    ) -> ScopedResponse<T> {
        if *request_scope != self.scope {
            return ScopedResponse::Stale;
        }

        if matches!(result, Err(ProviderError::Unauthorized))
            && let SessionKind::Account(account_id) = &request_scope.kind
        {
            let notice = self.revoke_active_account(account_id);
            return ScopedResponse::Revoked(notice);
        }

        ScopedResponse::Ready(result)
    }

    fn pending_cleanup_notice() -> SessionNotice {
        SessionNotice::warning(
            "Account cleanup is pending. Wallora will retry removing local data and the Windows Credential Manager entry at startup.",
        )
    }

    fn clean_account(
        storage: &dyn SessionStorage,
        credentials: &dyn CredentialStore,
        account_id: &AccountId,
    ) -> bool {
        // The journal normally hides the account. When revocation cannot write the journal,
        // either deactivation or vault deletion can independently prevent a restart restore.
        let purge_result = storage.deactivate_and_purge_account(account_id);
        match credentials.delete(account_id) {
            Ok(()) => storage.finish_credential_cleanup(account_id).is_ok(),
            Err(err) => {
                tracing::warn!(%err, cache_purged = purge_result.is_ok(), "account cleanup did not complete");
                false
            }
        }
    }

    fn clear_account_views(&mut self) {
        self.scope = SessionScope::guest(self.scope.generation.saturating_add(1));
        self.active_account = None;
        self.user_settings = None;
        self.own_collections = None;
    }

    fn retire_account(&mut self, account_id: &AccountId) -> Result<bool, StorageError> {
        self.storage.queue_credential_cleanup(account_id)?;
        self.clear_account_views();
        self.accounts.retain(|a| &a.id != account_id);
        Ok(Self::clean_account(
            &*self.storage,
            &*self.credentials,
            account_id,
        ))
    }

    fn revoke_active_account(&mut self, account_id: &AccountId) -> SessionNotice {
        let message = match self.retire_account(account_id) {
            Ok(true) => "Your API key was rejected by Wallhaven (401). Signed out and cleared local account data. Sign in again with a valid API key.".into(),
            Ok(false) => format!("Your API key was rejected by Wallhaven (401). Signed out. {}", Self::pending_cleanup_notice().message),
            Err(err) => {
                tracing::warn!(%err, "could not record revoked credential cleanup");
                self.clear_account_views();
                self.accounts.retain(|a| &a.id != account_id);
                if Self::clean_account(&*self.storage, &*self.credentials, account_id) {
                    "Your API key was rejected by Wallhaven (401). Signed out and cleared local account data. Sign in again with a valid API key.".into()
                } else {
                    "Your API key was rejected by Wallhaven (401). Signed out. Some local account data or the saved key could not be removed.".into()
                }
            }
        };
        let notice = SessionNotice::error(message);
        self.notice = Some(notice.clone());
        notice
    }

    /// Signs out after the cleanup intent is durable. A failed vault delete leaves a tombstone
    /// for startup recovery.
    pub fn logout(&mut self) -> LogoutOutcome {
        let Some(account_id) = self.scope.account_id().cloned() else {
            return LogoutOutcome {
                scope: self.scope.clone(),
                logged_out_account: None,
                warning: None,
            };
        };

        let (logged_out_account, warning) = match self.retire_account(&account_id) {
            Ok(true) => (Some(account_id), None),
            Ok(false) => (Some(account_id), Some(Self::pending_cleanup_notice())),
            Err(_) => (
                None,
                Some(SessionNotice::error(
                    "Could not record sign-out in local storage. Try signing out again.",
                )),
            ),
        };
        self.notice = Some(warning.clone().unwrap_or_else(|| {
            SessionNotice::info("Signed out to Guest and cleared account caches.")
        }));
        LogoutOutcome {
            scope: self.scope.clone(),
            logged_out_account,
            warning,
        }
    }

    /// Switches active session to another saved account without removing the outgoing account's
    /// credentials, while still bumping generation, clearing in-memory views, and purging the
    /// outgoing account's persisted cache.
    pub fn switch_account(&mut self, target_id: &AccountId) -> Result<SwitchOutcome, SwitchError> {
        if self.scope.account_id() == Some(target_id) {
            return Ok(SwitchOutcome {
                scope: self.scope.clone(),
                previous_account: Some(target_id.clone()),
            });
        }

        let target = self
            .accounts
            .iter()
            .find(|a| &a.id == target_id)
            .cloned()
            .ok_or(SwitchError::AccountNotFound)?;

        match self.credentials.load(target_id) {
            Ok(Some(secret)) if !secret.is_empty() => drop(secret),
            Ok(_) => return Err(SwitchError::MissingCredential),
            Err(_) => return Err(SwitchError::CredentialStoreUnavailable),
        }

        let previous_account = self.scope.account_id().cloned();
        self.storage
            .set_active_account(Some(target_id))
            .map_err(|_| SwitchError::Storage)?;

        self.scope =
            SessionScope::account(target_id.clone(), self.scope.generation.saturating_add(1));
        self.user_settings = None;
        self.own_collections = None;

        let mut updated_target = target;
        updated_target.is_active = true;
        self.active_account = Some(require_fresh_verification(updated_target));
        if let Ok(list) = self.storage.list_accounts() {
            self.accounts = list.into_iter().map(require_fresh_verification).collect();
        }

        self.notice = Some(SessionNotice::info(format!(
            "Switched active account to {}.",
            self.active_account
                .as_ref()
                .map(|a| a.username.as_str())
                .unwrap_or("account")
        )));

        Ok(SwitchOutcome {
            scope: self.scope.clone(),
            previous_account,
        })
    }

    /// Switches active session to Guest without deleting saved accounts or credentials, while
    /// bumping generation, clearing in-memory views, and purging the outgoing account's cache.
    pub fn switch_to_guest(&mut self) -> Result<SessionScope, StorageError> {
        let previous = self.scope.account_id().cloned();
        if previous.is_none() {
            return Ok(self.scope.clone());
        }

        self.storage.set_active_account(None)?;
        self.clear_account_views();
        if let Ok(list) = self.storage.list_accounts() {
            self.accounts = list.into_iter().map(require_fresh_verification).collect();
        }
        self.notice = Some(SessionNotice::info("Switched to Guest mode."));
        Ok(self.scope.clone())
    }

    /// Updates the local proxy preference on the single shared [`WallhavenClient`] and persists it
    /// separately from upstream account settings before publishing the transport.
    pub fn update_proxy(
        &mut self,
        client: &WallhavenClient,
        proxy: ProxyConfig,
    ) -> Result<ProxyConfig, ProxyUpdateError> {
        let candidate = client.prepare_proxy(&proxy)?;
        let applied = candidate.proxy().clone();
        let updated_prefs = LocalPreferences {
            proxy: applied.clone(),
        };
        self.storage
            .save_local_preferences(&updated_prefs)
            .map_err(|err| {
                tracing::error!(reason = %err, "failed to persist proxy preference");
                ProxyUpdateError::Storage
            })?;
        client.publish_proxy(candidate);
        self.local_preferences = updated_prefs;
        self.notice = Some(SessionNotice::info("Updated local proxy preference."));
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(id: u64, public: bool) -> CollectionSummary {
        CollectionSummary {
            id,
            label: format!("Collection {id}"),
            views: 0,
            public,
            count: 1,
        }
    }

    #[test]
    fn evaluate_username_verification_covers_all_five_outcomes() {
        // 1. Verified: public ids subset of own ids and at least one id in common.
        assert_eq!(
            evaluate_username_verification(
                Ok(&[col(10, true), col(20, false)]),
                Ok(&[col(10, true)]),
            ),
            IdentityCheckOutcome::Verified
        );

        // 2. Mismatch: any public id not in successfully decoded own list.
        assert_eq!(
            evaluate_username_verification(Ok(&[col(10, true)]), Ok(&[col(99, true)])),
            IdentityCheckOutcome::Mismatch
        );
        assert_eq!(
            evaluate_username_verification(Ok(&[]), Ok(&[col(99, true)])),
            IdentityCheckOutcome::Mismatch
        );

        // 3. UnverifiablePrivate: own list only has private collections, public list empty.
        assert_eq!(
            evaluate_username_verification(Ok(&[col(10, false)]), Ok(&[])),
            IdentityCheckOutcome::UnverifiablePrivate
        );

        // 4. UnverifiableEmpty: own list empty and public list empty.
        assert_eq!(
            evaluate_username_verification(Ok(&[]), Ok(&[])),
            IdentityCheckOutcome::UnverifiableEmpty
        );

        // 5. Inconclusive cases:
        // 5a. Ambiguous 404 with all-private own list (observed live on 2026-11-19).
        assert_eq!(
            evaluate_username_verification(Ok(&[col(10, false)]), Err(&ProviderError::NotFound)),
            IdentityCheckOutcome::Inconclusive
        );
        // 5b. Public list empty despite public entries in own list.
        assert_eq!(
            evaluate_username_verification(Ok(&[col(10, true)]), Ok(&[])),
            IdentityCheckOutcome::Inconclusive
        );
        // 5c. Own list failed/malformed.
        assert_eq!(
            evaluate_username_verification(
                Err(&ProviderError::Decode {
                    context: "collections".to_string()
                }),
                Ok(&[col(10, true)]),
            ),
            IdentityCheckOutcome::Inconclusive
        );
        // 5d. Public list failed/malformed.
        assert_eq!(
            evaluate_username_verification(Ok(&[col(10, true)]), Err(&ProviderError::Network),),
            IdentityCheckOutcome::Inconclusive
        );
    }
}
