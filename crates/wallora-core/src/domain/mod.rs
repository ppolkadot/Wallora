//! Core domain models and contracts.

pub mod account;
pub mod search;
pub mod wallpaper;

pub use account::{
    AccountId, CredentialError, CredentialStore, InMemoryCredentialStore, SecretString,
    SessionKind, SessionScope,
};
pub use search::{
    AspectRatio, CategoryMask, Dimensions, PurityMask, QueryEcho, RequestPriority, SearchMeta,
    SearchPage, SearchQuery, SortOrder, Sorting, TopRange,
};
pub use wallpaper::{Avatar, Category, Purity, Tag, Thumbs, Uploader, Wallpaper, WallpaperId};
