use std::error::Error as StdError;
use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use reqwest::StatusCode;
use tokio::sync::Semaphore;
use url::Url;

use crate::domain::{
    CollectionSummary, CredentialStore, ProxyConfig, ProxyError, RequestPriority, SearchPage,
    SearchQuery, SecretString, SessionKind, SessionScope, UserSettings, Wallpaper, WallpaperId,
    validate_username,
};
use crate::provider::Provider;
use crate::provider::api::{
    decode_collections_response, decode_search_response, decode_user_settings_response,
    decode_wallpaper_response, parse_retry_after,
};
use crate::provider::error::ProviderError;
use crate::provider::origin::{
    ImageOriginPolicy, OriginPolicy, authorize, authorize_secret, same_origin,
};
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

    /// Overall per-request deadline, so a trickling server cannot hold an image slot forever.
    pub const fn total_timeout(self) -> Duration {
        match self {
            Self::Thumbnail => Duration::from_secs(60),
            Self::Preview => Duration::from_secs(300),
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
    /// Test configurations may shorten the overall per-request image deadline.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) image_total_timeout_override: Option<Duration>,
    /// Test configurations must not inherit a developer machine's system proxy.
    pub(crate) use_system_proxy: bool,
    /// Initial proxy preference applied to both API and image clients.
    pub proxy: ProxyConfig,
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
            #[cfg(any(test, feature = "test-util"))]
            image_total_timeout_override: None,
            use_system_proxy: true,
            proxy: ProxyConfig::System,
        }
    }

    pub fn with_proxy(mut self, proxy: ProxyConfig) -> Self {
        self.proxy = proxy;
        self
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
            image_total_timeout_override: None,
            use_system_proxy: false,
            proxy: ProxyConfig::System,
        })
    }

    /// Shortens the overall per-request image deadline for tests.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_test_image_total_timeout(mut self, timeout: Duration) -> Self {
        self.image_total_timeout_override = Some(timeout);
        self
    }

    /// Allows image fetches from one explicit literal loopback origin.
    #[cfg(any(test, feature = "test-util"))]
    pub fn with_test_image_origin(mut self, origin: Url) -> Result<Self, ProviderError> {
        self.image_origin = ImageOriginPolicy::from_test_origin(&origin)?;
        Ok(self)
    }
}

pub(crate) struct HttpTransport {
    api_client: reqwest::Client,
    image_client: reqwest::Client,
    proxy: ProxyConfig,
}

