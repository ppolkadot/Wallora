use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::domain::{CollectionSummary, SearchPage, Tag, UserSettings, Wallpaper};
use crate::provider::error::ProviderError;

/// Hard upper bound applied to any upstream `Retry-After` value, in seconds or date form.
pub const MAX_COOLDOWN: Duration = Duration::from_secs(3_600);

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

pub fn decode_user_settings_response(bytes: &[u8]) -> Result<UserSettings, ProviderError> {
    let envelope: DataEnvelope<UserSettings> =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::Decode {
            context: "user settings response".to_string(),
        })?;
    Ok(envelope.data)
}

pub fn decode_collections_response(bytes: &[u8]) -> Result<Vec<CollectionSummary>, ProviderError> {
    let envelope: DataEnvelope<Vec<CollectionSummary>> =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::Decode {
            context: "collections response".to_string(),
        })?;
    Ok(envelope.data)
}

pub fn decode_tag_response(bytes: &[u8]) -> Result<Tag, ProviderError> {
    let envelope: DataEnvelope<Tag> =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::Decode {
            context: "tag response".to_string(),
        })?;
    Ok(envelope.data)
}

/// Parses an upstream `Retry-After` header: either delta-seconds or any of the three
/// HTTP-date forms. Returns `None` for absent or malformed values and never exceeds
/// [`MAX_COOLDOWN`].
pub fn parse_retry_after(raw: Option<&str>) -> Option<Duration> {
    parse_retry_after_at(raw, SystemTime::now())
}

fn parse_retry_after_at(raw: Option<&str>, now: SystemTime) -> Option<Duration> {
    let trimmed = raw?.trim();
    if trimmed.is_empty() {
        return None;
    }

    if trimmed.bytes().all(|b| b.is_ascii_digit()) {
        let secs = trimmed.bytes().fold(0u64, |acc, digit| {
            acc.saturating_mul(10)
                .saturating_add(u64::from(digit - b'0'))
        });
        return Some(Duration::from_secs(secs).min(MAX_COOLDOWN));
    }

    let target = httpdate::parse_http_date(trimmed).ok()?;
    let wait = target.duration_since(now).unwrap_or(Duration::ZERO);
    Some(wait.min(MAX_COOLDOWN))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Sunday, 06-Nov-94 08:49:37 GMT` as a `SystemTime`.
    const LEGACY_INSTANT: u64 = 784_111_777;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn retry_after_accepts_http_dates_and_clamps() {
        let now = at(LEGACY_INSTANT - 10);

        // IMF-fixdate and both legacy HTTP-date forms resolve to the same instant.
        assert_eq!(
            parse_retry_after_at(Some("Sun, 06 Nov 1994 08:49:37 GMT"), now),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            parse_retry_after_at(Some("Sunday, 06-Nov-94 08:49:37 GMT"), now),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            parse_retry_after_at(Some("Sun Nov  6 08:49:37 1994"), now),
            Some(Duration::from_secs(10))
        );

        // Outer whitespace is trimmed before parsing.
        assert_eq!(
            parse_retry_after_at(Some("  Sun, 06 Nov 1994 08:49:37 GMT  "), now),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            parse_retry_after_at(Some("\t15\n"), now),
            Some(Duration::from_secs(15))
        );

        // A past date maps to zero rather than a negative duration.
        assert_eq!(
            parse_retry_after_at(
                Some("Sun, 06 Nov 1994 08:49:37 GMT"),
                at(LEGACY_INSTANT + 100)
            ),
            Some(Duration::ZERO)
        );

        // A future date over an hour away clamps to the one-hour ceiling.
        assert_eq!(
            parse_retry_after_at(
                Some(&httpdate::fmt_http_date(now + Duration::from_secs(7_200))),
                now
            ),
            Some(MAX_COOLDOWN)
        );

        // Delta seconds, including zero, the u64 ceiling and decimal overflow.
        assert_eq!(parse_retry_after_at(Some("0"), now), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after_at(Some("45"), now),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            parse_retry_after_at(Some("18446744073709551615"), now),
            Some(MAX_COOLDOWN)
        );
        assert_eq!(
            parse_retry_after_at(Some("99999999999999999999999999"), now),
            Some(MAX_COOLDOWN)
        );

        // Default-invalid inputs.
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("-5"),
            Some("+5"),
            Some("1.5"),
            Some("12abc"),
            Some("0x10"),
            Some("not-a-date"),
            Some("Wed, 32 Oct 2015 07:28:00 GMT"),
        ] {
            assert_eq!(parse_retry_after_at(raw, now), None, "raw={raw:?}");
        }
    }
}
