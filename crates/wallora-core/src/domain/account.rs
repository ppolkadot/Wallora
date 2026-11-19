use std::collections::HashMap;
use std::fmt;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::domain::search::{
    AspectRatio, CategoryMask, Dimensions, PurityMask, SearchQuery, TopRange, deserialize_per_page,
};

/// External browser hand-off URL for editing upstream account settings (Option A gap).
pub const WALLHAVEN_SETTINGS_URL: &str = "https://wallhaven.cc/settings";

/// Secret string wrapper that zeroizes its contents on drop and redacts Debug/Display.
/// Does not implement `Serialize` so secrets cannot be accidentally serialized.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Locally generated opaque account identifier (never the upstream username or API key).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(String);

impl AccountId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Generates a new opaque random UUIDv4 account identifier.
    pub fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validates a user-supplied Wallhaven username for safe URL path-segment placement.
pub fn validate_username(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("Enter your Wallhaven username.");
    }
    if trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.contains('%')
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("Username contains invalid characters.");
    }
    Ok(trimmed.to_string())
}

/// Persisted username verification state for an accepted account session.
///
/// `Mismatch` is intentionally absent: mismatched usernames are refused at sign-in and never
/// persisted or activated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UsernameVerification {
    Verified,
    UnverifiablePrivate,
    UnverifiableEmpty,
    Inconclusive,
}

impl UsernameVerification {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::UnverifiablePrivate => "unverifiable-private",
            Self::UnverifiableEmpty => "unverifiable-empty",
            Self::Inconclusive => "inconclusive",
        }
    }

    pub fn from_db_str(raw: &str) -> Option<Self> {
        match raw {
            "verified" => Some(Self::Verified),
            "unverifiable-private" => Some(Self::UnverifiablePrivate),
            "unverifiable-empty" => Some(Self::UnverifiableEmpty),
            "inconclusive" => Some(Self::Inconclusive),
            _ => None,
        }
    }

    pub fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }

    /// Own collection items and the Favorites label require a verified username (Task 6a).
    pub fn can_access_own_collection_items(self) -> bool {
        self.is_verified()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Verified => "Verified",
            Self::UnverifiablePrivate => "Unverifiable (private collections only)",
            Self::UnverifiableEmpty => "Unverifiable (no collections)",
            Self::Inconclusive => "Inconclusive (unverified)",
        }
    }

    pub fn explanation(self) -> &'static str {
        match self {
            Self::Verified => "Username matched a shared public collection ID with this API key.",
            Self::UnverifiablePrivate => {
                "Signed in with a valid API key, but all owned collections are private so the username cannot be verified publicly. Your own collection list stays available; collection items require a verified username."
            }
            Self::UnverifiableEmpty => {
                "Signed in with a valid API key, but this account has no collections so the username cannot be verified publicly. Collection items require a verified username."
            }
            Self::Inconclusive => {
                "Signed in with a valid API key, but username verification was inconclusive. Retry verification later; your own collection list stays available, while collection items remain locked until verified."
            }
        }
    }
}

/// Complete outcome of comparing keyed `/collections` against unkeyed `/collections/{username}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdentityCheckOutcome {
    Verified,
    Mismatch,
    UnverifiablePrivate,
    UnverifiableEmpty,
    Inconclusive,
}

impl IdentityCheckOutcome {
    /// Converts accepted outcomes to the persisted [`UsernameVerification`] state; returns `None`
    /// for [`IdentityCheckOutcome::Mismatch`], which must refuse sign-in.
    pub fn into_accepted(self) -> Option<UsernameVerification> {
        match self {
            Self::Verified => Some(UsernameVerification::Verified),
            Self::Mismatch => None,
            Self::UnverifiablePrivate => Some(UsernameVerification::UnverifiablePrivate),
            Self::UnverifiableEmpty => Some(UsernameVerification::UnverifiableEmpty),
            Self::Inconclusive => Some(UsernameVerification::Inconclusive),
        }
    }
}

/// Non-secret account metadata stored in SQLite (API keys are never stored here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: AccountId,
    pub username: String,
    pub verification: UsernameVerification,
    pub is_active: bool,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
}

/// Upstream read-only account settings decoded from `GET /api/v1/settings`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserSettings {
    #[serde(default)]
    pub thumb_size: String,
    #[serde(deserialize_with = "deserialize_per_page")]
    pub per_page: u32,
    #[serde(default)]
    pub purity: Vec<String>,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub resolutions: Vec<String>,
    #[serde(default)]
    pub aspect_ratios: Vec<String>,
    #[serde(default)]
    pub toplist_range: Option<String>,
    #[serde(default)]
    pub tag_blacklist: Vec<String>,
    #[serde(default)]
    pub user_blacklist: Vec<String>,
    #[serde(default)]
    pub ai_art_filter: Option<i64>,
}

