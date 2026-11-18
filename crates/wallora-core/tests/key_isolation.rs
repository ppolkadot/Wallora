use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tracing_subscriber::fmt::MakeWriter;
use url::Url;
use wallora_core::domain::{
    AccountId, CredentialStore, InMemoryCredentialStore, SearchQuery, SecretString, SessionScope,
    WallpaperId,
};
use wallora_core::provider::{
    ImageClass, OriginPolicy, Provider, ProviderError, WallhavenClient, WallhavenClientConfig,
    authorize,
};
use wiremock::matchers::{header, header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod support;

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

/// Builds a client whose API base is an unused loopback address and whose image origin is the
/// supplied loopback server.
fn test_client_for_image(addr: SocketAddr) -> WallhavenClient {
    let api_base = Url::parse("http://127.0.0.1:1/api/v1").expect("valid loopback base");
    let origin = Url::parse(&format!("http://{addr}")).expect("valid loopback origin");
    let config = WallhavenClientConfig::for_test(api_base)
        .expect("valid test config")
        .with_test_image_origin(origin)
        .expect("valid test image origin");
    WallhavenClient::with_config(Arc::new(InMemoryCredentialStore::new()), config)
        .expect("client build")
}

fn image_url(addr: SocketAddr, path: &str) -> Url {
    Url::parse(&format!("http://{addr}{path}")).expect("valid image url")
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
    let key = allowed
        .headers()
        .get("x-api-key")
        .expect("keyed request carries the header");
    assert_eq!(key.to_str().ok(), Some(SECRET_KEY));
    assert!(
        key.is_sensitive(),
        "API key header must be marked sensitive"
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
    let image_origin = Url::parse(&image_server.uri()).unwrap();
    let config = WallhavenClientConfig::for_test(api_base)
        .unwrap()
        .with_test_image_origin(image_origin)
        .unwrap();
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
        .fetch_image_bytes(&img_url, ImageClass::Thumbnail)
        .await
        .expect("image fetch should succeed without key");
    assert_eq!(bytes, b"IMGDATA");

    // Test image host cross-origin redirect rejected
    let cross_url = Url::parse(&format!("{}/full/ab/cross.jpg", image_server.uri())).unwrap();
    let cross_err = client
        .fetch_image_bytes(&cross_url, ImageClass::Thumbnail)
        .await
        .expect_err("cross-origin image redirect must be rejected");
    assert_eq!(cross_err, ProviderError::OriginRejected);

    // Test image host same-origin redirect allowed
    let same_url = Url::parse(&format!("{}/full/ab/same-orig.jpg", image_server.uri())).unwrap();
    let same_bytes = client
        .fetch_image_bytes(&same_url, ImageClass::Thumbnail)
        .await
        .expect("same-origin image redirect should succeed");
    assert_eq!(same_bytes, b"SAMEORIG");

    // Verify search query serialization also never embeds key.
    let _ = client.search(SearchQuery::default(), scope).await;
    for req in api_server.received_requests().await.unwrap_or_default() {
        assert!(
            !req.url.as_str().contains(SECRET_KEY),
            "secret key leaked into a request URL: {}",
            req.url
        );
        assert!(
            !req.url.as_str().contains("apikey"),
            "API key must never be sent as a query parameter: {}",
            req.url
        );
    }

    // Verify captured tracing logs NEVER contain the secret key
    let logs = log_buf.contents();
    assert!(
        !logs.contains(SECRET_KEY),
        "secret key leaked into tracing logs!"
    );
}

#[tokio::test]
async fn production_image_fetch_rejects_loopback_without_connecting() {
    let image_server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"IMG".as_slice()))
        .expect(0)
        .mount(&image_server)
        .await;

    let client = WallhavenClient::new(Arc::new(InMemoryCredentialStore::new())).expect("client");
    let url = Url::parse(&format!("{}/full/ab/x.jpg", image_server.uri())).unwrap();
    let err = client
        .fetch_image_bytes(&url, ImageClass::Thumbnail)
        .await
        .expect_err("production policy must reject loopback images");

    assert_eq!(err, ProviderError::OriginRejected);
    let received = image_server.received_requests().await.unwrap_or_default();
    assert!(
        received.is_empty(),
        "production client connected to loopback"
    );
}

