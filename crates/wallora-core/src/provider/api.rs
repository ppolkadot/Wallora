use std::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::domain::{SearchPage, Wallpaper};
use crate::provider::error::ProviderError;

#[derive(Debug, Deserialize)]
struct DataEnvelope<T> {
    data: T,
}

pub fn decode_wallpaper_response(bytes: &[u8]) -> Result<Wallpaper, ProviderError> {
    let envelope: DataEnvelope<Wallpaper> =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::Decode {
            context: "wallpaper response".to_string(),
        })?;
    Ok(envelope.data)
}

pub fn decode_search_response(bytes: &[u8]) -> Result<SearchPage, ProviderError> {
    serde_json::from_slice::<SearchPage>(bytes).map_err(|_| ProviderError::Decode {
        context: "search page response".to_string(),
    })
}

pub fn parse_retry_after(raw: Option<&str>) -> Option<Duration> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    if let Ok(target) =
        OffsetDateTime::parse(trimmed, &time::format_description::well_known::Rfc2822)
    {
        let now = OffsetDateTime::now_utc();
        let delta = target - now;
        if delta.is_positive() {
            return Some(Duration::from_secs(delta.whole_seconds() as u64));
        }
        return Some(Duration::ZERO);
    }
    None
}
