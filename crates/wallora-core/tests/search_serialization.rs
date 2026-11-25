use std::sync::Arc;

use insta::assert_snapshot;
use url::Url;
use wallora_core::domain::{
    AspectRatio, CategoryMask, Dimensions, FileType, InMemoryCredentialStore, PurityMask,
    QueryBuilder, RequestPriority, SearchQuery, SessionScope, SortOrder, Sorting, TopRange,
};
use wallora_core::provider::{Provider, WallhavenClient, WallhavenClientConfig};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FIXTURE_SEARCH_PAGE: &str = include_str!("fixtures/search_page.json");

/// One real search request's recorded URL.
struct Recorded {
    url: Url,
}

impl Recorded {
    /// The raw query string, or the empty string when the request carried no query at all.
    fn query(&self) -> String {
        self.url.query().unwrap_or_default().to_string()
    }
}

async fn server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_SEARCH_PAGE))
        .mount(&server)
        .await;
    server
}

/// Sends `query` through `Provider::search` with a fresh client (so its own pacer never delays
/// the request) and returns the single request the mock received.
async fn record_search(server: &MockServer, query: SearchQuery) -> Recorded {
    let before = server
        .received_requests()
        .await
        .map(|requests| requests.len())
        .unwrap_or(0);

    let base = Url::parse(&format!("{}/api/v1", server.uri())).expect("valid mock URI");
    let config = WallhavenClientConfig::for_test(base).expect("valid test config");
    let client = WallhavenClient::with_config(Arc::new(InMemoryCredentialStore::new()), config)
        .expect("client build");
    client
        .search(query, SessionScope::guest(1))
        .await
        .expect("search should succeed against the mock");

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        requests.len(),
        before + 1,
        "each query must produce exactly one request"
    );
    let request = requests.last().expect("recorded request");
    assert_eq!(request.method.as_str(), "GET");
    assert_eq!(request.url.path(), "/api/v1/search");
    assert!(
        request
            .url
            .query_pairs()
            .all(|(key, _)| !key.eq_ignore_ascii_case("apikey")),
        "the API key must never be a query parameter: {}",
        request.url
    );
    Recorded {
        url: request.url.clone(),
    }
}

#[tokio::test]
async fn empty_query_serializes_empty() {
    let server = server().await;
    let recorded = record_search(&server, SearchQuery::default()).await;

    assert!(
        recorded.url.query().is_none(),
        "the default query must not add a query string: {}",
        recorded.url
    );
    assert!(
        !recorded.url.as_str().contains('?'),
        "the default query must not emit an empty query separator: {}",
        recorded.url
    );
    assert_snapshot!("empty_query", recorded.query());
}

#[tokio::test]
async fn full_documented_filters_serialize_deterministically() {
    let server = server().await;
    let recorded = record_search(
        &server,
        SearchQuery {
            q: Some("+landscape -city @artist id:42 type:png like:abc123".to_string()),
            categories: Some(CategoryMask {
                general: true,
                anime: false,
                people: true,
            }),
            purity: Some(PurityMask::SFW_AND_SKETCHY),
            sorting: Some(Sorting::Toplist),
            order: Some(SortOrder::Asc),
            top_range: Some(TopRange::OneMonth),
            atleast: Some(Dimensions::new(1920, 1080)),
            resolutions: vec![Dimensions::new(1920, 1080), Dimensions::new(2560, 1440)],
            ratios: vec![AspectRatio::new("16x9"), AspectRatio::new("21x9")],
            colors: Some("#F37316".to_string()),
            page: Some(3),
            seed: Some("seed42xyz".to_string()),
            priority: RequestPriority::Interactive,
        },
    )
    .await;

    assert_snapshot!("full_documented_filters", recorded.query());
}

#[tokio::test]
async fn all_documented_sortings_and_top_ranges_serialize() {
    let server = server().await;

    let mut sortings = Vec::new();
    for sorting in [
        Sorting::DateAdded,
        Sorting::Relevance,
        Sorting::Random,
        Sorting::Views,
        Sorting::Favorites,
        Sorting::Toplist,
    ] {
        let recorded = record_search(
            &server,
            SearchQuery {
                sorting: Some(sorting),
                ..Default::default()
            },
        )
        .await;
        sortings.push(recorded.query());
    }

    let mut top_ranges = Vec::new();
    for range in [
        TopRange::OneDay,
        TopRange::ThreeDays,
        TopRange::OneWeek,
        TopRange::OneMonth,
        TopRange::ThreeMonths,
        TopRange::SixMonths,
        TopRange::OneYear,
    ] {
        let recorded = record_search(
            &server,
            SearchQuery {
                sorting: Some(Sorting::Toplist),
                top_range: Some(range),
                ..Default::default()
            },
        )
        .await;
        top_ranges.push(recorded.query());
    }

    assert_snapshot!("documented_sortings", sortings.join("\n"));
    assert_snapshot!("documented_top_ranges", top_ranges.join("\n"));
}

#[tokio::test]
async fn all_documented_search_syntax_operators_serialize_end_to_end() {
    let server = server().await;

    let cases = [
        (
            QueryBuilder::new().tag("nature").unwrap().build(),
            "nature",
            None,
        ),
        (
            QueryBuilder::new().tag("red hair").unwrap().build(),
            "\"red hair\"",
            None,
        ),
        (
            QueryBuilder::new()
                .require_tag("landscape")
                .unwrap()
                .build(),
            "+landscape",
            Some("q=%2Blandscape"),
        ),
        (
            QueryBuilder::new().exclude_tag("city").unwrap().build(),
            "-city",
            Some("q=-city"),
        ),
        (
            QueryBuilder::new().user("artist").unwrap().build(),
            "@artist",
            Some("q=%40artist"),
        ),
        (
            QueryBuilder::new().tag_id(42).build(),
            "id:42",
            Some("q=id%3A42"),
        ),
        (
            QueryBuilder::new().file_type(FileType::Png).build(),
            "type:png",
            Some("q=type%3Apng"),
        ),
        (
            QueryBuilder::new().file_type(FileType::Jpg).build(),
            "type:jpg",
            Some("q=type%3Ajpg"),
        ),
        (
            QueryBuilder::new().like("abc123").unwrap().build(),
            "like:abc123",
            Some("q=like%3Aabc123"),
        ),
        (
            QueryBuilder::new()
                .tag("nature")
                .unwrap()
                .require_tag("landscape")
                .unwrap()
                .exclude_tag("city")
                .unwrap()
                .user("artist")
                .unwrap()
                .tag_id(42)
                .file_type(FileType::Png)
                .file_type(FileType::Jpg)
                .like("abc123")
                .unwrap()
                .build(),
            "nature +landscape -city @artist id:42 type:png type:jpg like:abc123",
            None,
        ),
    ];

    for (terms, expected_q, expected_raw_fragment) in cases {
        let recorded = record_search(
            &server,
            SearchQuery {
                q: terms.to_param(),
                ..Default::default()
            },
        )
        .await;

        let decoded_q = recorded
            .url
            .query_pairs()
            .find(|(key, _)| key == "q")
            .map(|(_, value)| value.into_owned());
        assert_eq!(decoded_q.as_deref(), Some(expected_q));
        if let Some(fragment) = expected_raw_fragment {
            assert_eq!(recorded.query(), fragment);
        }
    }
}
