use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use url::Url;
use wallora_core::domain::{
    AccountId, Category, CredentialStore, InMemoryCredentialStore, Purity, QueryEcho,
    RequestPriority, SearchQuery, SecretString, SessionScope, WallpaperId,
};
use wallora_core::provider::{
    Provider, ProviderError, WALLORA_USER_AGENT, WallhavenClient, WallhavenClientConfig,
};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const FIXTURE_WALLPAPER_SFW: &str = include_str!("fixtures/wallpaper_sfw.json");
const FIXTURE_WALLPAPER_MINIMAL_EXTRA: &str = include_str!("fixtures/wallpaper_minimal_extra.json");
const FIXTURE_SEARCH_PAGE: &str = include_str!("fixtures/search_page.json");
const FIXTURE_SEARCH_TAG_META: &str = include_str!("fixtures/search_tag_meta.json");
const FIXTURE_SEARCH_RANDOM_META: &str = include_str!("fixtures/search_random_meta.json");
const FIXTURE_ERROR_UNAUTHORIZED: &str = include_str!("fixtures/error_unauthorized.json");
const COALESCE_KEY: &str = "wh_coalesce_scope_key_12345";

fn assert_static_send<F: Future + Send + 'static>(_f: F) {}

fn test_client(server: &MockServer) -> WallhavenClient {
    let base = Url::parse(&format!("{}/api/v1", server.uri())).expect("valid mock URI");
    let creds = Arc::new(InMemoryCredentialStore::new());
    let config = WallhavenClientConfig::for_test(base).expect("valid test config");
    WallhavenClient::with_config(creds, config).expect("client build should succeed")
}

fn test_client_with_timeout(server: &MockServer, request_timeout: Duration) -> WallhavenClient {
    let base = Url::parse(&format!("{}/api/v1", server.uri())).expect("valid mock URI");
    let creds = Arc::new(InMemoryCredentialStore::new());
    let mut config = WallhavenClientConfig::for_test(base).expect("valid test config");
    config.request_timeout = request_timeout;
    WallhavenClient::with_config(creds, config).expect("client build should succeed")
}

/// Builds a keyed client plus the account id used for both scopes and requests.
fn keyed_client(server: &MockServer, account_name: &str) -> (WallhavenClient, AccountId) {
    let creds = Arc::new(InMemoryCredentialStore::new());
    let account = AccountId::new(account_name);
    creds
        .save(&account, &SecretString::new(COALESCE_KEY))
        .expect("save key");
    let base = Url::parse(&format!("{}/api/v1", server.uri())).expect("valid mock URI");
    let config = WallhavenClientConfig::for_test(base).expect("valid test config");
    let client = WallhavenClient::with_config(creds, config).expect("client build");
    (client, account)
}

fn count_query_requests(requests: &[Request], needle: &str) -> usize {
    requests
        .iter()
        .filter(|req| {
            req.url
                .query_pairs()
                .any(|(key, value)| key == "q" && value == needle)
        })
        .count()
}

#[tokio::test]
async fn decodes_full_wallpaper_fixture_and_sends_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/w/abc123"))
        .and(header("user-agent", WALLORA_USER_AGENT))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_WALLPAPER_SFW))
        .expect(1)
        .mount(&server)
        .await;

    let client = test_client(&server);
    let fut = client.wallpaper(WallpaperId::new("abc123"), SessionScope::guest(1));
    assert_static_send(fut);

    let wp = client
        .wallpaper(WallpaperId::new("abc123"), SessionScope::guest(1))
        .await
        .expect("full wallpaper fixture should decode");

    assert_eq!(wp.id.as_str(), "abc123");
    assert_eq!(wp.purity, Purity::Sfw);
    assert_eq!(wp.category, Category::General);
    assert_eq!(wp.dimension_x, 1920);
    assert_eq!(wp.dimension_y, 1080);
    assert_eq!(wp.file_size, 524_288);
    assert_eq!(wp.tags.len(), 1);
    assert_eq!(wp.tags[0].name, "landscape");
    assert_eq!(wp.tags[0].alias, "");
    assert_eq!(
        wp.uploader.as_ref().map(|u| u.username.as_str()),
        Some("example_user")
    );
}

