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
    #[error("failed to save account metadata to local storage")]
    Storage,
}

/// Validated candidate sign-in payload awaiting synchronous commit under scope/generation guard.
#[derive(Debug, Clone)]
pub struct SignInProbe {
    pub start_scope: SessionScope,
    pub username: String,
    pub api_key: SecretString,
    pub user_settings: UserSettings,
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
    if matches!(user_settings, Err(ProviderError::Unauthorized)) {
        return AccountRefreshPayload {
            request_scope: scope,
            user_settings,
            own_collections: Err(ProviderError::Unauthorized),
            identity_outcome: None,
        };
    }

    let own_collections = provider.own_collections(scope.clone()).await;
    if matches!(own_collections, Err(ProviderError::Unauthorized)) {
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
    /// Records accounts whose persisted Task 3 caches were purged (also serves as the seam where
    /// Task 4's `CacheStore` purge actor will subscribe).
    purged_accounts: Vec<AccountId>,
}

impl SessionManager {
    /// Restores saved local preferences and active session from SQLite + `CredentialStore`.
    ///
    /// If SQLite names an active account whose credential is missing or unreadable in Credential
    /// Manager, falls back cleanly to Guest, purges that account's persisted cache, and records a
    /// non-secret warning notice.
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

        let accounts = match storage.list_accounts() {
            Ok(list) => list,
            Err(_) => {
                notice = Some(SessionNotice::warning(
                    "Could not read saved accounts from local storage; starting in Guest mode.",
                ));
                Vec::new()
            }
        };

        let mut scope = SessionScope::guest(1);
        let mut active_account = None;
        let mut user_settings = None;
        let mut own_collections = None;
        let mut purged_accounts = Vec::new();

        if let Some(candidate) = accounts.iter().find(|a| a.is_active).cloned() {
            match credentials.load(&candidate.id) {
                Ok(Some(secret)) if !secret.is_empty() => {
                    // Drop and zeroize the loaded key immediately; we only verify availability.
                    drop(secret);
                    scope = SessionScope::account(candidate.id.clone(), 1);
                    if let Ok(Some(cached)) = storage.load_account_cache(&candidate.id) {
                        user_settings = cached.user_settings;
                        own_collections = cached.own_collections;
                    }
                    active_account = Some(candidate);
                }
                Ok(_) => {
                    let _ = storage.set_active_account(None, Some(&candidate.id));
                    purged_accounts.push(candidate.id.clone());
                    notice = Some(SessionNotice::warning(
                        "Stored API key was missing from Windows Credential Manager. Signed out to Guest and cleared account caches; sign in again.",
                    ));
                }
                Err(_) => {
                    let _ = storage.purge_account_cache(&candidate.id);
                    purged_accounts.push(candidate.id.clone());
                    notice = Some(SessionNotice::warning(
                        "Windows Credential Manager could not be read. Continuing in Guest mode without cached account data.",
                    ));
                }
            }
        }

        let refreshed_accounts = storage.list_accounts().unwrap_or(accounts);

        Self {
            storage,
            credentials,
            scope,
            accounts: refreshed_accounts,
            active_account,
            user_settings,
            own_collections,
            local_preferences,
            notice,
            purged_accounts,
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

    pub fn purged_accounts(&self) -> &[AccountId] {
        &self.purged_accounts
    }

    /// Returns the effective browse defaults for the current session: upstream `UserSettings`
    /// when signed in and loaded, or audited Guest defaults otherwise.
    pub fn browse_defaults(&self) -> BrowseDefaults {
        match (&self.scope.kind, &self.user_settings) {
            (SessionKind::Account(_), Some(settings)) => settings.to_browse_defaults(),
            _ => BrowseDefaults::guest(),
        }
    }

    /// Synchronous Phase 2 of sign-in: verifies scope/generation has not changed while the async
    /// probe was in flight, saves the key to `CredentialStore`, stores metadata + initial cache in
    /// SQLite (compensating with credential deletion on SQLite failure), and bumps generation.
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
            user_settings: Some(probe.user_settings.clone()),
            own_collections: probe.own_collections.clone(),
            updated_at_unix: now,
        };

