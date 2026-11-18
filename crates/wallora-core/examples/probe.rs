//! Opt-in Wallhaven API v1 probe for Task 1b.
//!
//! Enforces:
//! - Hard budget of <= 25 requests per run
//! - >= 2.0 s spacing between requests
//! - Immediate abort on first HTTP 429
//! - Reads `WALLHAVEN_API_KEY` from env / `.env` via `X-API-Key` header only
//! - Never prints, logs, or writes the API key or user-identifying values

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::Value;
use zeroize::{Zeroize, ZeroizeOnDrop};

const BASE_URL: &str = "https://wallhaven.cc/api/v1";
const USER_AGENT: &str = concat!(
    "Wallora/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/ppolkadot/Wallora)"
);
const MAX_REQUESTS: usize = 25;
const MIN_SPACING: Duration = Duration::from_millis(2100);

#[derive(Zeroize, ZeroizeOnDrop)]
struct ApiKey(String);

impl ApiKey {
    fn as_header_value(&self) -> &str {
        &self.0
    }
}

struct ProbeGuard {
    client: reqwest::Client,
    api_key: Option<ApiKey>,
    count: usize,
    last_request: Option<Instant>,
}

impl ProbeGuard {
    fn new(api_key: Option<ApiKey>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("client build failed: {e}"))?;
        Ok(Self {
            client,
            api_key,
            count: 0,
            last_request: None,
        })
    }

    async fn get(
        &mut self,
        path_and_query: &str,
        use_key: bool,
    ) -> Result<(StatusCode, Value), String> {
        if self.count >= MAX_REQUESTS {
            return Err(format!("hard budget exceeded ({MAX_REQUESTS} requests)"));
        }

        if let Some(last) = self.last_request {
            let elapsed = last.elapsed();
            if elapsed < MIN_SPACING {
                tokio::time::sleep(MIN_SPACING - elapsed).await;
            }
        }

        let url = format!("{BASE_URL}{path_and_query}");
        let mut req = self.client.get(&url);
        if use_key {
            let key = self
                .api_key
                .as_ref()
                .ok_or_else(|| "keyed request attempted without API key".to_string())?;
            req = req.header("X-API-Key", key.as_header_value());
        }

        self.count += 1;
        self.last_request = Some(Instant::now());

        let resp = req.send().await.map_err(|e| {
            format!(
                "request #{count} to {path_and_query} failed: {e}",
                count = self.count
            )
        })?;
        let status = resp.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(format!(
                "HTTP 429 received on request #{count} ({path_and_query}); aborting immediately",
                count = self.count
            ));
        }

        let body_text = resp
            .text()
            .await
            .map_err(|e| format!("failed reading body for {path_and_query}: {e}"))?;
        let json = serde_json::from_str::<Value>(&body_text)
            .unwrap_or_else(|_| Value::String(format!("<non-json len={}>", body_text.len())));

        Ok((status, json))
    }
}

fn json_keys(value: &Value) -> Vec<String> {
    match value.as_object() {
        Some(map) => map.keys().cloned().collect(),
        None => Vec::new(),
    }
}

