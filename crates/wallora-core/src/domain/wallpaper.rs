use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WallpaperId(String);

impl WallpaperId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WallpaperId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for WallpaperId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl From<String> for WallpaperId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purity {
    Sfw,
    Sketchy,
    Nsfw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    General,
    Anime,
    People,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thumbs {
    pub large: String,
    pub original: String,
    pub small: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Avatar {
    #[serde(rename = "200px", default)]
    pub px_200: Option<String>,
    #[serde(rename = "128px", default)]
    pub px_128: Option<String>,
    #[serde(rename = "32px", default)]
    pub px_32: Option<String>,
    #[serde(rename = "20px", default)]
    pub px_20: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uploader {
    pub username: String,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub avatar: Option<Avatar>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tag {
    pub id: u64,
    pub name: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub alias: String,
    pub category_id: u64,
    pub category: String,
    pub purity: Purity,
    pub created_at: String,
}

/// Deserializes a string field that upstream may render as `null`, mapping `null` to empty and
/// rejecting every other non-string type.
fn deserialize_string_or_null<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wallpaper {
    pub id: WallpaperId,
    pub url: String,
    #[serde(default)]
    pub short_url: Option<String>,
    #[serde(default)]
    pub uploader: Option<Uploader>,
    #[serde(default)]
    pub views: u64,
    #[serde(default)]
    pub favorites: u64,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    pub source: String,
    pub purity: Purity,
    pub category: Category,
    pub dimension_x: u32,
    pub dimension_y: u32,
    pub resolution: String,
    pub ratio: String,
    pub file_size: u64,
    pub file_type: String,
    pub created_at: String,
    #[serde(default)]
    pub colors: Vec<String>,
    pub path: String,
    pub thumbs: Thumbs,
    #[serde(default)]
    pub tags: Vec<Tag>,
}
