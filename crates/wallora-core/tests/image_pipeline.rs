//! Image pipeline behavior with a fake fetcher, a real `CacheStore` and a real `ImageGuard`.

use std::collections::VecDeque;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use image::{ImageFormat, Rgba, RgbaImage};
use tokio::sync::Semaphore;
use url::Url;
use wallora_core::domain::{AccountId, SessionScope};
use wallora_core::provider::{ImageClass, ProviderError};
use wallora_core::services::cache::{
    CacheConfig, CacheConnectivity, CacheError, CacheKey, CacheLookup, CacheScopeTransition,
    CacheStore, CancelToken,
};
use wallora_core::services::{
    ImageFetcher, ImageGuard, ImageGuardConfig, ImageGuardError, ImagePipeline, ImagePipelineError,
    ImageRequest, ImageSource, load_uncached,
};

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("wallora-pipeline-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

type Response = Result<Vec<u8>, ProviderError>;

/// Scripted fetcher: counts calls, can hold requests at a gate, and notices dropped futures.
struct FakeFetcher {
    queued: Mutex<VecDeque<Response>>,
    fallback: Mutex<Response>,
    calls: AtomicUsize,
    dropped_in_flight: Arc<AtomicUsize>,
    entered: Arc<Semaphore>,
    gate: Mutex<Option<Arc<Semaphore>>>,
}

impl FakeFetcher {
    fn new(fallback: Response) -> Arc<Self> {
        Arc::new(Self {
            queued: Mutex::default(),
            fallback: Mutex::new(fallback),
            calls: AtomicUsize::new(0),
            dropped_in_flight: Arc::default(),
            entered: Arc::new(Semaphore::new(0)),
            gate: Mutex::new(None),
        })
    }

    fn set_fallback(&self, response: Response) {
        *self.fallback.lock().unwrap() = response;
    }

    fn queue(&self, response: Response) {
        self.queued.lock().unwrap().push_back(response);
    }

    fn close_gate(&self) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *self.gate.lock().unwrap() = Some(Arc::clone(&gate));
        gate
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn wait_until_entered(&self) {
        self.entered.acquire().await.unwrap().forget();
    }
}

struct DropGuard {
    counter: Arc<AtomicUsize>,
    armed: bool,
}

impl DropGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        if self.armed {
            self.counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl ImageFetcher for FakeFetcher {
    fn fetch(
        &self,
        _url: Url,
        _class: ImageClass,
    ) -> impl std::future::Future<Output = Response> + Send + 'static {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let response = self
            .queued
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| self.fallback.lock().unwrap().clone());
        let gate = self.gate.lock().unwrap().clone();
        let entered = Arc::clone(&self.entered);
        let mut guard = DropGuard {
            counter: Arc::clone(&self.dropped_in_flight),
            armed: true,
        };
        async move {
            entered.add_permits(1);
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            guard.disarm();
            response
        }
    }
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = RgbaImage::from_pixel(width, height, Rgba([200, 30, 60, 255]));
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .expect("encode PNG");
    out
}

/// A PNG whose header is valid but whose compressed data fails its checksum.
fn corrupt_png() -> Vec<u8> {
    let mut bytes = png(32, 32);
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xFF;
    bytes
}

fn url(name: &str) -> Url {
    Url::parse(&format!("https://th.wallhaven.cc/small/ab/{name}.png")).unwrap()
}

fn key_for(url: &Url, class: ImageClass) -> CacheKey {
    CacheKey::new(class, url.as_str()).unwrap()
}

fn guest(generation: u64) -> SessionScope {
    SessionScope::guest(generation)
}

async fn open_cache(dir: &TempDir, cap: u64) -> CacheStore {
    CacheStore::open(
        CacheConfig::new(dir.0.join("cache"), guest(1))
            .with_cap_bytes(cap)
            .unwrap(),
    )
    .await
    .unwrap()
}

fn pipeline(fetcher: &Arc<FakeFetcher>, cache: &CacheStore) -> ImagePipeline<FakeFetcher> {
    ImagePipeline::new(Arc::clone(fetcher), cache.clone(), ImageGuard::new())
}

async fn load(
    pipeline: &ImagePipeline<FakeFetcher>,
    url: &Url,
    scope: SessionScope,
) -> Result<wallora_core::services::LoadedImage, ImagePipelineError> {
    tokio::time::timeout(
        Duration::from_secs(30),
        pipeline.load(
            ImageRequest::thumbnail(url.clone(), scope),
            CancelToken::new(),
        ),
    )
    .await
    .expect("load timed out")
}