impl HttpTransport {
    pub(crate) fn proxy(&self) -> &ProxyConfig {
        &self.proxy
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApiResponse {
    Json,
    Settings,
    SettingsProbe,
}

fn build_http_transport(
    config: &WallhavenClientConfig,
    proxy: &ProxyConfig,
) -> Result<HttpTransport, ProxyError> {
    let normalized = proxy.normalized()?;

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

    match &normalized {
        ProxyConfig::System => {
            if !config.use_system_proxy {
                api_builder = api_builder.no_proxy();
                image_builder = image_builder.no_proxy();
            }
        }
        ProxyConfig::Direct => {
            api_builder = api_builder.no_proxy();
            image_builder = image_builder.no_proxy();
        }
        ProxyConfig::Manual(proxy_url) => {
            let api_proxy =
                reqwest::Proxy::all(proxy_url).map_err(|_| ProxyError::TransportBuildFailed)?;
            let image_proxy =
                reqwest::Proxy::all(proxy_url).map_err(|_| ProxyError::TransportBuildFailed)?;
            api_builder = api_builder.no_proxy().proxy(api_proxy);
            image_builder = image_builder.no_proxy().proxy(image_proxy);
        }
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

    let api_client = api_builder
        .build()
        .map_err(|_| ProxyError::TransportBuildFailed)?;
    let image_client = image_builder
        .build()
        .map_err(|_| ProxyError::TransportBuildFailed)?;

    Ok(HttpTransport {
        api_client,
        image_client,
        proxy: normalized,
    })
}

enum RequestAuth {
    Scope(SessionScope),
    EphemeralKey { key: SecretString, probe_id: u64 },
}

struct WallhavenClientInner {
    transport: RwLock<HttpTransport>,
    image_sem: Arc<Semaphore>,
    pacer: ApiPacer,
    credentials: Arc<dyn CredentialStore>,
    config: WallhavenClientConfig,
    next_probe_id: AtomicU64,
}

/// Streaming response handle for full-resolution original downloads.
///
/// Shares the client's 6-slot image-host semaphore for the lifetime of the stream and relies on
/// the image client's 120-second idle-read timeout without thumbnail/preview byte ceilings.
pub struct OriginalImageStream {
    response: reqwest::Response,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl OriginalImageStream {
    pub fn content_length(&self) -> Option<u64> {
        self.response.content_length()
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, ProviderError> {
        loop {
            match self.response.chunk().await {
                Ok(Some(chunk)) if chunk.is_empty() => continue,
                Ok(Some(chunk)) => return Ok(Some(chunk.to_vec())),
                Ok(None) => return Ok(None),
                Err(err) => return Err(map_image_transport_error(&err)),
            }
        }
    }
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
        let transport =
            build_http_transport(&config, &config.proxy).map_err(|_| ProviderError::Network)?;

        Ok(Self {
            inner: Arc::new(WallhavenClientInner {
                transport: RwLock::new(transport),
                image_sem: Arc::new(Semaphore::new(IMAGE_IN_FLIGHT_CAP)),
                pacer: ApiPacer::new(),
                credentials,
                config,
                next_probe_id: AtomicU64::new(1),
            }),
        })
    }

    pub(crate) fn prepare_proxy(&self, proxy: &ProxyConfig) -> Result<HttpTransport, ProxyError> {
        build_http_transport(&self.inner.config, proxy)
    }

    /// Publishes the committed transport while preserving the shared pacer and image limits.
    pub(crate) fn publish_proxy(&self, transport: HttpTransport) {
        let mut guard = self
            .inner
            .transport
            .write()
            .unwrap_or_else(|err| err.into_inner());
        *guard = transport;
    }

    /// Returns the currently active proxy configuration on this client.
    pub fn proxy(&self) -> ProxyConfig {
        self.inner
            .transport
            .read()
            .map(|guard| guard.proxy.clone())
            .unwrap_or_default()
    }

    /// Observes the shared API cooldown for tests. Gated behind `test-util`.
    #[cfg(any(test, feature = "test-util"))]
    pub async fn wait_for_api_cooldown_for_test(&self) -> Duration {
        self.inner.pacer.wait_for_cooldown().await
    }

    /// Reports the free image slots. Gated behind `test-util`.
    #[cfg(any(test, feature = "test-util"))]
    pub fn available_image_slots_for_test(&self) -> usize {
        self.inner.image_sem.available_permits()
    }

    fn validate_image_url(&self, url: &Url) -> Result<(), ProviderError> {
        if url.scheme() != "https" && url.scheme() != "http" {
            return Err(ProviderError::OriginRejected);
        }
        if !self.inner.config.image_origin.is_authorized_image_url(url) {
            tracing::warn!("blocked image request failing origin policy");
            return Err(ProviderError::OriginRejected);
        }
        Ok(())
    }

    /// Fetches a bounded encoded thumbnail or preview from an allowlisted image host without
    /// API credentials, using same-origin redirects and the dedicated in-flight cap (6).
    /// The class's overall deadline bounds the send plus the whole body read.
    pub async fn fetch_image_bytes(
        &self,
        url: &Url,
        class: ImageClass,
    ) -> Result<Vec<u8>, ProviderError> {
        self.validate_image_url(url)?;

        let limit = class.byte_limit();
        let _permit = Arc::clone(&self.inner.image_sem)
            .acquire_owned()
            .await
            .map_err(|_| ProviderError::Busy)?;

        match tokio::time::timeout(
            self.inner.image_total_timeout(class),
            self.fetch_image_body(url, limit),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ProviderError::Timeout),
        }
    }

    /// Opens a streaming download for a full-resolution original wallpaper without sending API
    /// credentials or `Referer`, sharing the 6-slot image semaphore and enforcing same-origin
    /// redirects and the 120 s idle-read timeout without thumbnail/preview byte caps.
    pub async fn open_original_stream(
        &self,
        url: &Url,
    ) -> Result<OriginalImageStream, ProviderError> {
        self.validate_image_url(url)?;

        let permit = Arc::clone(&self.inner.image_sem)
            .acquire_owned()
            .await
            .map_err(|_| ProviderError::Busy)?;

        let image_client = self.inner.image_client()?;
        let resp = match image_client.get(url.clone()).send().await {
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

        Ok(OriginalImageStream {
            response: resp,
            _permit: permit,
        })
    }

    async fn fetch_image_body(&self, url: &Url, limit: usize) -> Result<Vec<u8>, ProviderError> {
        let image_client = self.inner.image_client()?;
        let resp = match image_client.get(url.clone()).send().await {
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
    fn api_client(&self) -> Result<reqwest::Client, ProviderError> {
        self.transport
            .read()
            .map(|guard| guard.api_client.clone())
            .map_err(|_| ProviderError::Network)
    }

    fn image_client(&self) -> Result<reqwest::Client, ProviderError> {
        self.transport
            .read()
            .map(|guard| guard.image_client.clone())
            .map_err(|_| ProviderError::Network)
    }

    fn image_total_timeout(&self, class: ImageClass) -> Duration {
        #[cfg(any(test, feature = "test-util"))]
        if let Some(timeout) = self.config.image_total_timeout_override {
            return timeout;
        }
        class.total_timeout()
    }

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
        auth: RequestAuth,
        priority: RequestPriority,
        response: ApiResponse,
    ) -> Result<Vec<u8>, ProviderError> {
        if !inner.config.origin_policy.is_authorized_api_url(&url) {
            tracing::warn!("blocked API request failing origin policy");
            return Err(ProviderError::OriginRejected);
        }

        let serialized = url.as_str().to_string();
        let coalesce_key = match &auth {
            RequestAuth::Scope(scope) => match &scope.kind {
                SessionKind::Guest => format!("guest:{}:{serialized}", scope.generation),
                SessionKind::Account(id) => format!(
                    "account:{}:{}:{}:{serialized}",
                    id.as_str().len(),
                    id.as_str(),
                    scope.generation
                ),
            },
            RequestAuth::EphemeralKey { probe_id, .. } => {
                // Never include the secret key in coalesce keys.
                format!("ephemeral:{probe_id}:{serialized}")
            }
        };

        let auth = Arc::new(auth);
        let pacer = inner.pacer.clone();
        pacer
            .run(coalesce_key, priority, move || {
                let inner = Arc::clone(&inner);
                let url = url.clone();
                let auth = Arc::clone(&auth);
                async move {
                    let api_client = inner.api_client()?;
                    let mut req = api_client
                        .get(url)
                        .build()
                        .map_err(|_| ProviderError::Network)?;

                    match auth.as_ref() {
                        RequestAuth::Scope(scope) => authorize(
                            &mut req,
                            scope,
                            inner.credentials.as_ref(),
                            &inner.config.origin_policy,
                        )?,
                        RequestAuth::EphemeralKey { key, .. } => {
                            authorize_secret(&mut req, key, &inner.config.origin_policy)?
                        }
                    }

                    let resp = match api_client.execute(req).await {
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
                        if response != ApiResponse::Json && status != StatusCode::OK {
                            return Ok(AttemptOutcome::Terminal(ProviderError::Upstream {
                                status: status.as_u16(),
                            }));
                        }
                        return match resp.bytes().await {
                            Ok(bytes) => Ok(AttemptOutcome::Success(bytes.to_vec())),
                            // Only sign-in can proceed on the status alone. An authenticated
                            // settings refresh still needs the body and retries failed reads.
                            Err(_) if response == ApiResponse::SettingsProbe => {
                                Ok(AttemptOutcome::Success(Vec::new()))
                            }
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
            let bytes = WallhavenClientInner::execute_paced_get(
                inner,
                url,
                RequestAuth::Scope(scope),
                q.priority,
                ApiResponse::Json,
            )
            .await?;
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
                RequestAuth::Scope(scope),
                RequestPriority::Interactive,
                ApiResponse::Json,
            )
            .await?;
            decode_wallpaper_response(&bytes)
        }
    }

    fn user_settings(
        &self,
        scope: SessionScope,
    ) -> impl Future<Output = Result<UserSettings, ProviderError>> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        async move {
            let url = inner.build_api_url(&["settings"])?;
            let bytes = WallhavenClientInner::execute_paced_get(
                inner,
                url,
                RequestAuth::Scope(scope),
                RequestPriority::Interactive,
                ApiResponse::Settings,
            )
            .await?;
            decode_user_settings_response(&bytes)
        }
    }

    fn own_collections(
        &self,
        scope: SessionScope,
    ) -> impl Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        async move {
            let url = inner.build_api_url(&["collections"])?;
            let bytes = WallhavenClientInner::execute_paced_get(
                inner,
                url,
                RequestAuth::Scope(scope),
                RequestPriority::Interactive,
                ApiResponse::Json,
            )
            .await?;
            decode_collections_response(&bytes)
        }
    }

    fn public_collections(
        &self,
        username: String,
        generation: u64,
    ) -> impl Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        async move {
            let clean = validate_username(&username).map_err(|_| ProviderError::NotFound)?;
            let url = inner.build_api_url(&["collections", &clean])?;
            // Public identity lookup MUST always remain unkeyed (Guest scope).
            let bytes = WallhavenClientInner::execute_paced_get(
                inner,
                url,
                RequestAuth::Scope(SessionScope::guest(generation)),
                RequestPriority::Interactive,
                ApiResponse::Json,
            )
            .await?;
            decode_collections_response(&bytes)
        }
    }

    fn user_settings_with_key(
        &self,
        api_key: SecretString,
    ) -> impl Future<Output = Result<Option<UserSettings>, ProviderError>> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        async move {
            let url = inner.build_api_url(&["settings"])?;
            let probe_id = inner.next_probe_id.fetch_add(1, Ordering::Relaxed);
            let bytes = WallhavenClientInner::execute_paced_get(
                inner,
                url,
                RequestAuth::EphemeralKey {
                    key: api_key,
                    probe_id,
                },
                RequestPriority::Interactive,
                ApiResponse::SettingsProbe,
            )
            .await?;
            Ok(decode_user_settings_response(&bytes).ok())
        }
    }

    fn own_collections_with_key(
        &self,
        api_key: SecretString,
    ) -> impl Future<Output = Result<Vec<CollectionSummary>, ProviderError>> + Send + 'static {
        let inner = Arc::clone(&self.inner);
        async move {
            let url = inner.build_api_url(&["collections"])?;
            let probe_id = inner.next_probe_id.fetch_add(1, Ordering::Relaxed);
            let bytes = WallhavenClientInner::execute_paced_get(
                inner,
                url,
                RequestAuth::EphemeralKey {
                    key: api_key,
                    probe_id,
                },
                RequestPriority::Interactive,
                ApiResponse::Json,
            )
            .await?;
            decode_collections_response(&bytes)
        }
    }
}