#[tokio::test]
async fn image_body_limit_checks_content_length_before_read() {
    let (listener, addr) = support::bind().await;
    let _server = support::serve(listener, |mut stream| async move {
        let _ = support::read_request_head(&mut stream).await;
        let _ = support::write_head(
            &mut stream,
            &[
                ("Content-Type", "application/octet-stream"),
                ("Content-Length", "2000001"),
                ("Connection", "close"),
            ],
        )
        .await;
        // Withhold the body entirely; the client must reject from the declared length alone.
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    let client = test_client_for_image(addr);
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.fetch_image_bytes(
            &image_url(addr, "/thumb/oversized.jpg"),
            ImageClass::Thumbnail,
        ),
    )
    .await
    .expect("content-length rejection must not wait for the body");

    assert_eq!(
        result,
        Err(ProviderError::ImageTooLarge { limit: 2_000_000 })
    );
}

#[tokio::test]
async fn image_body_limit_checks_chunked_total() {
    let (listener, addr) = support::bind().await;
    let _server = support::serve(listener, |mut stream| async move {
        let head = support::read_request_head(&mut stream)
            .await
            .unwrap_or_default();
        let request_line = head.lines().next().unwrap_or_default().to_string();
        let over_by_one = request_line.contains("cross");
        let _ = support::write_head(
            &mut stream,
            &[
                ("Content-Type", "application/octet-stream"),
                ("Transfer-Encoding", "chunked"),
                ("Connection", "close"),
            ],
        )
        .await;
        let first = vec![1u8; 1_000_000];
        let second = vec![2u8; 1_000_000];
        let _ = support::write_chunk(&mut stream, &first).await;
        let _ = support::write_chunk(&mut stream, &second).await;
        if over_by_one {
            let _ = support::write_chunk(&mut stream, &[3u8]).await;
        }
        let _ = support::finish_chunks(&mut stream).await;
        let _ = stream.shutdown().await;
    });

    let client = test_client_for_image(addr);

    // Exactly the thumbnail ceiling succeeds.
    let exact = client
        .fetch_image_bytes(&image_url(addr, "/exact.jpg"), ImageClass::Thumbnail)
        .await
        .expect("an exact-limit body must succeed");
    assert_eq!(exact.len(), 2_000_000);

    // A later chunk crossing the ceiling by one byte is rejected.
    let over = client
        .fetch_image_bytes(&image_url(addr, "/cross.jpg"), ImageClass::Thumbnail)
        .await
        .expect_err("a body one byte over the thumbnail ceiling must fail");
    assert_eq!(over, ProviderError::ImageTooLarge { limit: 2_000_000 });

    // Preview accepts a body that exceeded the thumbnail ceiling.
    let preview = client
        .fetch_image_bytes(&image_url(addr, "/cross.jpg"), ImageClass::Preview)
        .await
        .expect("preview accepts the same body");
    assert_eq!(preview.len(), 2_000_001);
}

#[tokio::test]
async fn image_host_enforces_in_flight_cap_of_6() {
    let (listener, addr) = support::bind().await;
    let active = Arc::new(AtomicUsize::new(0));
    let max_active = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let heads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (arrivals_tx, mut arrivals_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let server_active = Arc::clone(&active);
    let server_max = Arc::clone(&max_active);
    let server_gate = Arc::clone(&gate);
    let server_heads = Arc::clone(&heads);
    let _server = support::serve(listener, move |mut stream| {
        let active = Arc::clone(&server_active);
        let max_active = Arc::clone(&server_max);
        let gate = Arc::clone(&server_gate);
        let heads = Arc::clone(&server_heads);
        let arrivals_tx = arrivals_tx.clone();
        async move {
            let head = support::read_request_head(&mut stream)
                .await
                .unwrap_or_default();
            heads.lock().expect("heads lock").push(head);
            let current = active.fetch_add(1, Ordering::SeqCst) + 1;
            max_active.fetch_max(current, Ordering::SeqCst);
            let _ = arrivals_tx.send(());

            let wrote_head = support::write_head(
                &mut stream,
                &[
                    ("Content-Type", "application/octet-stream"),
                    ("Content-Length", "2"),
                    ("Connection", "close"),
                ],
            )
            .await
            .is_ok();
            if wrote_head && stream.write_all(b"O").await.is_ok() && stream.flush().await.is_ok() {
                // Hold the final byte until the test releases this connection. The gate permit
                // is forgotten so releasing one connection never releases the next waiter.
                if let Ok(permit) = gate.acquire().await {
                    permit.forget();
                    // Leave the concurrency count before the client can free its semaphore slot.
                    active.fetch_sub(1, Ordering::SeqCst);
                    let _ = stream.write_all(b"K").await;
                    let _ = stream.flush().await;
                    let _ = stream.shutdown().await;
                } else {
                    active.fetch_sub(1, Ordering::SeqCst);
                }
            } else {
                active.fetch_sub(1, Ordering::SeqCst);
            }
        }
    });

    let client = Arc::new(test_client_for_image(addr));
    let mut handles = Vec::new();
    for i in 0..12 {
        let c = Arc::clone(&client);
        let url = image_url(addr, &format!("/img/{i}.jpg"));
        handles.push(tokio::spawn(async move {
            c.fetch_image_bytes(&url, ImageClass::Thumbnail).await
        }));
    }

    for _ in 0..6 {
        tokio::time::timeout(Duration::from_secs(5), arrivals_rx.recv())
            .await
            .expect("six image requests must reach the host")
            .expect("arrival signal");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(400), arrivals_rx.recv())
            .await
            .is_err(),
        "more than six image requests reached the host"
    );

    // Release exactly one held body: exactly one replacement may reach the host.
    gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), arrivals_rx.recv())
        .await
        .expect("exactly one replacement must reach the host")
        .expect("arrival signal");
    assert!(
        tokio::time::timeout(Duration::from_millis(400), arrivals_rx.recv())
            .await
            .is_err(),
        "more than one replacement reached the host"
    );

    // Drain the rest.
    gate.add_permits(12);
    for handle in handles {
        let body = tokio::time::timeout(Duration::from_secs(20), handle)
            .await
            .expect("image call resolves")
            .expect("task join")
            .expect("image call succeeds");
        assert_eq!(body, b"OK");
    }

    assert_eq!(
        max_active.load(Ordering::SeqCst),
        6,
        "image in-flight cap must be exactly 6"
    );
    for head in heads.lock().expect("heads lock").iter() {
        assert!(
            !head.to_ascii_lowercase().contains("x-api-key"),
            "image request carried an API key header"
        );
    }
}