#[tokio::test]
async fn decodes_minimal_wallpaper_with_missing_optionals_and_unknown_extra_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/w/min001"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_WALLPAPER_MINIMAL_EXTRA))
        .expect(1)
        .mount(&server)
        .await;

    let client = test_client(&server);
    let wp = client
        .wallpaper(WallpaperId::new("min001"), SessionScope::guest(1))
        .await
        .expect("minimal fixture with unknown fields should decode");

    assert_eq!(wp.id.as_str(), "min001");
    assert_eq!(wp.short_url, None);
    assert_eq!(wp.uploader, None);
    assert_eq!(wp.views, 0);
    assert_eq!(wp.favorites, 0);
    assert_eq!(wp.source, "");
    assert!(wp.tags.is_empty());
}

#[tokio::test]
async fn decodes_search_meta_variants_text_tag_null_and_seed() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("q", "nature"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SEARCH_PAGE))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("q", "id:1"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SEARCH_TAG_META))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SEARCH_RANDOM_META))
        .expect(1)
        .mount(&server)
        .await;

    let client = test_client(&server);

    let text_page = client
        .search(
            SearchQuery {
                q: Some("nature".to_string()),
                ..Default::default()
            },
            SessionScope::guest(1),
        )
        .await
        .expect("text search should decode");
    assert_eq!(text_page.data.len(), 2);
    assert_eq!(text_page.meta.per_page, 24);
    assert_eq!(
        text_page.meta.query,
        Some(QueryEcho::Text("nature".to_string()))
    );
    assert_eq!(text_page.meta.seed, None);

    let tag_page = client
        .search(
            SearchQuery {
                q: Some("id:1".to_string()),
                ..Default::default()
            },
            SessionScope::guest(1),
        )
        .await
        .expect("tag search should decode");
    assert_eq!(
        tag_page.meta.query,
        Some(QueryEcho::Tag {
            id: 1,
            tag: "anime".to_string()
        })
    );

    let random_page = client
        .search(
            SearchQuery {
                page: Some(2),
                ..Default::default()
            },
            SessionScope::guest(1),
        )
        .await
        .expect("random meta search should decode");
    assert_eq!(random_page.meta.query, None);
    assert_eq!(random_page.meta.seed.as_deref(), Some("seed42xyz"));
    assert!(random_page.data.is_empty());
}

#[tokio::test]
async fn keyed_search_accepts_string_per_page_and_null_source() {
    let body = serde_json::json!({
        "data": [{
            "id": "str001",
            "url": "https://wallhaven.cc/w/str001",
            "short_url": null,
            "views": 7,
            "favorites": 2,
            "source": null,
            "purity": "sfw",
            "category": "general",
            "dimension_x": 1920,
            "dimension_y": 1080,
            "resolution": "1920x1080",
            "ratio": "1.78",
            "file_size": 2048,
            "file_type": "image/jpeg",
            "created_at": "2026-01-01 00:00:00",
            "colors": [],
            "path": "https://w.wallhaven.cc/full/st/wallhaven-str001.jpg",
            "thumbs": {
                "large": "https://th.wallhaven.cc/lg/st/str001.jpg",
                "original": "https://th.wallhaven.cc/orig/st/str001.jpg",
                "small": "https://th.wallhaven.cc/small/st/str001.jpg"
            }
        }],
        "meta": {
            "current_page": 1,
            "last_page": 2,
            "per_page": "48",
            "total": 1,
            "query": "stringly",
            "seed": null
        }
    })
    .to_string();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("q", "stringly"))
        .and(header("x-api-key", COALESCE_KEY))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    let (client, account) = keyed_client(&server, "acct-stringly");
    let page = client
        .search(
            SearchQuery {
                q: Some("stringly".to_string()),
                ..Default::default()
            },
            SessionScope::account(account, 1),
        )
        .await
        .expect("string per_page and null source must decode");

    assert_eq!(page.meta.per_page, 48);
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.data[0].id.as_str(), "str001");
    assert_eq!(page.data[0].source, "");
    assert_eq!(page.data[0].short_url, None);
}

