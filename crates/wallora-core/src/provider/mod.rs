//! Upstream provider contract, origin isolation, request pacing, and Wallhaven API v1 client.

pub mod api;
pub mod client;
pub mod error;
pub mod origin;
pub mod pacer;

use std::future::Future;

use crate::domain::{
    CollectionSummary, PurityMask, SearchPage, SearchQuery, SecretString, SessionScope, Tag,
    UserSettings, Wallpaper, WallpaperId,
};

pub use client::{
    IMAGE_IN_FLIGHT_CAP, ImageClass, OriginalImageStream, WALLHAVEN_API_BASE, WALLORA_USER_AGENT,
    WallhavenClient, WallhavenClientConfig,
};
pub use error::ProviderError;
pub use origin::{OriginPolicy, authorize, authorize_secret, same_origin};
pub use pacer::ApiPacer;

/// Provider abstraction consumed by GUI/services.
///
/// Task 2 exposes `search` and `wallpaper`; Task 3 adds `user_settings`, `own_collections`,
/// unkeyed public collection lookup for username verification, and candidate-key sign-in probes
/// routed through the same pacer and origin policy. Task 6a adds `collections` and
/// `collection_items`. Task 6b adds `tag`.
pub trait Provider: Send + Sync + 'static {
    fn search(
        &self,
        q: SearchQuery,
        scope: SessionScope,
    ) -> impl Future<Output = Result<SearchPage, ProviderError>> + Send + 'static;

    fn wallpaper(
        &self,
        id: WallpaperId,
        scope: SessionScope,
    ) -> impl Future<Output = Result<Wallpaper, ProviderError>> + Send + 'static;

    fn user_settings(
        &self,
        scope: SessionScope,
    ) -> impl Future<Output = Result<UserSettings, ProviderError>> + Send + 'static;

    fn own_collections(
        &self,
        scope: SessionScope,
    ) -> impl Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static;

    /// Unkeyed lookup of a user's public collections (`GET /api/v1/collections/{username}`),
    /// used for username verification during sign-in and re-verification.
    fn public_collections(
        &self,
        username: String,
        generation: u64,
    ) -> impl Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static;

    /// Keyed `GET /api/v1/settings` using a candidate API key during sign-in, before writing
    /// anything to `CredentialStore` or SQLite.
    /// Only HTTP 200 succeeds. The optional defaults are decoded separately from key validity.
    fn user_settings_with_key(
        &self,
        api_key: SecretString,
    ) -> impl Future<Output = Result<Option<UserSettings>, ProviderError>> + Send + 'static;

    /// Keyed `GET /api/v1/collections` using a candidate API key during sign-in, before writing
    /// anything to `CredentialStore` or SQLite.
    fn own_collections_with_key(
        &self,
        api_key: SecretString,
    ) -> impl Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static;

    /// Lists a user's collections (`GET /api/v1/collections/{username}`), carrying the active
    /// session scope so guest requests stay unkeyed and account requests authenticate.
    fn collections(
        &self,
        username: String,
        scope: SessionScope,
    ) -> impl Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static;

    /// Fetches one page of a collection's wallpapers
    /// (`GET /api/v1/collections/{username}/{id}`), with optional `purity` and `page` query params.
    fn collection_items(
        &self,
        username: String,
        collection_id: u64,
        purity: Option<PurityMask>,
        page: Option<u32>,
        scope: SessionScope,
    ) -> impl Future<Output = Result<SearchPage, ProviderError>> + Send + 'static;

    /// Fetches tag metadata (`GET /api/v1/tag/{id}`).
    fn tag(
        &self,
        id: u64,
        scope: SessionScope,
    ) -> impl Future<Output = Result<Tag, ProviderError>> + Send + 'static;
}