#[tokio::test]
async fn miss_fetches_stores_and_decodes_then_the_next_load_hits_the_cache() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(8, 6)));
    let pipeline = pipeline(&fetcher, &cache);
    let url = url("one");

    let first = load(&pipeline, &url, guest(1)).await.unwrap();
    assert_eq!(first.source, ImageSource::Network);
    assert_eq!(first.image.dimensions(), (8, 6));
    assert_eq!(first.image.rgba().len(), 8 * 6 * 4);
    assert_eq!(first.scope, guest(1));
    assert!(!first.stale && !first.recovered_from_corrupt_cache);
    assert_eq!(fetcher.calls(), 1);
    assert_eq!(cache.stats().await.unwrap().entry_count, 1);

    let second = load(&pipeline, &url, guest(1)).await.unwrap();
    assert_eq!(second.source, ImageSource::Cache);
    assert_eq!(second.image.rgba(), first.image.rgba());
    assert_eq!(fetcher.calls(), 1, "the hit must not touch the network");
    cache.shutdown().await;
}

#[tokio::test]
async fn undecodable_cached_entries_are_deleted_and_refetched_exactly_once() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(10, 10)));
    let pipeline = pipeline(&fetcher, &cache);

    for (name, bad) in [
        ("garbage", vec![0x13; 64]),
        ("truncated", png(10, 10)[..40].to_vec()),
        ("bad-crc", corrupt_png()),
    ] {
        let url = url(name);
        let key = key_for(&url, ImageClass::Thumbnail);
        cache
            .put(key.clone(), guest(1), Some(bad.len() as u64), bad)
            .await
            .unwrap();
        let before = fetcher.calls();

        let loaded = load(&pipeline, &url, guest(1)).await.unwrap();
        assert_eq!(loaded.source, ImageSource::Network, "{name}");
        assert!(loaded.recovered_from_corrupt_cache, "{name}");
        assert_eq!(loaded.image.dimensions(), (10, 10));
        assert_eq!(fetcher.calls(), before + 1, "{name}: exactly one refetch");

        // The replacement entry is the good one.
        let CacheLookup::Hit { bytes, .. } = cache.get(key, guest(1)).await.unwrap() else {
            panic!("{name}: refetched entry should be cached");
        };
        assert_eq!(bytes, png(10, 10));
        let hit = load(&pipeline, &url, guest(1)).await.unwrap();
        assert_eq!(hit.source, ImageSource::Cache);
        assert_eq!(fetcher.calls(), before + 1);
    }
    cache.shutdown().await;
}

#[tokio::test]
async fn a_second_bad_payload_is_an_error_and_leaves_nothing_cached() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(vec![0x13; 64]));
    let pipeline = pipeline(&fetcher, &cache);
    let url = url("bad-twice");
    cache
        .put(
            key_for(&url, ImageClass::Thumbnail),
            guest(1),
            Some(64),
            vec![0x13; 64],
        )
        .await
        .unwrap();

    let err = load(&pipeline, &url, guest(1)).await.unwrap_err();
    assert!(matches!(err, ImagePipelineError::Decode(_)), "{err:?}");
    assert_eq!(fetcher.calls(), 1, "only one refetch, never a loop");
    let stats = cache.stats().await.unwrap();
    assert_eq!((stats.entry_count, stats.total_charged_bytes), (0, 0));
    cache.shutdown().await;
}

#[tokio::test]
async fn fetched_images_that_pass_the_header_check_but_fail_to_decode_are_not_left_cached() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(corrupt_png()));
    let pipeline = pipeline(&fetcher, &cache);
    let url = url("corrupt-fetch");

    let err = load(&pipeline, &url, guest(1)).await.unwrap_err();
    assert!(matches!(err, ImagePipelineError::Decode(_)), "{err:?}");
    let stats = cache.stats().await.unwrap();
    assert_eq!((stats.entry_count, stats.live_reservations), (0, 0));
    assert_eq!(
        cache
            .get(key_for(&url, ImageClass::Thumbnail), guest(1))
            .await
            .unwrap(),
        CacheLookup::Miss
    );
    cache.shutdown().await;
}

