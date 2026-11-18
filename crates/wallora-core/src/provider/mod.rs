//! Upstream provider contract, origin isolation, request pacing, and Wallhaven API v1 client.

pub mod api;
pub mod client;
pub mod error;
pub mod origin;
pub mod pacer;

use std::future::Future;

use crate::domain::{SearchPage, SearchQuery, SessionScope, Wallpaper, WallpaperId};

pub use client::{
    IMAGE_IN_FLIGHT_CAP, ImageClass, WALLHAVEN_API_BASE, WALLORA_USER_AGENT, WallhavenClient,
    WallhavenClientConfig,
};
pub use error::ProviderError;
pub use origin::{OriginPolicy, authorize, same_origin};
pub use pacer::ApiPacer;

/// Provider abstraction consumed by GUI/services.
///
/// Task 2 exposes only `search` and `wallpaper`; later tasks add account/tag/collection methods.
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
}
