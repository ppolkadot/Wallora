use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use tokio::sync::Semaphore;
use url::Url;

use crate::domain::{
    CredentialStore, RequestPriority, SearchPage, SearchQuery, SessionKind, SessionScope,
    Wallpaper, WallpaperId,
};
use crate::provider::Provider;
use crate::provider::api::{decode_search_response, decode_wallpaper_response, parse_retry_after};
use crate::provider::error::ProviderError;
use crate::provider::origin::{ImageOriginPolicy, OriginPolicy, authorize, same_origin};
use crate::provider::pacer::{ApiPacer, AttemptOutcome};

pub const WALLHAVEN_API_BASE: &str = "https://wallhaven.cc/api/v1";
pub const WALLORA_USER_AGENT: &str = concat!(
    "Wallora/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ppolkadot/Wallora)"
);
pub const IMAGE_IN_FLIGHT_CAP: usize = 6;
/// Encoded byte ceiling for thumbnails.
pub const THUMBNAIL_BYTE_LIMIT: usize = 2_000_000;
/// Encoded byte ceiling for previews.
pub const PREVIEW_BYTE_LIMIT: usize = 64_000_000;

/// Which bounded encoded-image shape a caller needs. Originals use a later streaming path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageClass {
    Thumbnail,
    Preview,
}

impl ImageClass {
    pub const fn byte_limit(self) -> usize {
        match self {
            Self::Thumbnail => THUMBNAIL_BYTE_LIMIT,
            Self::Preview => PREVIEW_BYTE_LIMIT,
        }
    }
}

#[derive(Debug)]
struct CrossOriginRedirectError;

impl fmt::Display for CrossOriginRedirectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cross-origin image redirect rejected")
    }
}

impl StdError for CrossOriginRedirectError {}

fn is_cross_origin_redirect_error(err: &reqwest::Error) -> bool {
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if e.downcast_ref::<CrossOriginRedirectError>().is_some() {
            return true;
        }
        cur = e.source();
    }
    false
}

#[derive(Debug, Clone)]
pub struct WallhavenClientConfig {
    pub api_base: Url,
    /// Only test configurations may move this away from the production policy.
    pub(crate) origin_policy: OriginPolicy,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub image_read_timeout: Duration,
    pub(crate) image_origin: ImageOriginPolicy,
    /// Test configurations must not inherit a developer machine's system proxy.
    pub(crate) use_system_proxy: bool,
}

impl WallhavenClientConfig {
    pub fn production() -> Self {
        Self {
            api_base: Url::parse(WALLHAVEN_API_BASE).expect("valid static API URL"),
            origin_policy: OriginPolicy::production(),
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            image_read_timeout: Duration::from_secs(120),
            image_origin: ImageOriginPolicy::production(),
            use_system_proxy: true,
        }
    }

    /// Builds a fully sandboxed loopback configuration. Image fetches are rejected until
    /// [`WallhavenClientConfig::with_test_image_origin`] names an explicit loopback origin.
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(api_base: Url) -> Result<Self, ProviderError> {
        let origin_policy = OriginPolicy::from_api_base(&api_base)?;
        Ok(Self {
            api_base,
            origin_policy,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            image_read_timeout: Duration::from_secs(120),
            image_origin: ImageOriginPolicy::Disabled,
            use_system_proxy: false,
        })
    }

    /// Allows image fetches from one explicit literal loopback origin.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_test_image_origin(mut self, origin: Url) -> Result<Self, ProviderError> {
        self.image_origin = ImageOriginPolicy::from_test_origin(&origin)?;
        Ok(self)
    }
}

struct WallhavenClientInner {
    api_client: reqwest::Client,
    image_client: reqwest::Client,
    image_sem: Semaphore,
    pacer: ApiPacer,
    credentials: Arc<dyn CredentialStore>,
    config: WallhavenClientConfig,
}

#[derive(Clone)]
pub struct WallhavenClient {
    inner: Arc<WallhavenClientInner>,
}

impl WallhavenClient {
    pub fn new(credentials: Arc<dyn CredentialStore>) -> Result<Self, ProviderError> {
        Self::with_config(credentials, WallhavenClientConfig::production())
    }

