use reqwest::header::{HeaderName, HeaderValue};
use url::Url;

use crate::domain::{CredentialStore, SessionKind, SessionScope};
use crate::provider::error::ProviderError;

const X_API_KEY_HEADER: HeaderName = HeaderName::from_static("x-api-key");

/// Origin whitelist used to gate attachment of `X-API-Key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginPolicy {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub api_path_prefix: String,
}

impl OriginPolicy {
    /// Strict production origin policy: `https://wallhaven.cc` on port `443`, prefix `/api/v1/`.
    pub fn production() -> Self {
        Self {
            scheme: "https".to_string(),
            host: "wallhaven.cc".to_string(),
            port: 443,
            api_path_prefix: "/api/v1/".to_string(),
        }
    }

    /// Constructs a test origin policy bound strictly to `base_url`'s scheme, host, port, and `/api/v1/`.
    pub fn from_api_base(base_url: &Url) -> Result<Self, ProviderError> {
        let host = base_url
            .host_str()
            .ok_or(ProviderError::OriginRejected)?
            .to_ascii_lowercase();
        let port = base_url
            .port_or_known_default()
            .ok_or(ProviderError::OriginRejected)?;
        let mut prefix = base_url.path().to_string();
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        Ok(Self {
            scheme: base_url.scheme().to_ascii_lowercase(),
            host,
            port,
            api_path_prefix: prefix,
        })
    }

    /// Returns `true` only when `url` matches scheme, host, effective port, and API path prefix.
    pub fn is_authorized_api_url(&self, url: &Url) -> bool {
        if !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        let scheme_ok = url.scheme().eq_ignore_ascii_case(&self.scheme);
        let host_ok = url
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&self.host));
        let port_ok = url.port_or_known_default() == Some(self.port);
        let path_ok = url.path().starts_with(&self.api_path_prefix);
        scheme_ok && host_ok && port_ok && path_ok
    }
}

/// Attaches `X-API-Key` for authenticated scopes ONLY if `req.url()` passes `policy`.
pub fn authorize(
    req: &mut reqwest::Request,
    scope: &SessionScope,
    credentials: &dyn CredentialStore,
    policy: &OriginPolicy,
) -> Result<(), ProviderError> {
    req.headers_mut().remove(&X_API_KEY_HEADER);

    if !policy.is_authorized_api_url(req.url()) {
        return Err(ProviderError::OriginRejected);
    }

    match &scope.kind {
        SessionKind::Guest => Ok(()),
        SessionKind::Account(account_id) => {
            let secret = credentials
                .load(account_id)
                .map_err(|_| ProviderError::Unauthorized)?
                .ok_or(ProviderError::Unauthorized)?;
            if secret.is_empty() {
                return Err(ProviderError::Unauthorized);
            }
            let mut value = HeaderValue::from_str(secret.expose_secret())
                .map_err(|_| ProviderError::Unauthorized)?;
            value.set_sensitive(true);
            req.headers_mut().insert(X_API_KEY_HEADER, value);
            Ok(())
        }
    }
}

/// Returns `true` when two URLs share scheme, host, and effective port.
pub fn same_origin(a: &Url, b: &Url) -> bool {
    let scheme_ok = a.scheme().eq_ignore_ascii_case(b.scheme());
    let host_ok = a
        .host_str()
        .zip(b.host_str())
        .is_some_and(|(ha, hb)| ha.eq_ignore_ascii_case(hb));
    let port_a = a.port_or_known_default();
    let port_b = b.port_or_known_default();
    scheme_ok && host_ok && port_a.is_some() && port_a == port_b
}