impl UserSettings {
    pub fn purity_mask(&self) -> PurityMask {
        if self.purity.is_empty() {
            return PurityMask::SFW_ONLY;
        }
        let mut mask = PurityMask {
            sfw: false,
            sketchy: false,
            nsfw: false,
        };
        for entry in &self.purity {
            match entry.trim().to_ascii_lowercase().as_str() {
                "sfw" => mask.sfw = true,
                "sketchy" => mask.sketchy = true,
                "nsfw" => mask.nsfw = true,
                _ => {}
            }
        }
        if !mask.sfw && !mask.sketchy && !mask.nsfw {
            PurityMask::SFW_ONLY
        } else {
            mask
        }
    }

    pub fn category_mask(&self) -> CategoryMask {
        if self.categories.is_empty() {
            return CategoryMask::ALL;
        }
        let mut mask = CategoryMask {
            general: false,
            anime: false,
            people: false,
        };
        for entry in &self.categories {
            match entry.trim().to_ascii_lowercase().as_str() {
                "general" => mask.general = true,
                "anime" => mask.anime = true,
                "people" => mask.people = true,
                _ => {}
            }
        }
        if !mask.general && !mask.anime && !mask.people {
            CategoryMask::ALL
        } else {
            mask
        }
    }

    pub fn parsed_toplist_range(&self) -> TopRange {
        self.toplist_range
            .as_deref()
            .and_then(TopRange::from_api_str)
            .unwrap_or(TopRange::OneMonth)
    }

    pub fn parsed_resolutions(&self) -> Vec<Dimensions> {
        self.resolutions
            .iter()
            .filter_map(|raw| Dimensions::parse_wxh(raw))
            .collect()
    }

    pub fn parsed_aspect_ratios(&self) -> Vec<AspectRatio> {
        self.aspect_ratios
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(AspectRatio::new)
            .collect()
    }

    /// Projects supported upstream settings onto app-level browse defaults.
    pub fn to_browse_defaults(&self) -> BrowseDefaults {
        let thumb_size = if self.thumb_size.trim().is_empty() {
            "large".to_string()
        } else {
            self.thumb_size.trim().to_string()
        };
        BrowseDefaults {
            categories: self.category_mask(),
            purity: self.purity_mask(),
            per_page: if self.per_page == 0 {
                24
            } else {
                self.per_page
            },
            thumb_size,
            toplist_range: self.parsed_toplist_range(),
            resolutions: self.parsed_resolutions(),
            aspect_ratios: self.parsed_aspect_ratios(),
        }
    }
}

/// Effective browsing defaults applied to queries (either Guest defaults or upstream `UserSettings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowseDefaults {
    pub categories: CategoryMask,
    pub purity: PurityMask,
    pub per_page: u32,
    pub thumb_size: String,
    pub toplist_range: TopRange,
    pub resolutions: Vec<Dimensions>,
    pub aspect_ratios: Vec<AspectRatio>,
}

impl BrowseDefaults {
    /// Guest defaults matching the audited Wallhaven guest experience.
    pub fn guest() -> Self {
        Self {
            categories: CategoryMask::ALL,
            purity: PurityMask::SFW_ONLY,
            per_page: 24,
            thumb_size: "large".to_string(),
            toplist_range: TopRange::OneMonth,
            resolutions: Vec::new(),
            aspect_ratios: Vec::new(),
        }
    }

    /// Builds a [`SearchQuery`] seeded with the supported upstream defaults.
    pub fn to_search_query(&self) -> SearchQuery {
        SearchQuery {
            categories: Some(self.categories),
            purity: Some(self.purity),
            resolutions: self.resolutions.clone(),
            ratios: self.aspect_ratios.clone(),
            ..SearchQuery::default()
        }
    }
}

/// Validation and client-builder errors for the local manual proxy setting.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProxyError {
    #[error("enter a proxy URL (for example http://127.0.0.1:8080)")]
    EmptyUrl,
    #[error("invalid proxy URL; expected http://host:port or https://host:port")]
    InvalidUrl,
    #[error("proxy URL must use http:// or https://")]
    UnsupportedScheme,
    #[error("proxy URL must include a host")]
    MissingHost,
    #[error("proxy URLs with embedded credentials are not supported")]
    CredentialsNotSupported,
    #[error("proxy URL must not contain a query string or fragment")]
    DisallowedQueryOrFragment,
    #[error("failed to apply proxy configuration to HTTP transport")]
    TransportBuildFailed,
}

/// Local network proxy configuration, kept completely separate from upstream Wallhaven settings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ProxyConfig {
    /// Inherit Windows/environment system proxy settings (default).
    #[default]
    System,
    /// Bypass system proxies and connect directly.
    Direct,
    /// Route traffic through an explicit `http://` or `https://` proxy URL (for PAC/WPAD or
    /// per-protocol Windows proxy setups unsupported by `hyper-util`'s WinINET reader).
    Manual(String),
}

