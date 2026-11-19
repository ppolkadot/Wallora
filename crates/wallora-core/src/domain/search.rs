use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::Wallpaper;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CategoryMask {
    pub general: bool,
    pub anime: bool,
    pub people: bool,
}

impl CategoryMask {
    pub const ALL: Self = Self {
        general: true,
        anime: true,
        people: true,
    };

    pub fn to_param(self) -> String {
        format!(
            "{}{}{}",
            u8::from(self.general),
            u8::from(self.anime),
            u8::from(self.people),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PurityMask {
    pub sfw: bool,
    pub sketchy: bool,
    pub nsfw: bool,
}

impl PurityMask {
    pub const SFW_ONLY: Self = Self {
        sfw: true,
        sketchy: false,
        nsfw: false,
    };

    pub const SFW_AND_SKETCHY: Self = Self {
        sfw: true,
        sketchy: true,
        nsfw: false,
    };

    pub const ALL: Self = Self {
        sfw: true,
        sketchy: true,
        nsfw: true,
    };

    pub fn to_param(self) -> String {
        format!(
            "{}{}{}",
            u8::from(self.sfw),
            u8::from(self.sketchy),
            u8::from(self.nsfw),
        )
    }
}

/// Documented `sorting` parameter values from `https://wallhaven.cc/help/api`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sorting {
    DateAdded,
    Relevance,
    Random,
    Views,
    Favorites,
    Toplist,
}

impl Sorting {
    pub fn as_param(self) -> &'static str {
        match self {
            Self::DateAdded => "date_added",
            Self::Relevance => "relevance",
            Self::Random => "random",
            Self::Views => "views",
            Self::Favorites => "favorites",
            Self::Toplist => "toplist",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Desc,
    Asc,
}

impl SortOrder {
    pub fn as_param(self) -> &'static str {
        match self {
            Self::Desc => "desc",
            Self::Asc => "asc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TopRange {
    OneDay,
    ThreeDays,
    OneWeek,
    OneMonth,
    ThreeMonths,
    SixMonths,
    OneYear,
}

impl TopRange {
    pub fn as_param(self) -> &'static str {
        match self {
            Self::OneDay => "1d",
            Self::ThreeDays => "3d",
            Self::OneWeek => "1w",
            Self::OneMonth => "1M",
            Self::ThreeMonths => "3M",
            Self::SixMonths => "6M",
            Self::OneYear => "1y",
        }
    }

    pub fn from_api_str(raw: &str) -> Option<Self> {
        match raw.trim() {
            "1d" => Some(Self::OneDay),
            "3d" => Some(Self::ThreeDays),
            "1w" => Some(Self::OneWeek),
            "1M" => Some(Self::OneMonth),
            "3M" => Some(Self::ThreeMonths),
            "6M" => Some(Self::SixMonths),
            "1y" => Some(Self::OneYear),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

impl Dimensions {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub fn parse_wxh(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let (w, h) = trimmed
            .split_once('x')
            .or_else(|| trimmed.split_once('X'))?;
        let width: u32 = w.parse().ok()?;
        let height: u32 = h.parse().ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        Some(Self { width, height })
    }
}

impl fmt::Display for Dimensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AspectRatio(String);

impl AspectRatio {
    pub fn new(ratio: impl Into<String>) -> Self {
        Self(ratio.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AspectRatio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Request priority queue classification per Network Policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RequestPriority {
    #[default]
    Interactive,
    Visible,
    Prefetch,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub q: Option<String>,
    pub categories: Option<CategoryMask>,
    pub purity: Option<PurityMask>,
    pub sorting: Option<Sorting>,
    pub order: Option<SortOrder>,
    pub top_range: Option<TopRange>,
    pub atleast: Option<Dimensions>,
    pub resolutions: Vec<Dimensions>,
    pub ratios: Vec<AspectRatio>,
    pub colors: Option<String>,
    pub page: Option<u32>,
    pub seed: Option<String>,
    pub priority: RequestPriority,
}

impl SearchQuery {
    /// Serializes documented API query parameters in deterministic key order.
    pub fn to_query_pairs(&self) -> Vec<(&'static str, String)> {
        let mut pairs = Vec::new();
        if let Some(q) = &self.q
            && !q.is_empty()
        {
            pairs.push(("q", q.clone()));
        }
        if let Some(categories) = self.categories {
            pairs.push(("categories", categories.to_param()));
        }
        if let Some(purity) = self.purity {
            pairs.push(("purity", purity.to_param()));
        }
        if let Some(sorting) = self.sorting {
            pairs.push(("sorting", sorting.as_param().to_string()));
        }
        if let Some(order) = self.order {
            pairs.push(("order", order.as_param().to_string()));
        }
        if let Some(top_range) = self.top_range {
            pairs.push(("topRange", top_range.as_param().to_string()));
        }
        if let Some(atleast) = self.atleast {
            pairs.push(("atleast", atleast.to_string()));
        }
        if !self.resolutions.is_empty() {
            let joined = self
                .resolutions
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            pairs.push(("resolutions", joined));
        }
        if !self.ratios.is_empty() {
            let joined = self
                .ratios
                .iter()
                .map(AspectRatio::as_str)
                .collect::<Vec<_>>()
                .join(",");
            pairs.push(("ratios", joined));
        }
        if let Some(colors) = &self.colors
            && !colors.is_empty()
        {
            pairs.push(("colors", colors.trim_start_matches('#').to_lowercase()));
        }
        if let Some(page) = self.page {
            pairs.push(("page", page.to_string()));
        }
        if let Some(seed) = &self.seed
            && !seed.is_empty()
        {
            pairs.push(("seed", seed.clone()));
        }
        pairs
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QueryEcho {
    Text(String),
    Tag { id: u64, tag: String },
}

/// Accepts a JSON number or a nonempty ASCII decimal string within `u32` bounds.
///
/// The recorded `/settings` response renders `per_page` as a string while search responses use a
/// number; both shapes are accepted defensively. Serialization always emits a number.
pub(crate) fn deserialize_per_page<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum PerPage {
        Number(u32),
        Text(String),
    }

    match PerPage::deserialize(deserializer)? {
        PerPage::Number(value) => Ok(value),
        PerPage::Text(text) => {
            let decimal = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
            if !decimal {
                return Err(serde::de::Error::custom(
                    "per_page must be a nonempty decimal string",
                ));
            }
            text.parse::<u32>()
                .map_err(|_| serde::de::Error::custom("per_page exceeds the u32 range"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMeta {
    pub current_page: u32,
    pub last_page: u32,
    #[serde(deserialize_with = "deserialize_per_page")]
    pub per_page: u32,
    pub total: u64,
    #[serde(default)]
    pub query: Option<QueryEcho>,
    #[serde(default)]
    pub seed: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPage {
    pub data: Vec<Wallpaper>,
    pub meta: SearchMeta,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(value: serde_json::Value) -> Result<SearchMeta, serde_json::Error> {
        serde_json::from_value(value)
    }

    fn base(per_page: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "current_page": 1,
            "last_page": 3,
            "per_page": per_page,
            "total": 72,
        })
    }

    #[test]
    fn search_meta_rejects_invalid_per_page() {
        let rejected = [
            serde_json::json!({ "current_page": 1, "last_page": 3, "total": 72 }),
            base(serde_json::Value::Null),
            base(serde_json::json!("")),
            base(serde_json::json!("-24")),
            base(serde_json::json!("+24")),
            base(serde_json::json!("24.0")),
            base(serde_json::json!(24.0)),
            base(serde_json::json!("two dozen")),
            base(serde_json::json!("0x18")),
            base(serde_json::json!(4_294_967_296u64)),
            base(serde_json::json!("4294967296")),
            base(serde_json::json!("99999999999999999999")),
        ];
        for value in rejected {
            assert!(
                meta(value.clone()).is_err(),
                "per_page accepted invalid form: {value}"
            );
        }

        // Valid numeric and string forms both decode and re-serialize as numbers.
        for per_page in [serde_json::json!(24), serde_json::json!("24")] {
            let decoded = meta(base(per_page)).expect("valid per_page");
            assert_eq!(decoded.per_page, 24);
            let encoded = serde_json::to_value(&decoded).expect("serialize meta");
            assert_eq!(encoded["per_page"], serde_json::json!(24));
        }
    }
}