    pub fn with_config(
        credentials: Arc<dyn CredentialStore>,
        config: WallhavenClientConfig,
    ) -> Result<Self, ProviderError> {
        let mut api_builder = reqwest::Client::builder()
            .user_agent(WALLORA_USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout);

        let mut image_builder = reqwest::Client::builder()
            .user_agent(WALLORA_USER_AGENT)
            .referer(false)
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.image_read_timeout);

        if !config.use_system_proxy {
            api_builder = api_builder.no_proxy();
            image_builder = image_builder.no_proxy();
        }

        let image_origin = config.image_origin.clone();
        image_builder = image_builder.redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > 5 {
                return attempt.error(CrossOriginRedirectError);
            }
            let Some(initial) = attempt.previous().first() else {
                return attempt.error(CrossOriginRedirectError);
            };
            if !image_origin.is_authorized_image_url(attempt.url())
                || !same_origin(initial, attempt.url())
            {
                return attempt.error(CrossOriginRedirectError);
            }
            attempt.follow()
        }));

        let api_client = api_builder.build().map_err(|_| ProviderError::Network)?;
        let image_client = image_builder.build().map_err(|_| ProviderError::Network)?;

        Ok(Self {
            inner: Arc::new(WallhavenClientInner {
                api_client,
                image_client,
                image_sem: Semaphore::new(IMAGE_IN_FLIGHT_CAP),
                pacer: ApiPacer::new(),
                credentials,
                config,
            }),
        })
    }

    /// Observes the shared API cooldown for tests. Gated behind `test-util`.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn wait_for_api_cooldown_for_test(&self) -> Duration {
        self.inner.pacer.wait_for_cooldown().await
    }

    /// Fetches a bounded encoded thumbnail or preview from an allowlisted image host without
    /// API credentials, using same-origin redirects and the dedicated in-flight cap (6).
    pub async fn fetch_image_bytes(
        &self,
        url: &Url,
        class: ImageClass,
    ) -> Result<Vec<u8>, ProviderError> {
        if url.scheme() != "https" && url.scheme() != "http" {
            return Err(ProviderError::OriginRejected);
        }
        if !self.inner.config.image_origin.is_authorized_image_url(url) {
            tracing::warn!("blocked image request failing origin policy");
            return Err(ProviderError::OriginRejected);
        }

        let limit = class.byte_limit();
        let _permit = self
            .inner
            .image_sem
            .acquire()
            .await
            .map_err(|_| ProviderError::Busy)?;

        let resp = match self.inner.image_client.get(url.clone()).send().await {
            Ok(resp) => resp,
            Err(err) => return Err(map_image_transport_error(&err)),
        };

        let status = resp.status();
        if status.is_redirection() {
            return Err(ProviderError::OriginRejected);
        }
        if !status.is_success() {
            return Err(map_non_success_status(status, None));
        }

        if resp
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(ProviderError::ImageTooLarge { limit });
        }

        let mut response = resp;
        let mut body: Vec<u8> = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(err) => return Err(map_image_transport_error(&err)),
            };
            let projected = body
                .len()
                .checked_add(chunk.len())
                .ok_or(ProviderError::ImageTooLarge { limit })?;
            if projected > limit {
                return Err(ProviderError::ImageTooLarge { limit });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

fn map_image_transport_error(err: &reqwest::Error) -> ProviderError {
    if is_cross_origin_redirect_error(err) || err.is_redirect() {
        ProviderError::OriginRejected
    } else if err.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Network
    }
}

impl WallhavenClientInner {
    fn build_api_url(&self, segments: &[&str]) -> Result<Url, ProviderError> {
        let mut url = self.config.api_base.clone();
        {
            let mut path_segs = url
                .path_segments_mut()
                .map_err(|()| ProviderError::OriginRejected)?;
            path_segs.pop_if_empty();
            for seg in segments {
                path_segs.push(seg);
            }
        }
        Ok(url)
    }