        // Step 1: Save secret in OS Credential Manager before activating in SQLite.
        if let Err(err) = self.credentials.save(&account_id, &probe.api_key) {
            tracing::error!(reason = %err, "failed to store credential during sign-in");
            return Err(SignInError::CredentialStore);
        }

        // Step 2: Persist account record & initial cache in SQLite (purging any outgoing account
        // cache). On failure, delete the just-saved credential so no orphan remains.
        let outgoing_account = self.scope.account_id().cloned();
        if let Err(err) =
            self.storage
                .save_and_activate_account(&record, Some(&initial_cache), true)
        {
            tracing::error!(
                account_id = %account_id,
                reason = %err,
                "SQLite save failed during sign-in; rolling back saved credential"
            );
            if let Err(cleanup_err) = self.credentials.delete(&account_id) {
                tracing::warn!(
                    account_id = %account_id,
                    reason = %cleanup_err,
                    "failed to roll back credential after SQLite sign-in error"
                );
            }
            return Err(SignInError::Storage);
        }

        if let Some(prev_id) = outgoing_account {
            self.purged_accounts.push(prev_id);
        }

        self.scope = SessionScope::account(account_id, self.scope.generation.saturating_add(1));
        self.active_account = Some(record);
        self.user_settings = Some(probe.user_settings);
        self.own_collections = probe.own_collections;
        self.accounts = self
            .storage
            .list_accounts()
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

        match payload.user_settings {
            Ok(settings) => {
                self.user_settings = Some(settings);
            }
            Err(err) => {
                warning = Some(SessionNotice::warning(format!(
                    "Could not refresh upstream settings: {}",
                    err.user_message()
                )));
            }
        }

        match payload.own_collections {
            Ok(collections) => {
                self.own_collections = Some(collections);
            }
            Err(err) => {
                if warning.is_none() {
                    warning = Some(SessionNotice::warning(format!(
                        "Could not refresh own collections: {}",
                        err.user_message()
                    )));
                }
            }
        }

        let now = unix_now();
        if let Some(outcome) = payload.identity_outcome {
            let new_verification = match outcome.into_accepted() {
                Some(v) => v,
                None => {
                    warning = Some(SessionNotice::warning(
                        "Username verification detected a mismatch with public collections; username remains unverified.",
                    ));
                    UsernameVerification::Inconclusive
                }
            };
            if let Some(active) = &mut self.active_account {
                active.verification = new_verification;
                active.updated_at_unix = now;
            }
            if self
                .storage
                .update_verification(&account_id, new_verification, now)
                .is_err()
                && warning.is_none()
            {
                warning = Some(SessionNotice::warning(
                    "Refreshed account in memory, but could not persist verification status.",
                ));
            }
            if let Ok(list) = self.storage.list_accounts() {
                self.accounts = list;
            }
        }

