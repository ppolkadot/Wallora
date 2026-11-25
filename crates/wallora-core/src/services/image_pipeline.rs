//! Guarded thumbnail and preview pipeline (Task 5): cache first, then the allowlisted image host,
//! with every byte checked by [`ImageGuard`] and every result verified against the session scope.
//!
//! Order of operations for [`ImagePipeline::load`]:
//! 1. Cache lookup under the request's [`SessionScope`]. A hit is decoded through the guard. If
//!    the entry cannot be decoded it is invalidated and fetched once more.
//! 2. On a miss (or a purged corrupt entry) the image is fetched from the image host. Its header
//!    and encoded size are checked *before* it is written, so a bomb is never cached.
//! 3. The bytes are stored under the scope (a failed store is not fatal) and decoded.
//! 4. The cache's current scope must still equal the request's scope before a texture is returned.
//!
//! Cancellation is cooperative: every await point races the caller's [`CancelToken`]. Dropping an
//! in-flight cache write cancels its reservation through the actor, so a cancelled load never
//! leaves a staging file, a charge, or a half-published entry behind.

use std::future::Future;
use std::sync::Arc;

use thiserror::Error;
use url::Url;

use crate::domain::SessionScope;
use crate::provider::{ImageClass, ProviderError, WallhavenClient};
use crate::services::cache::{CacheError, CacheKey, CacheLookup, CacheStore, CancelToken};
use crate::services::image_guard::{DecodedImage, ImageGuard, ImageGuardError};

/// Source of encoded image bytes. [`WallhavenClient`] is the production implementation; tests use
/// fakes so fetch counts and timing are observable.
pub trait ImageFetcher: Send + Sync + 'static {
    fn fetch(
        &self,
        url: Url,
        class: ImageClass,
    ) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send + 'static;
}

impl ImageFetcher for WallhavenClient {
    fn fetch(
        &self,
        url: Url,
        class: ImageClass,
    ) -> impl Future<Output = Result<Vec<u8>, ProviderError>> + Send + 'static {
        let client = self.clone();
        async move { client.fetch_image_bytes(&url, class).await }
    }
}

/// One image to load for the current session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRequest {
    /// URL taken from an API response (never constructed by the app).
    pub url: Url,
    pub class: ImageClass,
    pub scope: SessionScope,
}

impl ImageRequest {
    pub fn thumbnail(url: Url, scope: SessionScope) -> Self {
        Self {
            url,
            class: ImageClass::Thumbnail,
            scope,
        }
    }