    async fn execute_paced_get(
        inner: Arc<WallhavenClientInner>,
        url: Url,
        scope: SessionScope,
        priority: RequestPriority,
    ) -> Result<Vec<u8>, ProviderError> {
        if !inner.config.origin_policy.is_authorized_api_url(&url) {
            tracing::warn!("blocked API request failing origin policy");
            return Err(ProviderError::OriginRejected);
        }

        let serialized = url.as_str().to_string();
        let coalesce_key = match &scope.kind {
            SessionKind::Guest => format!("guest:{}:{serialized}", scope.generation),
            SessionKind::Account(id) => format!(
                "account:{}:{}:{}:{serialized}",
                id.as_str().len(),
                id.as_str(),
                scope.generation
            ),
        };

        let pacer = inner.pacer.clone();
        pacer
            .run(coalesce_key, priority, move || {
                let inner = Arc::clone(&inner);
                let url = url.clone();
                let scope = scope.clone();
                async move {
                    let mut req = inner
                        .api_client
                        .get(url)
                        .build()
                        .map_err(|_| ProviderError::Network)?;

                    authorize(
                        &mut req,
                        &scope,
                        inner.credentials.as_ref(),
                        &inner.config.origin_policy,
                    )?;

                    let resp = match inner.api_client.execute(req).await {
                        Ok(resp) => resp,
                        Err(err) => {
                            let mapped = if err.is_timeout() {
                                ProviderError::Timeout
                            } else {
                                ProviderError::Network
                            };
                            return Ok(AttemptOutcome::RetryableTransient(mapped));
                        }
                    };

                    let status = resp.status();
                    if status.is_success() {
                        return match resp.bytes().await {
                            Ok(bytes) => Ok(AttemptOutcome::Success(bytes.to_vec())),
                            Err(err) => {
                                let mapped = if err.is_timeout() {
                                    ProviderError::Timeout
                                } else {
                                    ProviderError::Network
                                };
                                Ok(AttemptOutcome::RetryableTransient(mapped))
                            }
                        };
                    }

                    if status == StatusCode::TOO_MANY_REQUESTS {
                        let retry_after = parse_retry_after(
                            resp.headers()
                                .get(reqwest::header::RETRY_AFTER)
                                .and_then(|v| v.to_str().ok()),
                        );
                        tracing::warn!(?retry_after, "upstream rate limit (429)");
                        return Ok(AttemptOutcome::RetryableRateLimit { retry_after });
                    }

                    if status.is_server_error() {
                        tracing::warn!(status = status.as_u16(), "upstream 5xx error");
                        return Ok(AttemptOutcome::RetryableTransient(
                            ProviderError::Upstream {
                                status: status.as_u16(),
                            },
                        ));
                    }

                    // 3xx, 401, 403, 404, and other 4xx are terminal (never retried).
                    let mapped = map_non_success_status(status, None);
                    tracing::debug!(status = status.as_u16(), "upstream terminal response");
                    Ok(AttemptOutcome::Terminal(mapped))
                }
            })
            .await
    }
}

fn map_non_success_status(status: StatusCode, retry_after: Option<Duration>) -> ProviderError {
    match status {
        StatusCode::UNAUTHORIZED => ProviderError::Unauthorized,
        StatusCode::FORBIDDEN => ProviderError::Forbidden,
        StatusCode::NOT_FOUND => ProviderError::NotFound,
        StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited { retry_after },
        _ => ProviderError::Upstream {
            status: status.as_u16(),
        },
    }
}

fn is_valid_wallpaper_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains('?')
        && !id.contains('#')
        && id != "."
        && id != ".."
}

impl Provider for WallhavenClient {
    fn search(
        &self,
        q: SearchQuery,
        scope: SessionScope,
    ) -> impl Future<Output = Result<SearchPage, ProviderError>> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        async move {
            let mut url = inner.build_api_url(&["search"])?;
            let pairs = q.to_query_pairs();
            if !pairs.is_empty() {
                let mut query_mut = url.query_pairs_mut();
                for (k, v) in pairs {
                    query_mut.append_pair(k, &v);
                }
            }
            let bytes =
                WallhavenClientInner::execute_paced_get(inner, url, scope, q.priority).await?;
            decode_search_response(&bytes)
        }
    }

    fn wallpaper(
        &self,
        id: WallpaperId,
        scope: SessionScope,
    ) -> impl Future<Output = Result<Wallpaper, ProviderError>> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        async move {
            if !is_valid_wallpaper_id(id.as_str()) {
                return Err(ProviderError::NotFound);
            }
            let url = inner.build_api_url(&["w", id.as_str()])?;
            let bytes = WallhavenClientInner::execute_paced_get(
                inner,
                url,
                scope,
                RequestPriority::Interactive,
            )
            .await?;
            decode_wallpaper_response(&bytes)
        }
    }
}