        let cache = AccountCachedState {
            user_settings: self.user_settings.clone(),
            own_collections: self.own_collections.clone(),
            updated_at_unix: now,
        };
        if self
            .storage
            .save_account_cache(&account_id, &cache)
            .is_err()
            && warning.is_none()
        {
            warning = Some(SessionNotice::warning(
                "Refreshed account in memory, but could not update local cache.",
            ));
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

    fn revoke_active_account(&mut self, account_id: &AccountId) -> SessionNotice {
        let outgoing = account_id.clone();
        self.scope = SessionScope::guest(self.scope.generation.saturating_add(1));
        self.active_account = None;
        self.user_settings = None;
        self.own_collections = None;
        self.purged_accounts.push(outgoing.clone());

        let db_err = self.storage.remove_account(&outgoing).err();
        let cred_err = self.credentials.delete(&outgoing).err();

        if let Ok(list) = self.storage.list_accounts() {
            self.accounts = list;
        } else {
            self.accounts.retain(|a| a.id != outgoing);
        }

        let mut message = "Your API key was rejected by Wallhaven (401). You have been signed out and account caches were cleared. Sign in again with a valid API key.".to_string();
        if db_err.is_some() || cred_err.is_some() {
            message.push_str(
                " Note: part of the local credential/storage cleanup encountered an error.",
            );
        }

        tracing::warn!(
            account_id = %outgoing,
            new_generation = self.scope.generation,
            db_cleanup_ok = db_err.is_none(),
            credential_cleanup_ok = cred_err.is_none(),
            "revoked active session after 401"
        );

        let notice = SessionNotice::error(message);
        self.notice = Some(notice.clone());
        notice
    }

    /// Logs out the active account: bumps generation, clears all in-memory account views, purges
    /// persisted Task 3 account cache, removes the account row, and deletes its key from
    /// `CredentialStore`. Preserves local proxy preferences, downloads, and applied wallpapers.
    pub fn logout(&mut self) -> LogoutOutcome {
        let Some(account_id) = self.scope.account_id().cloned() else {
            return LogoutOutcome {
                scope: self.scope.clone(),
                logged_out_account: None,
                warning: None,
            };
        };

        self.scope = SessionScope::guest(self.scope.generation.saturating_add(1));
        self.active_account = None;
        self.user_settings = None;
        self.own_collections = None;
        self.purged_accounts.push(account_id.clone());

        let db_err = self.storage.remove_account(&account_id).err();
        let cred_err = self.credentials.delete(&account_id).err();

        if let Ok(list) = self.storage.list_accounts() {
            self.accounts = list;
        } else {
            self.accounts.retain(|a| a.id != account_id);
        }

        let warning = match (db_err, cred_err) {
            (None, None) => None,
            (Some(_), None) => Some(SessionNotice::warning(
                "Signed out to Guest and removed the API key, but local storage cleanup failed.",
            )),
            (None, Some(_)) => Some(SessionNotice::warning(
                "Signed out to Guest and cleared account caches, but removing the entry from Windows Credential Manager failed.",
            )),
            (Some(_), Some(_)) => Some(SessionNotice::warning(
                "Signed out to Guest, but local storage and Windows Credential Manager cleanup failed.",
            )),
        };

        self.notice = Some(warning.clone().unwrap_or_else(|| {
            SessionNotice::info("Signed out to Guest and cleared account caches.")
        }));

        LogoutOutcome {
            scope: self.scope.clone(),
            logged_out_account: Some(account_id),
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
            .set_active_account(Some(target_id), previous_account.as_ref())
            .map_err(|_| SwitchError::Storage)?;

        if let Some(prev) = &previous_account {
            self.purged_accounts.push(prev.clone());
        }

        self.scope =
            SessionScope::account(target_id.clone(), self.scope.generation.saturating_add(1));
        self.user_settings = None;
        self.own_collections = None;

        if let Ok(Some(cached)) = self.storage.load_account_cache(target_id) {
            self.user_settings = cached.user_settings;
            self.own_collections = cached.own_collections;
        }

        let mut updated_target = target;
        updated_target.is_active = true;
        self.active_account = Some(updated_target);
        if let Ok(list) = self.storage.list_accounts() {
            self.accounts = list;
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

        self.storage.set_active_account(None, previous.as_ref())?;
        if let Some(prev) = previous {
            self.purged_accounts.push(prev);
        }

        self.scope = SessionScope::guest(self.scope.generation.saturating_add(1));
        self.active_account = None;
        self.user_settings = None;
        self.own_collections = None;
        if let Ok(list) = self.storage.list_accounts() {
            self.accounts = list;
        }
        self.notice = Some(SessionNotice::info("Switched to Guest mode."));
        Ok(self.scope.clone())
    }

    /// Updates the local proxy preference on the single shared [`WallhavenClient`] and persists it
    /// separately from upstream account settings. Rolls back transport changes if persistence fails.
    pub fn update_proxy(
        &mut self,
        client: &WallhavenClient,
        proxy: ProxyConfig,
    ) -> Result<ProxyConfig, ProxyUpdateError> {
        let previous = self.local_preferences.proxy.clone();
        let applied = client.set_proxy(proxy)?;
        let updated_prefs = LocalPreferences {
            proxy: applied.clone(),
        };
        if let Err(err) = self.storage.save_local_preferences(&updated_prefs) {
            tracing::error!(reason = %err, "failed to persist proxy preference; rolling back transport");
            let _ = client.set_proxy(previous);
            return Err(ProxyUpdateError::Storage);
        }
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
