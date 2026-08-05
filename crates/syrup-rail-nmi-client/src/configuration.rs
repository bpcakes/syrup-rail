use std::{fmt, net::IpAddr, time::Duration};

use reqwest::header::HeaderValue;
use url::{Host, Url};
use zeroize::Zeroizing;

pub(crate) const MAX_CREDENTIAL_BYTES: usize = 4_096;

// Eight concurrent report bodies cap crate-owned buffered XML at 32 MiB before
// parser/tree overhead. The admission budget is report-specific so ordinary
// payment mutations and small queries never queue behind large report work.
pub(crate) const MAX_NMI_CONCURRENT_REPORTS: usize = 8;

// Idle connections retain per-origin OS and TLS state but do not own report
// response buffers. Keep this transport-reuse ceiling independent from report
// admission so memory-policy changes cannot silently alter payment latency.
pub(crate) const MAX_NMI_IDLE_CONNECTIONS_PER_HOST: usize = 32;

/// A validated NMI API root.
#[derive(Clone)]
pub struct Endpoint {
    pub(crate) url: Url,
}

/// Private API and query credentials owned in zeroizing buffers.
pub struct Credentials {
    pub(crate) private_api_key: Zeroizing<String>,
    pub(crate) query_security_key: Zeroizing<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigurationError {
    #[error("NMI HTTP client could not be configured")]
    HttpClient,
    #[error("NMI endpoint must be a credential-free root URL")]
    EndpointInvalid,
    #[error("NMI production endpoint must use HTTPS")]
    EndpointMustUseHttps,
    #[error("NMI local endpoint must use HTTP on a loopback or localhost host")]
    EndpointMustUseLoopbackHttp,
    #[error("NMI loopback HTTP transport was not enabled")]
    LoopbackHttpDisabled,
    #[error("NMI private API key is required")]
    PrivateApiKeyRequired,
    #[error("NMI query security key is required")]
    QuerySecurityKeyRequired,
    #[error("NMI credential is too long")]
    CredentialTooLong,
    #[error("NMI private API key is invalid for the Authorization header")]
    PrivateApiKeyInvalidHeader,
}

impl Endpoint {
    /// Parses a credential-free HTTPS root URL.
    ///
    /// Merchant host allowlisting belongs to the application and intentionally
    /// is not performed by this reusable package.
    pub fn parse_https(value: impl AsRef<str>) -> Result<Self, ConfigurationError> {
        let url = parse_endpoint(value.as_ref())?;
        if url.scheme() != "https" {
            return Err(ConfigurationError::EndpointMustUseHttps);
        }
        Ok(Self { url })
    }

    /// Parses an HTTP root URL whose host is a loopback IP or exactly
    /// `localhost`.
    ///
    /// This constructor is explicit so production code cannot accidentally
    /// weaken an HTTPS endpoint while local tests remain possible.
    pub fn parse_loopback_http(value: impl AsRef<str>) -> Result<Self, ConfigurationError> {
        let url = parse_endpoint(value.as_ref())?;
        if url.scheme() != "http" || !is_loopback_host(&url) {
            return Err(ConfigurationError::EndpointMustUseLoopbackHttp);
        }
        Ok(Self { url })
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("origin", &self.url.origin().ascii_serialization())
            .finish()
    }
}

impl Credentials {
    pub fn new(
        private_api_key: impl Into<Zeroizing<String>>,
        query_security_key: impl Into<Zeroizing<String>>,
    ) -> Result<Self, ConfigurationError> {
        let private_api_key = private_api_key.into();
        let query_security_key = query_security_key.into();
        if private_api_key.len() > MAX_CREDENTIAL_BYTES
            || query_security_key.len() > MAX_CREDENTIAL_BYTES
        {
            return Err(ConfigurationError::CredentialTooLong);
        }
        if private_api_key.trim().is_empty() {
            return Err(ConfigurationError::PrivateApiKeyRequired);
        }
        if query_security_key.trim().is_empty() {
            return Err(ConfigurationError::QuerySecurityKeyRequired);
        }
        HeaderValue::from_str(&private_api_key)
            .map_err(|_| ConfigurationError::PrivateApiKeyInvalidHeader)?;
        Ok(Self {
            private_api_key,
            query_security_key,
        })
    }

    pub(crate) fn private_api_key_header(&self) -> HeaderValue {
        let mut value = HeaderValue::from_str(self.private_api_key.as_str())
            .expect("private API key was validated at construction");
        value.set_sensitive(true);
        value
    }
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("private_api_key", &"[redacted]")
            .field("query_security_key", &"[redacted]")
            .finish()
    }
}

pub(crate) fn configured_http_client(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, ConfigurationError> {
    builder
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(5))
        .pool_max_idle_per_host(MAX_NMI_IDLE_CONNECTIONS_PER_HOST)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .build()
        .map_err(|_| ConfigurationError::HttpClient)
}

fn parse_endpoint(value: &str) -> Result<Url, ConfigurationError> {
    let url = Url::parse(value).map_err(|_| ConfigurationError::EndpointInvalid)?;
    if !url.has_host()
        || url.cannot_be_a_base()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigurationError::EndpointInvalid);
    }
    Ok(url)
}

fn is_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => IpAddr::V4(address).is_loopback(),
        Some(Host::Ipv6(address)) => IpAddr::V6(address).is_loopback(),
        None => false,
    }
}
