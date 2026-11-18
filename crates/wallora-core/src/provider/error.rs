use std::time::Duration;

use thiserror::Error;

/// Canonical provider error variants mapped cleanly to user-facing messages and actions.
/// Never embeds raw URLs, headers, or credential values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderError {
    #[error("authentication required or API key rejected")]
    Unauthorized,
    #[error("access forbidden by upstream")]
    Forbidden,
    #[error("requested resource was not found")]
    NotFound,
    #[error("rate limited by upstream")]
    RateLimited { retry_after: Option<Duration> },
    #[error("too many requests queued, try again")]
    Busy,
    #[error("request timed out")]
    Timeout,
    #[error("network error")]
    Network,
    #[error("failed to decode upstream response ({context})")]
    Decode { context: String },
    #[error("upstream returned HTTP {status}")]
    Upstream { status: u16 },
    #[error("request rejected by origin policy")]
    OriginRejected,
}

impl ProviderError {
    pub fn user_message(&self) -> String {
        match self {
            Self::Unauthorized => {
                "Sign in with a valid API key to access this resource.".to_string()
            }
            Self::Forbidden => "Wallhaven denied access to this resource.".to_string(),
            Self::NotFound => "The requested wallpaper or search target was not found.".to_string(),
            Self::RateLimited {
                retry_after: Some(d),
            } => {
                format!(
                    "Rate limit reached. Wait {} seconds before retrying.",
                    d.as_secs().max(1)
                )
            }
            Self::RateLimited { retry_after: None } => {
                "Rate limit reached. Wait 60 seconds before retrying.".to_string()
            }
            Self::Busy => "Too many requests queued, try again.".to_string(),
            Self::Timeout => "The request to Wallhaven timed out. Try again.".to_string(),
            Self::Network => {
                "Unable to reach Wallhaven. Check your connection and try again.".to_string()
            }
            Self::Decode { context } => format!("Received unexpected data ({context})."),
            Self::Upstream { status } => format!("Wallhaven returned an unexpected HTTP {status}."),
            Self::OriginRejected => "Blocked a request to an unauthorized origin.".to_string(),
        }
    }
}