#[tokio::test]
async fn oversized_images_are_rejected_before_they_are_cached() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 50_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(1025, 1)));
    let pipeline = pipeline(&fetcher, &cache);

    let wide = url("wide");
    let err = load(&pipeline, &wide, guest(1)).await.unwrap_err();
    assert!(
        matches!(
            err,
            ImagePipelineError::Decode(
                ImageGuardError::DimensionsExceeded { .. } | ImageGuardError::DecoderLimit(_)
            )
        ),
        "{err:?}"
    );

    fetcher.set_fallback(Ok(vec![0u8; 2_000_001]));
    let heavy = url("heavy");
    let err = load(&pipeline, &heavy, guest(1)).await.unwrap_err();
    assert!(
        matches!(
            err,
            ImagePipelineError::Decode(ImageGuardError::EncodedTooLarge { .. })
        ),
        "{err:?}"
    );
    let stats = cache.stats().await.unwrap();
    assert_eq!(
        (
            stats.entry_count,
            stats.published_bytes,
            stats.total_charged_bytes
        ),
        (0, 0, 0),
        "rejected payloads never reach the cache"
    );

    // The same dimensions are fine as a preview.
    fetcher.set_fallback(Ok(png(2000, 4)));
    let preview = pipeline
        .load(
            ImageRequest::preview(url("panorama"), guest(1)),
            CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(preview.image.dimensions(), (2000, 4));
    cache.shutdown().await;
}

#[tokio::test]
async fn provider_errors_are_returned_and_nothing_is_cached() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Err(ProviderError::Network));
    let pipeline = pipeline(&fetcher, &cache);
    let url = url("flaky");

    let err = load(&pipeline, &url, guest(1)).await.unwrap_err();
    assert_eq!(err, ImagePipelineError::Provider(ProviderError::Network));
    assert_eq!(cache.stats().await.unwrap().entry_count, 0);

    fetcher.set_fallback(Ok(png(4, 4)));
    assert!(load(&pipeline, &url, guest(1)).await.is_ok());
    assert_eq!(fetcher.calls(), 2);
    cache.shutdown().await;
}

#[tokio::test]
async fn entries_are_scoped_and_stale_scopes_never_get_a_texture() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(5, 5)));
    let pipeline = pipeline(&fetcher, &cache);
    let url = url("scoped");

    load(&pipeline, &url, guest(1)).await.unwrap();
    assert_eq!(fetcher.calls(), 1);

    // The session signs in: the account has its own namespace, so the guest copy is not used.
    let account = SessionScope::account(AccountId::generate(), 2);
    cache
        .transition_scope(CacheScopeTransition {
            new_scope: account.clone(),
            purge_account: None,
        })
        .await
        .unwrap();
    let loaded = load(&pipeline, &url, account.clone()).await.unwrap();
    assert_eq!(loaded.source, ImageSource::Network);
    assert_eq!(fetcher.calls(), 2);
    assert_eq!(
        load(&pipeline, &url, account.clone()).await.unwrap().source,
        ImageSource::Cache
    );

    // A request still carrying the old guest scope is refused instead of being served.
    let err = load(&pipeline, &url, guest(1)).await.unwrap_err();
    assert_eq!(err, ImagePipelineError::ScopeChanged);
    cache.shutdown().await;
}

#[tokio::test]
async fn a_scope_change_while_fetching_discards_the_result_and_publishes_nothing() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(6, 6)));
    let gate = fetcher.close_gate();
    let pipeline = pipeline(&fetcher, &cache);
    let url = url("mid-flight");

    let task = {
        let (pipeline, url) = (pipeline.clone(), url.clone());
        tokio::spawn(async move {
            pipeline
                .load(ImageRequest::thumbnail(url, guest(1)), CancelToken::new())
                .await
        })
    };
    fetcher.wait_until_entered().await;
    cache
        .transition_scope(CacheScopeTransition {
            new_scope: guest(2),
            purge_account: None,
        })
        .await
        .unwrap();
    gate.add_permits(1);

    let err = task.await.unwrap().unwrap_err();
    assert_eq!(err, ImagePipelineError::ScopeChanged);
    let stats = cache.stats().await.unwrap();
    assert_eq!((stats.entry_count, stats.live_reservations), (0, 0));
    for scope in [guest(1), guest(2)] {
        assert_eq!(
            cache
                .get(key_for(&url, ImageClass::Thumbnail), scope)
                .await
                .unwrap(),
            CacheLookup::Miss
        );
    }
    cache.shutdown().await;
}