impl ProxyConfig {
    pub fn validate_manual_url(raw: &str) -> Result<Url, ProxyError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ProxyError::EmptyUrl);
        }
        let parsed = Url::parse(trimmed).map_err(|_| ProxyError::InvalidUrl)?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(ProxyError::UnsupportedScheme);
        }
        if parsed.host_str().is_none() {
            return Err(ProxyError::MissingHost);
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(ProxyError::CredentialsNotSupported);
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(ProxyError::DisallowedQueryOrFragment);
        }
        Ok(parsed)
    }

    pub fn normalized(&self) -> Result<Self, ProxyError> {
        match self {
            Self::System => Ok(Self::System),
            Self::Direct => Ok(Self::Direct),
            Self::Manual(raw) => {
                let parsed = Self::validate_manual_url(raw)?;
                Ok(Self::Manual(parsed.to_string()))
            }
        }
    }

    pub fn mode_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Direct => "direct",
            Self::Manual(_) => "manual",
        }
    }
}

/// Local-only application preferences persisted independently of accounts and upstream settings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LocalPreferences {
    pub proxy: ProxyConfig,
}

/// Whether a request or cache entry belongs to guest browsing or a signed-in account.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionKind {
    Guest,
    Account(AccountId),
}

/// Scope carried by every provider request, response, cache read, and cache write.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionScope {
    pub kind: SessionKind,
    pub generation: u64,
}

impl SessionScope {
    pub fn guest(generation: u64) -> Self {
        Self {
            kind: SessionKind::Guest,
            generation,
        }
    }

    pub fn account(id: AccountId, generation: u64) -> Self {
        Self {
            kind: SessionKind::Account(id),
            generation,
        }
    }

    pub fn account_id(&self) -> Option<&AccountId> {
        match &self.kind {
            SessionKind::Guest => None,
            SessionKind::Account(id) => Some(id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CredentialError {
    #[error("credential store unavailable: {0}")]
    Unavailable(String),
    #[error("credential I/O failed: {0}")]
    Io(String),
}

/// OS-backed secret storage contract (implemented in `wallora-windows` for release).
pub trait CredentialStore: Send + Sync + 'static {
    fn load(&self, account: &AccountId) -> Result<Option<SecretString>, CredentialError>;
    fn save(&self, account: &AccountId, key: &SecretString) -> Result<(), CredentialError>;
    fn delete(&self, account: &AccountId) -> Result<(), CredentialError>;
}

/// In-memory credential store for tests and guest-only provider construction.
#[derive(Debug, Default)]
pub struct InMemoryCredentialStore {
    entries: RwLock<HashMap<AccountId, SecretString>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn load(&self, account: &AccountId) -> Result<Option<SecretString>, CredentialError> {
        let guard = self
            .entries
            .read()
            .map_err(|_| CredentialError::Unavailable("lock poisoned".to_string()))?;
        Ok(guard.get(account).cloned())
    }

    fn save(&self, account: &AccountId, key: &SecretString) -> Result<(), CredentialError> {
        let mut guard = self
            .entries
            .write()
            .map_err(|_| CredentialError::Unavailable("lock poisoned".to_string()))?;
        guard.insert(account.clone(), key.clone());
        Ok(())
    }

    fn delete(&self, account: &AccountId) -> Result<(), CredentialError> {
        let mut guard = self
            .entries
            .write()
            .map_err(|_| CredentialError::Unavailable("lock poisoned".to_string()))?;
        guard.remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_validation_rejects_traversal_and_reserved_chars() {
        assert_eq!(validate_username("alice_99").as_deref(), Ok("alice_99"));
        assert_eq!(validate_username("  bob-1  ").as_deref(), Ok("bob-1"));
        for bad in [
            "", "   ", ".", "..", "a/b", "a\\b", "a?x=1", "a#frag", "a b", "a%20b",
        ] {
            assert!(validate_username(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn proxy_validation_accepts_http_https_and_rejects_credentials() {
        assert!(ProxyConfig::validate_manual_url("http://127.0.0.1:8080").is_ok());
        assert!(ProxyConfig::validate_manual_url("https://proxy.corp.local:8443").is_ok());
        assert_eq!(
            ProxyConfig::validate_manual_url(""),
            Err(ProxyError::EmptyUrl)
        );
        assert_eq!(
            ProxyConfig::validate_manual_url("socks5://127.0.0.1:1080"),
            Err(ProxyError::UnsupportedScheme)
        );
        assert_eq!(
            ProxyConfig::validate_manual_url("http://user:pass@127.0.0.1:8080"),
            Err(ProxyError::CredentialsNotSupported)
        );
        assert_eq!(
            ProxyConfig::validate_manual_url("http://127.0.0.1:8080/?a=1"),
            Err(ProxyError::DisallowedQueryOrFragment)
        );
    }

    #[test]
    fn account_id_generate_produces_distinct_uuids() {
        let a = AccountId::generate();
        let b = AccountId::generate();
        assert_ne!(a, b);
        assert!(Uuid::parse_str(a.as_str()).is_ok());
    }
}
