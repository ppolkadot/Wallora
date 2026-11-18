use insta::assert_snapshot;
use url::form_urlencoded;
use wallora_core::domain::{
    AspectRatio, CategoryMask, Dimensions, PurityMask, RequestPriority, SearchQuery, SortOrder,
    Sorting, TopRange,
};

fn serialize_query_string(query: &SearchQuery) -> String {
    let mut ser = form_urlencoded::Serializer::new(String::new());
    for (k, v) in query.to_query_pairs() {
        ser.append_pair(k, &v);
    }
    ser.finish()
}

#[test]
fn empty_query_serializes_empty() {
    let query = SearchQuery::default();
    assert_snapshot!("empty_query", serialize_query_string(&query));
}

#[test]
fn full_documented_filters_serialize_deterministically() {
    let query = SearchQuery {
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
    };

    assert_snapshot!("full_documented_filters", serialize_query_string(&query));
}

#[test]
fn all_documented_sortings_and_top_ranges_serialize() {
    let sortings = [
        Sorting::DateAdded,
        Sorting::Relevance,
        Sorting::Random,
        Sorting::Views,
        Sorting::Favorites,
        Sorting::Toplist,
    ]
    .into_iter()
    .map(|sorting| {
        let query = SearchQuery {
            sorting: Some(sorting),
            ..Default::default()
        };
        serialize_query_string(&query)
    })
    .collect::<Vec<_>>()
    .join("\n");

    let top_ranges = [
        TopRange::OneDay,
        TopRange::ThreeDays,
        TopRange::OneWeek,
        TopRange::OneMonth,
        TopRange::ThreeMonths,
        TopRange::SixMonths,
        TopRange::OneYear,
    ]
    .into_iter()
    .map(|range| {
        let query = SearchQuery {
            sorting: Some(Sorting::Toplist),
            top_range: Some(range),
            ..Default::default()
        };
        serialize_query_string(&query)
    })
    .collect::<Vec<_>>()
    .join("\n");

    assert_snapshot!("documented_sortings", sortings);
    assert_snapshot!("documented_top_ranges", top_ranges);
}
