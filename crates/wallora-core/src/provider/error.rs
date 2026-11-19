use std::time::Duration;

use thiserror::Error;

/// Canonical provider error variants mapped cleanly to user-facing messages and actions.
/// Never embeds raw URLs, headers, or credential values.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderError {
    #[error("authentication required or API key rejected")]
    Unauthorized,
    #[error("local credential storage could not be read")]
    CredentialStoreUnavailable,
    #[error("stored API key is missing")]
    MissingCredential,
    #[error("API key cannot be used as a request header")]
    InvalidCredential,
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
    #[error("image payload exceeded the {limit} byte limit")]
    ImageTooLarge { limit: usize },
    #[error("request rejected by origin policy")]
    OriginRejected,
}

impl ProviderError {
    pub fn is_local_credential_error(&self) -> bool {
        matches!(
            self,
            Self::CredentialStoreUnavailable | Self::MissingCredential | Self::InvalidCredential
        )
    }

    pub fn user_message(&self) -> String {
        match self {
            Self::Unauthorized => {
                "Sign in with a valid API key to access this resource.".to_string()
            }
            Self::CredentialStoreUnavailable => {
                "Could not read Windows Credential Manager. Try again.".to_string()
            }
            Self::MissingCredential => {
                "The saved API key is missing from Windows Credential Manager. Sign in again."
                    .to_string()
            }
            Self::InvalidCredential => {
                "The API key is empty or contains invalid data. Enter a valid key and try again."
                    .to_string()
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
            Self::ImageTooLarge { limit } => {
                format!("This image exceeds the {limit} byte size limit.")
            }
            Self::OriginRejected => "Blocked a request to an unauthorized origin.".to_string(),
        }
    }
}
