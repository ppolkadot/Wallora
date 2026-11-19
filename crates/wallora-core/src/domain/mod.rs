//! Core domain models and contracts.

pub mod account;
pub mod collection;
pub mod search;
pub mod wallpaper;

pub use account::{
    AccountId, AccountRecord, BrowseDefaults, CredentialError, CredentialStore,
    IdentityCheckOutcome, InMemoryCredentialStore, LocalPreferences, ProxyConfig, ProxyError,
    SecretString, SessionKind, SessionScope, UserSettings, UsernameVerification,
    WALLHAVEN_SETTINGS_URL, validate_username,
};
pub use collection::CollectionSummary;
pub use search::{
    AspectRatio, CategoryMask, Dimensions, PurityMask, QueryEcho, RequestPriority, SearchMeta,
    SearchPage, SearchQuery, SortOrder, Sorting, TopRange,
};
pub use wallpaper::{Avatar, Category, Purity, Tag, Thumbs, Uploader, Wallpaper, WallpaperId};