#[tokio::test]
async fn wallpaper_accepts_null_tag_alias() {
    let body = serde_json::json!({
        "data": {
            "id": "aliasing",
            "url": "https://wallhaven.cc/w/aliasing",
            "short_url": null,
            "views": 3,
            "favorites": 1,
            "source": "",
            "purity": "sfw",
            "category": "general",
            "dimension_x": 1920,
            "dimension_y": 1080,
            "resolution": "1920x1080",
            "ratio": "1.78",
            "file_size": 4096,
            "file_type": "image/jpeg",
            "created_at": "2026-01-02 00:00:00",
            "colors": [],
            "path": "https://w.wallhaven.cc/full/al/wallhaven-aliasing.jpg",
            "thumbs": {
                "large": "https://th.wallhaven.cc/lg/al/aliasing.jpg",
                "original": "https://th.wallhaven.cc/orig/al/aliasing.jpg",
                "small": "https://th.wallhaven.cc/small/al/aliasing.jpg"
            },
            "tags": [
                {
                    "id": 7,
                    "name": "city",
                    "alias": null,
                    "category_id": 2,
                    "category": "Places",
                    "purity": "sfw",
                    "created_at": "2020-01-01 00:00:00"
                },
                {
                    "id": 8,
                    "name": "night",
                    "alias": "nightscape",
                    "category_id": 2,
                    "category": "Places",
                    "purity": "sfw",
                    "created_at": "2020-01-01 00:00:00"
                }
            ]
        }
    })
    .to_string();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/w/aliasing"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .expect(1)
        .mount(&server)
        .await;

    let client = test_client(&server);
    let wp = client
        .wallpaper(WallpaperId::new("aliasing"), SessionScope::guest(1))
        .await
        .expect("null tag alias must decode");

    assert_eq!(wp.tags.len(), 2);
    assert_eq!(wp.tags[0].alias, "");
    assert_eq!(wp.tags[1].alias, "nightscape");
}

#[tokio::test]
async fn malformed_json_returns_decode_error_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/w/badjson"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{not valid json"))
        .expect(1)
        .mount(&server)
        .await;

    let client = test_client(&server);
    let err = client
        .wallpaper(WallpaperId::new("badjson"), SessionScope::guest(1))
        .await
        .expect_err("malformed JSON must return Decode error");

    assert!(matches!(err, ProviderError::Decode { .. }));
}

#[tokio::test]
async fn client_errors_401_403_404_are_never_retried() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/api/v1/w/unauth"))
        .respond_with(ResponseTemplate::new(401).set_body_string(FIXTURE_ERROR_UNAUTHORIZED))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/w/forbid"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/w/missing"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;

    let client = test_client(&server);
    assert_eq!(
        client
            .wallpaper(WallpaperId::new("unauth"), SessionScope::guest(1))
            .await,
        Err(ProviderError::Unauthorized)
    );
    assert_eq!(
        client
            .wallpaper(WallpaperId::new("forbid"), SessionScope::guest(1))
            .await,
        Err(ProviderError::Forbidden)
    );
    assert_eq!(
        client
            .wallpaper(WallpaperId::new("missing"), SessionScope::guest(1))
            .await,
        Err(ProviderError::NotFound)
    );
}