fn search_ids(value: &Value) -> Vec<String> {
    value
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .take(5)
                .filter_map(|item| {
                    item.get("id")
                        .and_then(|id| id.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn search_purities(value: &Value) -> BTreeSet<String> {
    value
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    item.get("purity")
                        .and_then(|p| p.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn describe_value_types(value: &Value) -> Vec<String> {
    match value.as_object() {
        Some(map) => map
            .iter()
            .map(|(k, v)| {
                let kind = match v {
                    Value::Null => "null",
                    Value::Bool(_) => "bool",
                    Value::Number(_) => "number",
                    Value::String(_) => "string",
                    Value::Array(a) => {
                        if let Some(first) = a.first() {
                            if first.is_string() {
                                "array<string>"
                            } else if first.is_object() {
                                "array<object>"
                            } else {
                                "array"
                            }
                        } else {
                            "array<empty>"
                        }
                    }
                    Value::Object(_) => "object",
                };
                format!("{k}:{kind}")
            })
            .collect(),
        None => Vec::new(),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("Probe aborted/failed: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    dotenvy::dotenv().ok();

    let api_key = std::env::var("WALLHAVEN_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(ApiKey);

    let username = std::env::var("WALLHAVEN_USERNAME")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    println!("Wallora API Probe (Task 1b)");
    println!("Budget: max {MAX_REQUESTS} requests, min spacing {MIN_SPACING:?}");
    println!(
        "API key loaded: {}",
        if api_key.is_some() {
            "yes (redacted)"
        } else {
            "no"
        }
    );
    println!(
        "Username supplied: {}",
        if username.is_some() {
            "yes (redacted)"
        } else {
            "no"
        }
    );
    println!("---");

    let mut guard = ProbeGuard::new(api_key)?;

    // 1. Known SFW wallpaper as guest
    let (status, sfw_json) = guard.get("/w/94x38z", false).await?;
    let data_keys = sfw_json.get("data").map(json_keys).unwrap_or_default();
    let tag_keys = sfw_json
        .get("data")
        .and_then(|d| d.get("tags"))
        .and_then(|t| t.as_array())
        .and_then(|a| a.first())
        .map(json_keys)
        .unwrap_or_default();
    let uploader_keys = sfw_json
        .get("data")
        .and_then(|d| d.get("uploader"))
        .map(json_keys)
        .unwrap_or_default();
    println!(
        "[1] GET /w/94x38z (guest) -> {} | data keys: {:?} | tag keys: {:?} | uploader keys: {:?}",
        status.as_u16(),
        data_keys,
        tag_keys,
        uploader_keys
    );

    // 2. Find an NSFW id if key present, then test guest 401 vs keyed 200
    if guard.api_key.is_some() {
        let (nsfw_search_status, nsfw_search) = guard.get("/search?purity=001", true).await?;
        let nsfw_id = nsfw_search
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        println!(
            "[2] GET /search?purity=001 (keyed) -> {} | found nsfw sample id: {}",
            nsfw_search_status.as_u16(),
            nsfw_id.is_some()
        );

        if let Some(id) = nsfw_id {
            let (guest_nsfw_status, guest_nsfw_json) =
                guard.get(&format!("/w/{id}"), false).await?;
            println!(
                "[3] GET /w/<nsfw_id> (guest) -> {} | body keys: {:?}",
                guest_nsfw_status.as_u16(),
                json_keys(&guest_nsfw_json)
            );

            let (keyed_nsfw_status, keyed_nsfw_json) = guard.get(&format!("/w/{id}"), true).await?;
            let actual_purity = keyed_nsfw_json
                .get("data")
                .and_then(|d| d.get("purity"))
                .and_then(|p| p.as_str())
                .unwrap_or("unknown");
            println!(
                "[4] GET /w/<nsfw_id> (keyed) -> {} | purity={actual_purity}",
                keyed_nsfw_status.as_u16()
            );
        } else {
            println!(
                "[3-4] SKIPPED /w/<nsfw_id> check: keyed purity=001 returned no items (account may restrict NSFW)"
            );
        }
    } else {
        println!("[2-4] SKIPPED NSFW checks (no WALLHAVEN_API_KEY)");
    }

    // 5. Guest purity=111
    let (status, p111_json) = guard.get("/search?purity=111", false).await?;
    println!(
        "[5] GET /search?purity=111 (guest) -> {} | count={} | purities={:?}",
        status.as_u16(),
        p111_json
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0),
        search_purities(&p111_json)
    );

    // 6-11. Documented sortings
    for (idx, path) in [
        "/search?sorting=date_added",
        "/search?sorting=relevance&q=nature",
        "/search?sorting=random",
        "/search?sorting=views",
        "/search?sorting=favorites",
        "/search?sorting=toplist",
    ]
    .iter()
    .enumerate()
    {
        let (st, js) = guard.get(path, false).await?;
        let meta_keys = js.get("meta").map(describe_value_types).unwrap_or_default();
        let seed_type = js
            .get("meta")
            .and_then(|m| m.get("seed"))
            .map(|s| match s {
                Value::Null => "null",
                Value::String(_) => "string",
                _ => "other",
            })
            .unwrap_or("absent");
        println!(
            "[{}] GET {} (guest) -> {} | top3={:?} | meta={:?} | seed_type={}",
            6 + idx,
            path,
            st.as_u16(),
            search_ids(&js).into_iter().take(3).collect::<Vec<_>>(),
            meta_keys,
            seed_type
        );
    }

    // 12. like:{id}
    let (st, like_js) = guard.get("/search?q=like:94x38z", false).await?;
    println!(
        "[12] GET /search?q=like:94x38z (guest) -> {} | results={}",
        st.as_u16(),
        like_js
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    );

    // 13. id:{tagid}
    let (st, tag_search_js) = guard.get("/search?q=id:1", false).await?;
    let meta_query_shape = tag_search_js
        .get("meta")
        .and_then(|m| m.get("query"))
        .map(describe_value_types)
        .unwrap_or_default();
    println!(
        "[13] GET /search?q=id:1 (guest) -> {} | meta.query shape={:?}",
        st.as_u16(),
        meta_query_shape
    );

    // 14. /tag/1
    let (st, tag_js) = guard.get("/tag/1", false).await?;
    println!(
        "[14] GET /tag/1 (guest) -> {} | data types={:?}",
        st.as_u16(),
        tag_js
            .get("data")
            .map(describe_value_types)
            .unwrap_or_default()
    );

    // 15. /settings guest -> 401
    let (st, settings_guest_js) = guard.get("/settings", false).await?;
    println!(
        "[15] GET /settings (guest) -> {} | body keys={:?}",
        st.as_u16(),
        json_keys(&settings_guest_js)
    );

    // 16. /settings keyed -> 200
    if guard.api_key.is_some() {
        let (st, settings_keyed_js) = guard.get("/settings", true).await?;
        let types = settings_keyed_js
            .get("data")
            .map(describe_value_types)
            .unwrap_or_default();
        println!(
            "[16] GET /settings (keyed) -> {} | data types={:?}",
            st.as_u16(),
            types
        );
    } else {
        println!("[16] SKIPPED GET /settings (keyed) (no WALLHAVEN_API_KEY)");
    }

    // 17. /collections keyed -> 200 (scrubbed structure only)
    let mut first_public_collection_id: Option<u64> = None;
    if guard.api_key.is_some() {
        let (st, cols_js) = guard.get("/collections", true).await?;
        let arr = cols_js
            .get("data")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        let entry_types = arr.first().map(describe_value_types).unwrap_or_default();
        let has_default_label = arr.iter().any(|c| {
            c.get("label").and_then(|l| l.as_str()).is_some_and(|l| {
                l.eq_ignore_ascii_case("Default") || l.eq_ignore_ascii_case("Favorites")
            })
        });
        first_public_collection_id = arr
            .iter()
            .find(|c| c.get("public").and_then(|p| p.as_u64()) == Some(1))
            .and_then(|c| c.get("id").and_then(|id| id.as_u64()));
        println!(
            "[17] GET /collections (keyed) -> {} | count={} | entry types={:?} | has Default/Favorites label={}",
            st.as_u16(),
            arr.len(),
            entry_types,
            has_default_label
        );
    } else {
        println!("[17] SKIPPED GET /collections (keyed) (no WALLHAVEN_API_KEY)");
    }

    // 18-19. /collections/{username} and /collections/{username}/{id} if username supplied
    if let Some(user) = username {
        let (st, user_cols) = guard.get(&format!("/collections/{user}"), false).await?;
        let count = user_cols
            .get("data")
            .and_then(|d| d.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        println!(
            "[18] GET /collections/<username> (guest) -> {} | public collections={count}",
            st.as_u16()
        );
        if let Some(cid) = first_public_collection_id.or_else(|| {
            user_cols
                .get("data")
                .and_then(|d| d.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("id"))
                .and_then(|id| id.as_u64())
        }) {
            let (st2, col_items) = guard
                .get(&format!("/collections/{user}/{cid}?purity=100"), false)
                .await?;
            println!(
                "[19] GET /collections/<username>/<id>?purity=100 (guest) -> {} | items={} | meta={:?}",
                st2.as_u16(),
                col_items
                    .get("data")
                    .and_then(|d| d.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0),
                col_items
                    .get("meta")
                    .map(describe_value_types)
                    .unwrap_or_default()
            );
        }
    } else {
        println!(
            "[18-19] SKIPPED /collections/<username> checks (no WALLHAVEN_USERNAME; account collection-item rows remain unresolved)"
        );
    }

    // 20-22. `hot` sorting check: sorting=hot vs default vs sorting=not_a_sort
    let (st_hot, hot_js) = guard.get("/search?sorting=hot", false).await?;
    let (st_def, def_js) = guard.get("/search", false).await?;
    let (st_bad, bad_js) = guard.get("/search?sorting=not_a_sort", false).await?;
    let hot_ids = search_ids(&hot_js);
    let def_ids = search_ids(&def_js);
    let bad_ids = search_ids(&bad_js);
    println!(
        "[20-22] `hot` check: hot={} def={} bad={} | hot!=def: {} | hot!=bad: {} | def==bad: {}",
        st_hot.as_u16(),
        st_def.as_u16(),
        st_bad.as_u16(),
        hot_ids != def_ids,
        hot_ids != bad_ids,
        def_ids == bad_ids
    );

    println!("---");
    println!(
        "Probe completed cleanly. Total requests sent: {}/{MAX_REQUESTS}",
        guard.count
    );
    Ok(())
}