#[tokio::test]
async fn cancelling_mid_fetch_stops_cleanly_without_poisoning_the_cache() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(7, 7)));
    let gate = fetcher.close_gate();
    let pipeline = pipeline(&fetcher, &cache);
    let url = url("scrolled-away");
    let token = CancelToken::new();

    let task = {
        let (pipeline, url, token) = (pipeline.clone(), url.clone(), token.clone());
        tokio::spawn(async move {
            pipeline
                .load(ImageRequest::thumbnail(url, guest(1)), token)
                .await
        })
    };
    fetcher.wait_until_entered().await;
    token.cancel();

    let err = task.await.unwrap().unwrap_err();
    assert_eq!(err, ImagePipelineError::Cancelled);
    assert_eq!(
        fetcher.dropped_in_flight.load(Ordering::SeqCst),
        1,
        "the in-flight fetch was dropped, releasing its image slot"
    );
    let stats = cache.stats().await.unwrap();
    assert_eq!(
        (
            stats.entry_count,
            stats.live_reservations,
            stats.total_charged_bytes
        ),
        (0, 0, 0)
    );

    // The same image loads normally afterwards.
    gate.add_permits(1);
    let loaded = load(&pipeline, &url, guest(1)).await.unwrap();
    assert_eq!(loaded.source, ImageSource::Network);
    assert_eq!(cache.stats().await.unwrap().entry_count, 1);
    cache.shutdown().await;
}

#[tokio::test]
async fn a_load_cancelled_before_it_starts_does_no_work() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(4, 4)));
    let pipeline = pipeline(&fetcher, &cache);
    let token = CancelToken::new();
    token.cancel();

    let err = pipeline
        .load(ImageRequest::thumbnail(url("never"), guest(1)), token)
        .await
        .unwrap_err();
    assert_eq!(err, ImagePipelineError::Cancelled);
    assert_eq!(fetcher.calls(), 0);
    cache.shutdown().await;
}

#[tokio::test]
async fn offline_cache_hits_are_reported_stale_without_a_fetch() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(4, 4)));
    let pipeline = pipeline(&fetcher, &cache);
    let url = url("offline");
    load(&pipeline, &url, guest(1)).await.unwrap();

    cache
        .set_connectivity(CacheConnectivity::Offline)
        .await
        .unwrap();
    let loaded = load(&pipeline, &url, guest(1)).await.unwrap();
    assert_eq!(loaded.source, ImageSource::Cache);
    assert!(loaded.stale);
    assert_eq!(fetcher.calls(), 1);
    cache.shutdown().await;
}

#[tokio::test]
async fn a_full_cache_does_not_stop_images_from_loading() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 10).await;
    let fetcher = FakeFetcher::new(Ok(png(16, 16)));
    let pipeline = pipeline(&fetcher, &cache);

    let loaded = load(&pipeline, &url("too-big-to-cache"), guest(1))
        .await
        .unwrap();
    assert_eq!(loaded.source, ImageSource::Network);
    assert_eq!(cache.stats().await.unwrap().entry_count, 0);
    cache.shutdown().await;
}

#[tokio::test]
async fn decoder_budget_exhaustion_does_not_discard_a_good_cached_entry() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(8, 6)));
    let url = url("budget");
    load(&pipeline(&fetcher, &cache), &url, guest(1))
        .await
        .unwrap();

    // A guard too small for this image fails the load but must not call the bytes corrupt.
    let tiny = ImageGuard::with_config(ImageGuardConfig {
        decode_concurrency: 1,
        decoded_bytes_budget: 16,
    });
    let strict = ImagePipeline::new(Arc::clone(&fetcher), cache.clone(), tiny);
    let err = load(&strict, &url, guest(1)).await.unwrap_err();
    assert!(
        matches!(
            err,
            ImagePipelineError::Decode(ImageGuardError::DecodedBudgetExceeded { .. })
        ),
        "{err:?}"
    );
    assert_eq!(fetcher.calls(), 1, "no refetch for a resource limit");
    assert_eq!(cache.stats().await.unwrap().entry_count, 1);
    cache.shutdown().await;
}

#[tokio::test]
async fn a_shut_down_cache_is_reported_not_papered_over() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(4, 4)));
    let pipeline = pipeline(&fetcher, &cache);
    cache.shutdown().await;

    let err = load(&pipeline, &url("late"), guest(1)).await.unwrap_err();
    assert_eq!(err, ImagePipelineError::Cache(CacheError::ShuttingDown));
    assert_eq!(fetcher.calls(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_loads_share_one_cache_without_leaking_charges() {
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(12, 12)));
    let pipeline = pipeline(&fetcher, &cache);

    let mut tasks = Vec::new();
    for i in 0..24 {
        let pipeline = pipeline.clone();
        let url = url(&format!("shared-{}", i % 6));
        tasks.push(tokio::spawn(async move {
            pipeline
                .load(ImageRequest::thumbnail(url, guest(1)), CancelToken::new())
                .await
        }));
    }
    for task in tasks {
        let loaded = task.await.unwrap().expect("load");
        assert_eq!(loaded.image.dimensions(), (12, 12));
    }
    let stats = cache.stats().await.unwrap();
    assert_eq!(stats.entry_count, 6);
    assert_eq!(stats.live_reservations, 0);
    assert!(stats.total_charged_bytes <= stats.cap_bytes);
    assert_eq!(
        pipeline.guard().available_decoded_bytes(),
        pipeline.guard().decoded_bytes_budget(),
        "every texture permit was released"
    );
    cache.shutdown().await;
}