#[tokio::test]
async fn five_xx_retries_at_most_twice_and_succeeds_on_third_attempt() {
    struct FlakyResponder(AtomicUsize);
    impl Respond for FlakyResponder {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            if n < 2 {
                ResponseTemplate::new(503)
            } else {
                ResponseTemplate::new(200).set_body_string(FIXTURE_WALLPAPER_SFW)
            }
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/w/abc123"))
        .respond_with(FlakyResponder(AtomicUsize::new(0)))
        .expect(3)
        .mount(&server)
        .await;

    let client = test_client(&server);
    let wp = client
        .wallpaper(WallpaperId::new("abc123"), SessionScope::guest(1))
        .await
        .expect("third attempt should succeed");
    assert_eq!(wp.id.as_str(), "abc123");
}

#[tokio::test]
async fn persistent_500_stops_after_max_two_retries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/w/boom"))
        .respond_with(ResponseTemplate::new(500))
        .expect(3)
        .mount(&server)
        .await;

    let client = test_client(&server);
    let err = client
        .wallpaper(WallpaperId::new("boom"), SessionScope::guest(1))
        .await
        .expect_err("persistent 500 must fail after 3 attempts");
    assert_eq!(err, ProviderError::Upstream { status: 500 });
}

#[tokio::test]
async fn request_timeout_maps_to_timeout_error_after_retries() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/w/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(400)))
        .expect(3)
        .mount(&server)
        .await;

    let client = test_client_with_timeout(&server, Duration::from_millis(80));
    let err = client
        .wallpaper(WallpaperId::new("slow"), SessionScope::guest(1))
        .await
        .expect_err("slow endpoint must time out");
    assert_eq!(err, ProviderError::Timeout);
}

#[tokio::test]
async fn rate_limit_429_with_retry_after_triggers_cooldown_and_drops_prefetch() {
    struct RateLimitThenOk(AtomicUsize);
    impl Respond for RateLimitThenOk {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                ResponseTemplate::new(429).insert_header("Retry-After", "3")
            } else {
                ResponseTemplate::new(200).set_body_string(FIXTURE_SEARCH_PAGE)
            }
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("q", "interactive"))
        .respond_with(RateLimitThenOk(AtomicUsize::new(0)))
        .expect(2)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("q", "prefetch"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SEARCH_PAGE))
        .expect(0)
        .mount(&server)
        .await;

    let client = test_client(&server);
    let interactive_fut = tokio::spawn({
        let c = client.clone();
        async move {
            c.search(
                SearchQuery {
                    q: Some("interactive".to_string()),
                    priority: RequestPriority::Interactive,
                    ..Default::default()
                },
                SessionScope::guest(1),
            )
            .await
        }
    });

    // Observe the applied cooldown instead of sleeping a fixed readiness delay.
    let remaining = tokio::time::timeout(
        Duration::from_secs(10),
        client.wait_for_api_cooldown_for_test(),
    )
    .await
    .expect("a 429 with Retry-After must activate the shared cooldown");

    assert!(
        remaining > Duration::from_secs(1) && remaining <= Duration::from_secs(3),
        "expected the observed 3s cooldown, got {remaining:?}"
    );

    let prefetch_res = client
        .search(
            SearchQuery {
                q: Some("prefetch".to_string()),
                priority: RequestPriority::Prefetch,
                ..Default::default()
            },
            SessionScope::guest(1),
        )
        .await;

    assert!(
        matches!(prefetch_res, Err(ProviderError::RateLimited { .. })),
        "prefetch during 429 cooldown must be dropped: {prefetch_res:?}"
    );

    let page = tokio::time::timeout(Duration::from_secs(30), interactive_fut)
        .await
        .expect("interactive retry must finish")
        .expect("task join")
        .expect("interactive request should succeed after Retry-After retry");
    assert_eq!(page.data.len(), 2);

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        count_query_requests(&requests, "interactive"),
        2,
        "the interactive request must be attempted exactly twice"
    );
    assert_eq!(
        count_query_requests(&requests, "prefetch"),
        0,
        "the dropped prefetch must never reach upstream"
    );
}

