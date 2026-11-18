use std::io::Write;
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;
use url::Url;
use wallora_core::domain::{
    AccountId, CredentialStore, InMemoryCredentialStore, SearchQuery, SecretString, SessionScope,
    WallpaperId,
};
use wallora_core::provider::{
    OriginPolicy, Provider, ProviderError, WallhavenClient, WallhavenClientConfig, authorize,
};
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET_KEY: &str = "wh_super_secret_test_key_do_not_leak_987654";
const FIXTURE_WALLPAPER_SFW: &str = include_str!("fixtures/wallpaper_sfw.json");

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn contents(&self) -> String {
        let guard = self.0.lock().expect("log lock");
        String::from_utf8_lossy(&guard).into_owned()
    }
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self.0.lock().expect("log lock");
        guard.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(Arc::clone(&self.0))
    }
}

fn store_with_secret() -> (Arc<InMemoryCredentialStore>, AccountId, SessionScope) {
    let creds = Arc::new(InMemoryCredentialStore::new());
    let account = AccountId::new("acct-iso-1");
    creds
        .save(&account, &SecretString::new(SECRET_KEY))
        .expect("save key");
    let scope = SessionScope::account(account.clone(), 1);
    (creds, account, scope)
}

#[test]
fn secret_string_redacts_debug_and_display() {
    let secret = SecretString::new(SECRET_KEY);
    let dbg = format!("{secret:?}");
    let disp = format!("{secret}");
    assert_eq!(dbg, "***");
    assert_eq!(disp, "***");
    assert!(!dbg.contains(SECRET_KEY));
    assert!(!disp.contains(SECRET_KEY));
}

#[test]
fn origin_policy_authorizes_only_exact_https_wallhaven_443_api_v1() {
    let (creds, _acct, scope) = store_with_secret();
    let policy = OriginPolicy::production();

    // Allowed: https://wallhaven.cc/api/v1/* (default port 443)
    let mut allowed = reqwest::Request::new(
        reqwest::Method::GET,
        Url::parse("https://wallhaven.cc/api/v1/search?q=nature").unwrap(),
    );
    authorize(&mut allowed, &scope, creds.as_ref(), &policy).expect("exact API origin allowed");
    assert_eq!(
        allowed
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some(SECRET_KEY)
    );

    // Guest scope never attaches X-API-Key even on allowed origin
    let mut guest_req = reqwest::Request::new(
        reqwest::Method::GET,
        Url::parse("https://wallhaven.cc/api/v1/search").unwrap(),
    );
    authorize(
        &mut guest_req,
        &SessionScope::guest(1),
        creds.as_ref(),
        &policy,
    )
    .expect("guest origin allowed");
    assert!(guest_req.headers().get("x-api-key").is_none());

    // Rejected: non-443 port (8443), plain http, image hosts, non-/api/v1/ paths
    let rejected_urls = [
        "https://wallhaven.cc:8443/api/v1/search",
        "http://wallhaven.cc/api/v1/search",
        "https://w.wallhaven.cc/full/ab/wallhaven-abc123.jpg",
        "https://th.wallhaven.cc/small/ab/abc123.jpg",
        "https://wallhaven.cc/w/abc123",
        "https://evil.wallhaven.cc/api/v1/search",
    ];

    for raw in rejected_urls {
        let mut req = reqwest::Request::new(reqwest::Method::GET, Url::parse(raw).unwrap());
        let res = authorize(&mut req, &scope, creds.as_ref(), &policy);
        assert_eq!(res, Err(ProviderError::OriginRejected), "url={raw}");
        assert!(
            req.headers().get("x-api-key").is_none(),
            "key leaked onto rejected url {raw}"
        );
    }
}

#[tokio::test]
async fn client_with_production_policy_rejects_port_8443_and_http_before_network() {
    let (creds, _acct, scope) = store_with_secret();

    for bad_base in [
        "https://wallhaven.cc:8443/api/v1",
        "http://wallhaven.cc/api/v1",
    ] {
        let mut config = WallhavenClientConfig::production();
        config.api_base = Url::parse(bad_base).unwrap();
        let client = WallhavenClient::with_config(creds.clone(), config).expect("build client");

        let err = client
            .wallpaper(WallpaperId::new("abc123"), scope.clone())
            .await
            .expect_err("non-production origin must be rejected");
        assert_eq!(err, ProviderError::OriginRejected);
        assert!(!format!("{err}").contains(SECRET_KEY));
        assert!(!format!("{err:?}").contains(SECRET_KEY));
    }
}

