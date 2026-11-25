//! The pipeline wired to the real `WallhavenClient` image transport against a loopback origin.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use image::{ImageFormat, Rgba, RgbaImage};
use url::Url;
use wallora_core::domain::{
    AccountId, CredentialStore, InMemoryCredentialStore, SecretString, SessionScope,
};
use wallora_core::provider::{ProviderError, WallhavenClient, WallhavenClientConfig};
use wallora_core::services::cache::{CacheConfig, CacheStore, CancelToken};
use wallora_core::services::{
    ImageGuard, ImagePipeline, ImagePipelineError, ImageRequest, ImageSource,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET_KEY: &str = "wh_synthetic_pipeline_secret";

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = RgbaImage::from_pixel(width, height, Rgba([10, 120, 200, 255]));
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    out
}

fn client_for(server: &MockServer) -> (Arc<WallhavenClient>, SessionScope) {
    let creds = Arc::new(InMemoryCredentialStore::new());
    let account = AccountId::generate();
    creds
        .save(&account, &SecretString::new(SECRET_KEY))
        .unwrap();
    let api_base = Url::parse(&format!("{}/api/v1", server.uri())).unwrap();
    let config = WallhavenClientConfig::for_test(api_base)
        .unwrap()
        .with_test_image_origin(Url::parse(&server.uri()).unwrap())
        .unwrap();
    let client = WallhavenClient::with_config(creds as Arc<dyn CredentialStore>, config).unwrap();
    (Arc::new(client), SessionScope::account(account, 1))
}

async fn open_cache(dir: &TempDir, scope: SessionScope) -> CacheStore {
    CacheStore::open(
        CacheConfig::new(dir.0.join("cache"), scope)
            .with_cap_bytes(1_000_000)
            .unwrap(),
    )
    .await
    .unwrap()
}

fn temp_dir() -> TempDir {
    let dir = std::env::temp_dir().join(format!("wallora-loopback-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    TempDir(dir)
}

#[tokio::test]
async fn real_client_fetches_once_caches_and_never_sends_credentials_or_referer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/small/ab/real.png"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png(9, 9)))
        .mount(&server)
        .await;
    let (client, scope) = client_for(&server);
    let dir = temp_dir();
    let cache = open_cache(&dir, scope.clone()).await;
    let pipeline = ImagePipeline::new(client, cache.clone(), ImageGuard::new());
    let url = Url::parse(&format!("{}/small/ab/real.png", server.uri())).unwrap();

    let first = pipeline
        .load(
            ImageRequest::thumbnail(url.clone(), scope.clone()),
            CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(first.source, ImageSource::Network);
    assert_eq!(first.image.dimensions(), (9, 9));
    let second = pipeline
        .load(
            ImageRequest::thumbnail(url.clone(), scope.clone()),
            CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(second.source, ImageSource::Cache);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "the second load came from the cache");
    for request in &requests {
        assert!(request.headers.get("x-api-key").is_none());
        assert!(request.headers.get("referer").is_none());
        assert!(!request.url.as_str().contains(SECRET_KEY));
    }
    cache.shutdown().await;
}

#[tokio::test]
async fn upstream_errors_and_blocked_origins_surface_without_caching() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/small/ab/missing.png"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let (client, scope) = client_for(&server);
    let dir = temp_dir();
    let cache = open_cache(&dir, scope.clone()).await;
    let pipeline = ImagePipeline::new(client, cache.clone(), ImageGuard::new());

    let missing = Url::parse(&format!("{}/small/ab/missing.png", server.uri())).unwrap();
    let err = pipeline
        .load(
            ImageRequest::thumbnail(missing, scope.clone()),
            CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, ImagePipelineError::Provider(ProviderError::NotFound));

    // A host outside the allowlist is refused by the client before any request is made.
    let foreign = Url::parse("https://example.com/small/ab/evil.png").unwrap();
    let err = pipeline
        .load(ImageRequest::thumbnail(foreign, scope), CancelToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        ImagePipelineError::Provider(ProviderError::OriginRejected)
    );
    assert_eq!(cache.stats().await.unwrap().entry_count, 0);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    cache.shutdown().await;
}
