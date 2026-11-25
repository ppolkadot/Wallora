//! Core domain models and contracts.

pub mod account;
pub mod collection;
pub mod query_builder;
pub mod search;
pub mod wallpaper;

pub use account::{
    AccountId, AccountRecord, BrowseDefaults, CacheCapError, CredentialError, CredentialStore,
    DEFAULT_CACHE_CAP_BYTES, HARD_MAX_CACHE_CAP_BYTES, IdentityCheckOutcome,
    InMemoryCredentialStore, LocalPreferences, ProxyConfig, ProxyError, SecretString, SessionKind,
    SessionScope, UserSettings, UsernameVerification, WALLHAVEN_SETTINGS_URL,
    validate_cache_cap_preference, validate_username,
};
pub use collection::CollectionSummary;
pub use query_builder::{
    FileType, QueryBuilder, QueryToken, QueryTokenError, SearchTerms, parse_tokens,
};
pub use search::{
    AspectRatio, CategoryMask, Dimensions, PurityMask, QueryEcho, RequestPriority, SearchMeta,
    SearchPage, SearchQuery, SortOrder, Sorting, TopRange,
};
pub use wallpaper::{Avatar, Category, Purity, Tag, Thumbs, Uploader, Wallpaper, WallpaperId};