#[tokio::test]
async fn wiremock_key_sent_to_api_absent_on_images_and_redirects_blocked() {
    let log_buf = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(log_buf.clone())
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let api_server = MockServer::start().await;
    let image_server = MockServer::start().await;
    let evil_server = MockServer::start().await;

    // Evil target must NEVER receive any request from either API 3xx or image cross-origin 3xx.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&evil_server)
        .await;

    // 1. Authenticated API call sends X-API-Key header (not in URL).
    Mock::given(method("GET"))
        .and(path("/api/v1/w/abc123"))
        .and(header("x-api-key", SECRET_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_WALLPAPER_SFW))
        .expect(1)
        .mount(&api_server)
        .await;

    // 2. API 3xx redirect to another origin is NOT followed.
    Mock::given(method("GET"))
        .and(path("/api/v1/w/redir302"))
        .and(header("x-api-key", SECRET_KEY))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", &format!("{}/steal", evil_server.uri())),
        )
        .expect(1)
        .mount(&api_server)
        .await;

    // 3. API 401 still never leaks secret key into error or logs.
    Mock::given(method("GET"))
        .and(path("/api/v1/w/fail401"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&api_server)
        .await;

    // 4. Image host must NEVER receive X-API-Key.
    Mock::given(method("GET"))
        .and(path("/full/ab/wallhaven-abc123.jpg"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"IMGDATA".as_slice()))
        .expect(1)
        .mount(&image_server)
        .await;

    // Ensure no request with x-api-key ever hits image_server.
    Mock::given(header_exists("x-api-key"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&image_server)
        .await;

    // 5. Image host cross-origin redirect rejected without contacting target.
    Mock::given(method("GET"))
        .and(path("/full/ab/cross.jpg"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", &format!("{}/cross.jpg", evil_server.uri())),
        )
        .expect(1)
        .mount(&image_server)
        .await;

    // 6. Image host same-origin redirect is allowed.
    Mock::given(method("GET"))
        .and(path("/full/ab/same-orig.jpg"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("Location", &format!("{}/final.jpg", image_server.uri())),
        )
        .expect(1)
        .mount(&image_server)
        .await;
    Mock::given(method("GET"))
        .and(path("/final.jpg"))
        .and(|req: &wiremock::Request| !req.headers.contains_key("x-api-key"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"SAMEORIG".as_slice()))
        .expect(1)
        .mount(&image_server)
        .await;

    let (creds, _acct, scope) = store_with_secret();
    let api_base = Url::parse(&format!("{}/api/v1", api_server.uri())).unwrap();
    let config = WallhavenClientConfig::for_test(api_base).unwrap();
    let client = WallhavenClient::with_config(creds, config).unwrap();

    // Test authenticated API call
    let wp = client
        .wallpaper(WallpaperId::new("abc123"), scope.clone())
        .await
        .expect("API call with key should succeed");
    assert_eq!(wp.id.as_str(), "abc123");

    // Verify no URL in received requests contains the secret
    for req in api_server.received_requests().await.unwrap_or_default() {
        assert!(!req.url.as_str().contains(SECRET_KEY));
    }

    // Test API 3xx does not follow redirect
    let redir_err = client
        .wallpaper(WallpaperId::new("redir302"), scope.clone())
        .await
        .expect_err("API 3xx must not follow redirect");
    assert_eq!(redir_err, ProviderError::Upstream { status: 302 });

    // Test API 401 does not leak secret in error Display/Debug
    let err401 = client
        .wallpaper(WallpaperId::new("fail401"), scope.clone())
        .await
        .expect_err("401 must fail");
    assert_eq!(err401, ProviderError::Unauthorized);
    assert!(!format!("{err401}").contains(SECRET_KEY));
    assert!(!format!("{err401:?}").contains(SECRET_KEY));

    // Test image host fetch has no key
    let img_url = Url::parse(&format!(
        "{}/full/ab/wallhaven-abc123.jpg",
        image_server.uri()
    ))
    .unwrap();
    let bytes = client
        .fetch_image_bytes(&img_url)
        .await
        .expect("image fetch should succeed without key");
    assert_eq!(bytes, b"IMGDATA");

    // Test image host cross-origin redirect rejected
    let cross_url = Url::parse(&format!("{}/full/ab/cross.jpg", image_server.uri())).unwrap();
    let cross_err = client
        .fetch_image_bytes(&cross_url)
        .await
        .expect_err("cross-origin image redirect must be rejected");
    assert_eq!(cross_err, ProviderError::OriginRejected);

    // Test image host same-origin redirect allowed
    let same_url = Url::parse(&format!("{}/full/ab/same-orig.jpg", image_server.uri())).unwrap();
    let same_bytes = client
        .fetch_image_bytes(&same_url)
        .await
        .expect("same-origin image redirect should succeed");
    assert_eq!(same_bytes, b"SAMEORIG");

    // Verify search query serialization also never embeds key
    let _ = client.search(SearchQuery::default(), scope).await;

    // Verify captured tracing logs NEVER contain the secret key
    let logs = log_buf.contents();
    assert!(
        !logs.contains(SECRET_KEY),
        "secret key leaked into tracing logs!"
    );
}

#[tokio::test]
async fn image_host_enforces_in_flight_cap_of_6() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use wiremock::{Request, Respond};

    struct ConcurrencyProbe {
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl Respond for ConcurrencyProbe {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let cur = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(cur, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(80));
            self.active.fetch_sub(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_bytes(b"OK".as_slice())
        }
    }

    let image_server = MockServer::start().await;
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));

    Mock::given(method("GET"))
        .respond_with(ConcurrencyProbe {
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
        })
        .mount(&image_server)
        .await;

    let creds = Arc::new(InMemoryCredentialStore::new());
    let client = WallhavenClient::new(creds).unwrap();

    let mut handles = Vec::new();
    for i in 0..12 {
        let c = client.clone();
        let url = Url::parse(&format!("{}/img/{i}.jpg", image_server.uri())).unwrap();
        handles.push(tokio::spawn(async move { c.fetch_image_bytes(&url).await }));
    }

    for h in handles {
        assert_eq!(h.await.unwrap().unwrap(), b"OK");
    }

    assert!(
        max_active.load(Ordering::SeqCst) <= 6,
        "image in-flight cap 6 exceeded: {}",
        max_active.load(Ordering::SeqCst)
    );
}