    pub fn preview(url: Url, scope: SessionScope) -> Self {
        Self {
            url,
            class: ImageClass::Preview,
            scope,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSource {
    Cache,
    Network,
}

/// A decoded texture and where it came from.
#[derive(Debug)]
pub struct LoadedImage {
    pub image: DecodedImage,
    pub scope: SessionScope,
    pub source: ImageSource,
    /// The cache served this while offline, so it may be out of date.
    pub stale: bool,
    /// A cached copy was undecodable, so it was deleted and fetched again.
    pub recovered_from_corrupt_cache: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ImagePipelineError {
    #[error("the image load was cancelled")]
    Cancelled,
    #[error("the session changed while the image was loading")]
    ScopeChanged,
    #[error("{0}")]
    Provider(#[from] ProviderError),
    #[error("{0}")]
    Decode(#[from] ImageGuardError),
    #[error("the image cache is unavailable: {0}")]
    Cache(CacheError),
}

/// Decode failures that mean the bytes themselves are bad, as opposed to the decoder being out
/// of resources. Only these justify discarding a cached entry. Bytes are decoded from memory, so
/// an I/O error here is a truncated or damaged payload rather than a disk fault.
fn indicts_bytes(error: &ImageGuardError) -> bool {
    !matches!(
        error,
        ImageGuardError::DecodedBudgetExceeded { .. } | ImageGuardError::TaskCancelled
    )
}

async fn cancellable<T>(
    cancel: &CancelToken,
    future: impl Future<Output = T>,
) -> Result<T, ImagePipelineError> {
    let mut token = cancel.clone();
    tokio::select! {
        biased;
        () = token.cancelled() => Err(ImagePipelineError::Cancelled),
        output = future => Ok(output),
    }
}

/// Cache-first, guard-checked image loading. Cheap to clone; share one per app.
pub struct ImagePipeline<F: ImageFetcher> {
    fetcher: Arc<F>,
    cache: CacheStore,
    guard: ImageGuard,
}

impl<F: ImageFetcher> Clone for ImagePipeline<F> {
    fn clone(&self) -> Self {
        Self {
            fetcher: Arc::clone(&self.fetcher),
            cache: self.cache.clone(),
            guard: self.guard.clone(),
        }
    }
}

impl<F: ImageFetcher> ImagePipeline<F> {
    pub fn new(fetcher: Arc<F>, cache: CacheStore, guard: ImageGuard) -> Self {
        Self {
            fetcher,
            cache,
            guard,
        }
    }

    pub fn cache(&self) -> &CacheStore {
        &self.cache
    }

    pub fn guard(&self) -> &ImageGuard {
        &self.guard
    }

    /// Loads and decodes `request`, stopping promptly if `cancel` fires.
    pub async fn load(
        &self,
        request: ImageRequest,
        cancel: CancelToken,
    ) -> Result<LoadedImage, ImagePipelineError> {
        let ImageRequest { url, class, scope } = request;
        let key = CacheKey::new(class, url.as_str()).map_err(ImagePipelineError::Cache)?;
        if cancel.is_cancelled() {
            return Err(ImagePipelineError::Cancelled);
        }

        let mut recovered = false;
        match cancellable(&cancel, self.cache.get(key.clone(), scope.clone())).await? {
            Ok(CacheLookup::Hit {
                scope: hit_scope,
                bytes,
                stale,
            }) if hit_scope == scope => {
                match cancellable(&cancel, self.guard.decode_bytes(bytes, class)).await? {
                    Ok(image) => {
                        return self
                            .finish(image, scope, ImageSource::Cache, stale, false, &cancel)
                            .await;
                    }
                    Err(error) if indicts_bytes(&error) => {
                        tracing::debug!(%error, "cached image failed to decode; refetching once");
                        self.invalidate(&cancel, &key, &scope).await?;
                        recovered = true;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            // Another scope's entry, a miss, or an entry the cache just purged as corrupt.
            Ok(CacheLookup::Hit { .. } | CacheLookup::Miss | CacheLookup::CorruptPurged) => {}
            Err(CacheError::ShuttingDown) => {
                return Err(ImagePipelineError::Cache(CacheError::ShuttingDown));
            }
            // A broken cache must not stop images from loading; continue uncached.
            Err(error) => tracing::warn!(%error, "image cache read failed; continuing uncached"),
        }

        let bytes = cancellable(&cancel, self.fetcher.fetch(url, class)).await??;
        // Header and size checks come first so a bomb or garbage payload is never cached.
        ImageGuard::inspect_bytes(&bytes, class)?;

        let stored = self.store(&cancel, &key, &scope, &bytes).await?;
        match cancellable(&cancel, self.guard.decode_bytes(bytes, class)).await? {
            Ok(image) => {
                self.finish(
                    image,
                    scope,
                    ImageSource::Network,
                    false,
                    recovered,
                    &cancel,
                )
                .await
            }
            Err(error) => {
                if stored && indicts_bytes(&error) {
                    // Never leave an undecodable entry behind for the next caller.
                    let _ = self.cache.invalidate_entry(key, scope).await;
                }
                Err(error.into())
            }
        }
    }

    async fn invalidate(
        &self,
        cancel: &CancelToken,
        key: &CacheKey,
        scope: &SessionScope,
    ) -> Result<(), ImagePipelineError> {
        match cancellable(
            cancel,
            self.cache.invalidate_entry(key.clone(), scope.clone()),
        )
        .await?
        {
            Ok(_) => Ok(()),
            Err(CacheError::ShuttingDown) => {
                Err(ImagePipelineError::Cache(CacheError::ShuttingDown))
            }
            Err(error) => {
                tracing::warn!(%error, "could not invalidate a corrupt cache entry");
                Ok(())
            }
        }
    }

    /// Writes the fetched bytes. Returns whether they were published.
    async fn store(
        &self,
        cancel: &CancelToken,
        key: &CacheKey,
        scope: &SessionScope,
        bytes: &[u8],
    ) -> Result<bool, ImagePipelineError> {
        let put = self.cache.put(
            key.clone(),
            scope.clone(),
            Some(bytes.len() as u64),
            bytes.to_vec(),
        );
        match cancellable(cancel, put).await? {
            Ok(_) => Ok(true),
            Err(CacheError::ScopeMismatch) => Err(ImagePipelineError::ScopeChanged),
            Err(CacheError::ShuttingDown) => {
                Err(ImagePipelineError::Cache(CacheError::ShuttingDown))
            }
            // The actor cancels reservations when the scope moves on; the scope check in
            // `finish` turns that into `ScopeChanged`. Anything else (capacity, disk) only means
            // this image is not cached.
            Err(error) => {
                tracing::debug!(%error, "image not cached");
                Ok(false)
            }
        }
    }

    async fn finish(
        &self,
        image: DecodedImage,
        scope: SessionScope,
        source: ImageSource,
        stale: bool,
        recovered_from_corrupt_cache: bool,
        cancel: &CancelToken,
    ) -> Result<LoadedImage, ImagePipelineError> {
        let current = match cancellable(cancel, self.cache.stats()).await? {
            Ok(stats) => stats.current_scope,
            Err(error) => return Err(ImagePipelineError::Cache(error)),
        };
        if current != scope {
            return Err(ImagePipelineError::ScopeChanged);
        }
        Ok(LoadedImage {
            image,
            scope,
            source,
            stale,
            recovered_from_corrupt_cache,
        })
    }
}
