use reqwest::header::{HeaderName, HeaderValue};
#[cfg(any(test, feature = "test-util"))]
use url::Host;
use url::Url;

use crate::domain::{CredentialStore, SessionKind, SessionScope};
use crate::provider::error::ProviderError;

const X_API_KEY_HEADER: HeaderName = HeaderName::from_static("x-api-key");

/// Origin whitelist used to gate attachment of `X-API-Key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginPolicy {
    scheme: String,
    host: String,
    port: u16,
    api_path_prefix: String,
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

    /// Constructs a test origin policy bound strictly to `base_url`'s scheme, host, port, and
    /// path prefix. The base must be a literal loopback HTTP(S) origin with no userinfo, query,
    /// fragment, or explicit non-default port ambiguity.
    #[cfg(any(test, feature = "test-util"))]
    pub fn from_api_base(base_url: &Url) -> Result<Self, ProviderError> {
        let (scheme, host, port) = test_origin_parts(base_url)?;
        let mut prefix = base_url.path().to_string();
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        Ok(Self {
            scheme,
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

/// Validates a test-only origin and returns its lowercased scheme, literal loopback host and
/// effective port. Rejects anything a loopback fixture cannot legitimately be.
#[cfg(any(test, feature = "test-util"))]
fn test_origin_parts(url: &Url) -> Result<(String, String, u16), ProviderError> {
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ProviderError::OriginRejected);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ProviderError::OriginRejected);
    }
    let scheme = url.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(ProviderError::OriginRejected);
    }
    let host = match url.host() {
        Some(Host::Ipv4(ip)) if ip.is_loopback() => ip.to_string(),
        Some(Host::Ipv6(ip)) if ip.is_loopback() => url
            .host_str()
            .ok_or(ProviderError::OriginRejected)?
            .to_ascii_lowercase(),
        _ => return Err(ProviderError::OriginRejected),
    };
    let port = url
        .port_or_known_default()
        .ok_or(ProviderError::OriginRejected)?;
    Ok((scheme, host, port))
}

/// Allowlist applied to every image request and redirect hop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ImageOriginPolicy {
    /// HTTPS on port 443, exact `w.wallhaven.cc` or `th.wallhaven.cc` only.
    Production,
    /// The single literal loopback origin supplied by a test configuration.
    #[cfg(any(test, feature = "test-util"))]
    Test {
        scheme: String,
        host: String,
        port: u16,
    },
    /// Test configurations without an explicit image origin allow no image requests.
    #[cfg(any(test, feature = "test-util"))]
    Disabled,
}

impl ImageOriginPolicy {
    pub(crate) fn production() -> Self {
        Self::Production
    }

    /// Accepts one explicit literal loopback HTTP(S) origin with a root path.
    #[cfg(any(test, feature = "test-util"))]
    pub(crate) fn from_test_origin(origin: &Url) -> Result<Self, ProviderError> {
        let (scheme, host, port) = test_origin_parts(origin)?;
        if origin.path() != "/" {
            return Err(ProviderError::OriginRejected);
        }
        Ok(Self::Test { scheme, host, port })
    }

    pub(crate) fn is_authorized_image_url(&self, url: &Url) -> bool {
        if !url.username().is_empty() || url.password().is_some() {
            return false;
        }
        let Some(host) = url.host_str() else {
            return false;
        };
        match self {
            Self::Production => {
                url.scheme().eq_ignore_ascii_case("https")
                    && url.port_or_known_default() == Some(443)
                    && matches!(
                        host.to_ascii_lowercase().as_str(),
                        "w.wallhaven.cc" | "th.wallhaven.cc"
                    )
            }
            #[cfg(any(test, feature = "test-util"))]
            Self::Test { scheme, host, port } => {
                url.scheme().eq_ignore_ascii_case(scheme)
                    && host.eq_ignore_ascii_case(url.host_str().unwrap_or_default())
                    && url.port_or_known_default() == Some(*port)
            }
            #[cfg(any(test, feature = "test-util"))]
            Self::Disabled => false,
        }
    }
}

/// Attaches `X-API-Key` for authenticated scopes ONLY if `req.url()` passes `policy`.
///
/// The key is loaded per attempt, converted once into a sensitive header value, and the loaded
/// secret is released (and zeroized on drop) before the header is attached.
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
            let value = {
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
                value
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_image_origin_policy_is_exact() {
        let production = OriginPolicy::production();
        let image = ImageOriginPolicy::production();

        for allowed in [
            "https://w.wallhaven.cc/full/ab/wallhaven-abc123.jpg",
            "https://th.wallhaven.cc/small/ab/abc123.jpg",
        ] {
            let url = Url::parse(allowed).unwrap();
            assert!(
                image.is_authorized_image_url(&url),
                "approved image host rejected: {allowed}"
            );
        }

        for rejected in [
            "http://w.wallhaven.cc/full/ab/x.jpg",
            "https://w.wallhaven.cc:8443/full/ab/x.jpg",
            "https://wallhaven.cc/full/ab/x.jpg",
            "https://evilw.wallhaven.cc/full/ab/x.jpg",
            "https://w.wallhaven.cc.evil.example/full/ab/x.jpg",
            "https://sub.w.wallhaven.cc/full/ab/x.jpg",
            "https://user:pass@w.wallhaven.cc/full/ab/x.jpg",
            "http://127.0.0.1:8080/full/ab/x.jpg",
            "https://127.0.0.1/full/ab/x.jpg",
            "https://[::1]:8080/full/ab/x.jpg",
        ] {
            let url = Url::parse(rejected).unwrap();
            assert!(
                !image.is_authorized_image_url(&url),
                "production image policy accepted {rejected}"
            );
        }

        // The production API policy still rejects image hosts and non-API paths.
        for rejected in [
            "https://w.wallhaven.cc/full/ab/x.jpg",
            "https://wallhaven.cc:8443/api/v1/search",
            "http://wallhaven.cc/api/v1/search",
        ] {
            let url = Url::parse(rejected).unwrap();
            assert!(
                !production.is_authorized_api_url(&url),
                "accepted {rejected}"
            );
        }

        // A test origin accepts only its own configured loopback origin.
        let origin = Url::parse("http://127.0.0.1:45123").unwrap();
        let test = ImageOriginPolicy::from_test_origin(&origin).unwrap();
        for accepted in [
            "http://127.0.0.1:45123/thumb/1.jpg",
            "http://127.0.0.1:45123/deep/path/image.jpg",
        ] {
            assert!(
                test.is_authorized_image_url(&Url::parse(accepted).unwrap()),
                "test image origin rejected {accepted}"
            );
        }
        for rejected in [
            "http://127.0.0.1:45124/thumb/1.jpg",
            "http://127.0.0.2:45123/thumb/1.jpg",
            "https://127.0.0.1:45123/thumb/1.jpg",
            "http://[::1]:45123/thumb/1.jpg",
            "http://evil.example/thumb/1.jpg",
        ] {
            assert!(
                !test.is_authorized_image_url(&Url::parse(rejected).unwrap()),
                "test image origin accepted {rejected}"
            );
        }
    }
}