#[tokio::test]
async fn rate_limit_429_without_retry_after_defaults_to_60s_cooldown_and_drops_prefetch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("q", "first"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .and(query_param("q", "prefetch2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SEARCH_PAGE))
        .expect(0)
        .mount(&server)
        .await;

    let (client, account) = keyed_client(&server, "acct-cooldown-default");

    let first_task = tokio::spawn({
        let c = client.clone();
        let acct = account.clone();
        async move {
            c.search(
                SearchQuery {
                    q: Some("first".to_string()),
                    priority: RequestPriority::Visible,
                    ..Default::default()
                },
                SessionScope::account(acct, 1),
            )
            .await
        }
    });

    let remaining = tokio::time::timeout(
        Duration::from_secs(10),
        client.wait_for_api_cooldown_for_test(),
    )
    .await
    .expect("a 429 without Retry-After must activate the default cooldown");

    assert!(
        remaining >= Duration::from_secs(55) && remaining <= Duration::from_secs(60),
        "expected the ~60s default cooldown, got {remaining:?}"
    );

    let prefetch_err = client
        .search(
            SearchQuery {
                q: Some("prefetch2".to_string()),
                priority: RequestPriority::Prefetch,
                ..Default::default()
            },
            SessionScope::account(account, 1),
        )
        .await
        .expect_err("prefetch during 60s default cooldown must be dropped");

    match prefetch_err {
        ProviderError::RateLimited {
            retry_after: Some(d),
        } => {
            assert!(
                d >= Duration::from_secs(55) && d <= Duration::from_secs(60),
                "expected ~60s default cooldown, got {d:?}"
            );
        }
        other => panic!("unexpected error for dropped prefetch: {other:?}"),
    }

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        count_query_requests(&requests, "prefetch2"),
        0,
        "the dropped prefetch must never reach upstream"
    );

    first_task.abort();
    let aborted = first_task.await;
    assert!(aborted.is_err(), "the caller was aborted by the test");
}

#[tokio::test]
async fn separate_session_scopes_do_not_coalesce() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SEARCH_PAGE))
        .expect(4)
        .mount(&server)
        .await;

    let (client, account) = keyed_client(&server, "acct-scope-isolation");

    // The primer starts the shared pacer clock so the three admissions below stay queued.
    client
        .search(SearchQuery::default(), SessionScope::guest(0))
        .await
        .expect("primer search should succeed");

    let mut guest = Box::pin(client.search(SearchQuery::default(), SessionScope::guest(1)));
    let mut account_generation_one = Box::pin(client.search(
        SearchQuery::default(),
        SessionScope::account(account.clone(), 1),
    ));
    let mut account_generation_two = Box::pin(client.search(
        SearchQuery::default(),
        SessionScope::account(account.clone(), 2),
    ));

    // Drive the first polls consecutively without yielding, so all three jobs are admitted
    // before any worker can start on the test runtime.
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    assert!(
        matches!(guest.as_mut().poll(&mut cx), Poll::Pending),
        "guest search finished during its first poll"
    );
    assert!(
        matches!(account_generation_one.as_mut().poll(&mut cx), Poll::Pending),
        "first account generation finished during its first poll"
    );
    assert!(
        matches!(account_generation_two.as_mut().poll(&mut cx), Poll::Pending),
        "second account generation finished during its first poll"
    );

    let (guest_page, first_page, second_page) = tokio::join!(
        &mut guest,
        &mut account_generation_one,
        &mut account_generation_two
    );
    assert_eq!(guest_page.expect("guest search").data.len(), 2);
    assert_eq!(first_page.expect("first account search").data.len(), 2);
    assert_eq!(second_page.expect("second account search").data.len(), 2);

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        requests.len(),
        4,
        "primer plus three distinct scopes must each send one request"
    );

    let mut keyed = 0;
    for req in &requests {
        assert_eq!(req.url.path(), "/api/v1/search");
        assert!(
            !req.url.as_str().contains("apikey"),
            "API key must never be sent as a query parameter: {}",
            req.url
        );
        assert!(
            !req.url.as_str().contains(COALESCE_KEY),
            "secret leaked into a request URL: {}",
            req.url
        );
        if let Some(value) = req.headers.get("x-api-key") {
            assert_eq!(value.to_str().ok(), Some(COALESCE_KEY));
            keyed += 1;
        }
    }
    assert_eq!(
        keyed, 2,
        "only the two account-scoped requests may carry the API key header"
    );
}