#[tokio::test]
async fn queued_fetch_responses_are_consumed_in_order() {
    // Sanity check of the fake itself: a bad first answer then a good one.
    let dir = TempDir::new();
    let cache = open_cache(&dir, 1_000_000).await;
    let fetcher = FakeFetcher::new(Ok(png(3, 3)));
    fetcher.queue(Err(ProviderError::Timeout));
    let pipeline = pipeline(&fetcher, &cache);
    let url = url("queued");
    assert_eq!(
        load(&pipeline, &url, guest(1)).await.unwrap_err(),
        ImagePipelineError::Provider(ProviderError::Timeout)
    );
    assert!(load(&pipeline, &url, guest(1)).await.is_ok());
    cache.shutdown().await;
}

#[test]
fn thumbnail_url_rejects_unparseable_urls_and_accepts_valid_urls() {
    let err = ImageRequest::thumbnail_url("not a valid url", guest(1)).unwrap_err();
    assert_eq!(
        err,
        ImagePipelineError::Provider(ProviderError::OriginRejected)
    );

    let req = ImageRequest::thumbnail_url("https://th.wallhaven.cc/lg/ab/123.jpg", guest(1))
        .expect("valid url");
    assert_eq!(req.class, ImageClass::Thumbnail);
    assert_eq!(req.scope, guest(1));
}

#[tokio::test]
async fn load_uncached_decodes_valid_images_rejects_bombs_and_honors_cancellation() {
    let fetcher = FakeFetcher::new(Ok(png(8, 6)));
    let guard = ImageGuard::new();
    let url = url("uncached-ok");

    let loaded = load_uncached(
        fetcher.as_ref(),
        &guard,
        ImageRequest::thumbnail(url.clone(), guest(1)),
        CancelToken::new(),
    )
    .await
    .expect("uncached load succeeds");
    assert_eq!(loaded.source, ImageSource::Network);
    assert!(!loaded.stale);
    assert_eq!(loaded.image.dimensions(), (8, 6));
    assert_eq!(fetcher.calls(), 1);

    // Oversized dimensions are rejected before full decode.
    fetcher.set_fallback(Ok(png(1025, 1)));
    let err = load_uncached(
        fetcher.as_ref(),
        &guard,
        ImageRequest::thumbnail(url.clone(), guest(1)),
        CancelToken::new(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(
            err,
            ImagePipelineError::Decode(
                ImageGuardError::DimensionsExceeded { .. } | ImageGuardError::DecoderLimit(_)
            )
        ),
        "{err:?}"
    );

    // Pre-cancelled token never touches the fetcher.
    let before = fetcher.calls();
    let cancelled = CancelToken::new();
    cancelled.cancel();
    let err = load_uncached(
        fetcher.as_ref(),
        &guard,
        ImageRequest::thumbnail(url.clone(), guest(1)),
        cancelled,
    )
    .await
    .unwrap_err();
    assert_eq!(err, ImagePipelineError::Cancelled);
    assert_eq!(fetcher.calls(), before);

    // Mid-flight cancellation drops the in-flight fetch.
    let gated_fetcher = FakeFetcher::new(Ok(png(4, 4)));
    let gate = gated_fetcher.close_gate();
    let token = CancelToken::new();
    let task = {
        let fetcher = Arc::clone(&gated_fetcher);
        let guard = guard.clone();
        let token = token.clone();
        tokio::spawn(async move {
            load_uncached(
                fetcher.as_ref(),
                &guard,
                ImageRequest::thumbnail(url, guest(1)),
                token,
            )
            .await
        })
    };
    gated_fetcher.wait_until_entered().await;
    token.cancel();
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(err, ImagePipelineError::Cancelled);
    assert_eq!(gated_fetcher.dropped_in_flight.load(Ordering::SeqCst), 1);
    gate.add_permits(1);
}
