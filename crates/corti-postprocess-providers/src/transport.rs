use std::{
    error::Error as _,
    fmt,
    io::{self, Read},
    time::{Duration, Instant},
};

use corti_postprocess::CancellationToken;
use thiserror::Error;
use url::Url;
use zeroize::Zeroize;

const MAX_API_KEY_BYTES: usize = 16 * 1024;
const UREQ_READ_CHUNK_BYTES: usize = 16 * 1024;

/// An API key owned by the native provider edge.
///
/// This type deliberately implements neither `Clone`, `Display`, nor serialization. Its debug form is
/// redacted and its allocation is zeroized on drop.
///
/// ```compile_fail
/// use corti_postprocess_providers::ApiKey;
/// let key = ApiKey::new("synthetic-key").unwrap();
/// let _ = serde_json::to_string(&key); // secrets never implement `Serialize`
/// ```
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(value: impl Into<String>) -> Result<Self, ApiKeyError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ApiKeyError::Empty);
        }
        if value.len() > MAX_API_KEY_BYTES {
            return Err(ApiKeyError::TooLong);
        }
        if !value.bytes().all(|byte| byte.is_ascii_graphic()) {
            return Err(ApiKeyError::InvalidHeaderValue);
        }
        Ok(Self(value))
    }

    fn into_secret_header(self, prefix: &str) -> SecretString {
        let mut value = String::with_capacity(prefix.len().saturating_add(self.0.len()));
        value.push_str(prefix);
        value.push_str(&self.0);
        // `self` is dropped and its original allocation is zeroized after this copy is made. The new
        // allocation is independently zeroized by `SecretString`.
        SecretString(value)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

impl Drop for ApiKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ApiKeyError {
    #[error("API key is empty")]
    Empty,
    #[error("API key is too long")]
    TooLong,
    #[error("API key cannot be represented as an HTTP header value")]
    InvalidHeaderValue,
}

/// Sanitized credential-resolution failures. There is no provider or key text field by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CredentialError {
    #[error("credential is absent")]
    Absent,
    #[error("credential was rejected")]
    Rejected,
    #[error("credential source is unavailable")]
    Unavailable,
}

/// Injected direct-API credential seam. Implementations may use Keychain or workload identity, but no
/// environment-variable or credential-file implementation is provided by this crate.
pub trait ApiKeySource: Send {
    fn resolve(&mut self) -> Result<ApiKey, CredentialError>;

    /// Called after a provider proves that the supplied credential was rejected (HTTP 401).
    fn mark_rejected(&mut self) {}
}

/// A secret HTTP value. It cannot be cloned, displayed, or serialized.
pub struct SecretString(String);

impl SecretString {
    /// Exposes the value only to the injected transport that must place it on the wire.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

pub enum HttpHeaderValue {
    Public(String),
    Secret(SecretString),
}

impl HttpHeaderValue {
    pub fn expose_to_transport(&self) -> &str {
        match self {
            Self::Public(value) => value,
            Self::Secret(value) => value.expose_secret(),
        }
    }

    pub const fn is_secret(&self) -> bool {
        matches!(self, Self::Secret(_))
    }
}

impl fmt::Debug for HttpHeaderValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Public(value) => f.debug_tuple("Public").field(value).finish(),
            Self::Secret(_) => f.write_str("Secret(<redacted>)"),
        }
    }
}

pub struct HttpHeader {
    name: String,
    value: HttpHeaderValue,
}

impl HttpHeader {
    pub fn public(
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, HttpBuildError> {
        Self::new(name.into(), HttpHeaderValue::Public(value.into()))
    }

    pub fn secret(name: impl Into<String>, value: SecretString) -> Result<Self, HttpBuildError> {
        Self::new(name.into(), HttpHeaderValue::Secret(value))
    }

    fn new(name: String, value: HttpHeaderValue) -> Result<Self, HttpBuildError> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(HttpBuildError::InvalidHeaderName);
        }
        if value
            .expose_to_transport()
            .bytes()
            .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
        {
            return Err(HttpBuildError::InvalidHeaderValue);
        }
        Ok(Self {
            name: name.to_ascii_lowercase(),
            value,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &HttpHeaderValue {
        &self.value
    }
}

impl fmt::Debug for HttpHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpHeader")
            .field("name", &self.name)
            .field("secret", &self.value.is_secret())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

impl HttpMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// Provider-neutral HTTP request. Debug output excludes all header values and the content-bearing body.
pub struct HttpRequest {
    method: HttpMethod,
    url: Url,
    headers: Vec<HttpHeader>,
    body: Vec<u8>,
    timeout: Option<Duration>,
}

impl HttpRequest {
    pub fn new(method: HttpMethod, url: Url) -> Result<Self, HttpBuildError> {
        if !matches!(url.scheme(), "http" | "https") || url.cannot_be_a_base() {
            return Err(HttpBuildError::InvalidUrl);
        }
        Ok(Self {
            method,
            url,
            headers: Vec::new(),
            body: Vec::new(),
            timeout: None,
        })
    }

    pub fn with_public_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Self, HttpBuildError> {
        self.headers.push(HttpHeader::public(name, value)?);
        Ok(self)
    }

    pub fn with_api_key_header(
        mut self,
        name: impl Into<String>,
        key: ApiKey,
        prefix: &str,
    ) -> Result<Self, HttpBuildError> {
        self.headers
            .push(HttpHeader::secret(name, key.into_secret_header(prefix))?);
        Ok(self)
    }

    pub fn with_json_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout.max(Duration::from_micros(1)));
        self
    }

    pub const fn method(&self) -> HttpMethod {
        self.method
    }

    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn headers(&self) -> &[HttpHeader] {
        &self.headers
    }

    /// Content is exposed only at the transport boundary. `Debug` never includes it.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub const fn timeout(&self) -> Option<Duration> {
        self.timeout
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(HttpHeader::name)
                    .collect::<Vec<_>>(),
            )
            .field("body_bytes", &self.body.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum HttpBuildError {
    #[error("invalid HTTP URL")]
    InvalidUrl,
    #[error("invalid HTTP header name")]
    InvalidHeaderName,
    #[error("invalid HTTP header value")]
    InvalidHeaderValue,
}

/// Whether a failed exchange is proven safe for the one permitted automatic retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestDelivery {
    /// The transport proves that zero request-body bytes could have reached the provider.
    NotSent,
    /// Request bytes may have reached the provider, but no response was established.
    MayHaveBeenSent,
    /// Response headers or bytes were observed.
    ResponseStarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportErrorKind {
    Network,
    Timeout,
    Canceled,
    Protocol,
}

/// Sanitized transport failure. It intentionally stores no URL, body, credential, or free-form message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("HTTP transport failed ({kind:?}, {delivery:?})")]
pub struct TransportError {
    pub kind: TransportErrorKind,
    pub delivery: RequestDelivery,
}

impl TransportError {
    pub const fn new(kind: TransportErrorKind, delivery: RequestDelivery) -> Self {
        Self { kind, delivery }
    }

    pub const fn not_sent(kind: TransportErrorKind) -> Self {
        Self::new(kind, RequestDelivery::NotSent)
    }
}

pub trait HttpResponseBody: Send {
    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError>;

    /// Best-effort abort hook. Implementations must not claim that calling it prevented remote billing.
    fn cancel(&mut self) {}
}

pub struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Box<dyn HttpResponseBody>,
}

impl HttpResponse {
    pub fn new(
        status: u16,
        headers: impl IntoIterator<Item = (String, String)>,
        body: Box<dyn HttpResponseBody>,
    ) -> Self {
        Self {
            status,
            headers: headers
                .into_iter()
                .map(|(name, value)| (name.to_ascii_lowercase(), value))
                .collect(),
            body,
        }
    }

    pub const fn status(&self) -> u16 {
        self.status
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body_mut(&mut self) -> &mut dyn HttpResponseBody {
        &mut *self.body
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("body", &"<stream>")
            .finish()
    }
}

/// Injected HTTP seam. Tests use scripted implementations; no adapter reads ambient network settings or
/// credentials by itself.
pub trait HttpTransport: Send {
    fn send(
        &mut self,
        request: &HttpRequest,
        cancel: &CancellationToken,
    ) -> Result<HttpResponse, TransportError>;
}

/// Injected monotonic clock used for deadlines and latency accounting.
pub trait Clock: Send + Sync {
    fn monotonic_micros(&self) -> u64;
}

impl<T: Clock + ?Sized> Clock for std::sync::Arc<T> {
    fn monotonic_micros(&self) -> u64 {
        (**self).monotonic_micros()
    }
}

#[derive(Debug)]
pub struct SystemClock {
    epoch: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn monotonic_micros(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

/// Production blocking HTTP implementation. It never resolves credentials; callers must inject secret
/// headers explicitly. Since ureq cannot prove how far a failed write progressed, its send failures are
/// conservatively marked `MayHaveBeenSent` and are never automatically retried.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self {
            // Direct-provider credentials must never follow a redirect to another origin. Fixed adapter
            // endpoints are HTTPS, so the concrete production transport rejects plaintext URLs as well.
            agent: ureq::AgentBuilder::new()
                .https_only(true)
                .redirects(0)
                .build(),
        }
    }
}

impl fmt::Debug for UreqTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UreqTransport(<configured>)")
    }
}

impl HttpTransport for UreqTransport {
    fn send(
        &mut self,
        request: &HttpRequest,
        cancel: &CancellationToken,
    ) -> Result<HttpResponse, TransportError> {
        if cancel.is_cancelled() {
            return Err(TransportError::not_sent(TransportErrorKind::Canceled));
        }

        let mut wire = self
            .agent
            .request(request.method().as_str(), request.url().as_str());
        for header in request.headers() {
            wire = wire.set(header.name(), header.value().expose_to_transport());
        }
        if let Some(timeout) = request.timeout() {
            wire = wire.timeout(timeout);
        }

        let result = if request.method() == HttpMethod::Get {
            wire.call()
        } else {
            wire.send_bytes(request.body())
        };
        let response = match result {
            Ok(response) | Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(error)) => {
                let timed_out = error
                    .source()
                    .and_then(|source| source.downcast_ref::<io::Error>())
                    .is_some_and(|source| {
                        matches!(
                            source.kind(),
                            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                        )
                    });
                let kind = if timed_out {
                    TransportErrorKind::Timeout
                } else {
                    TransportErrorKind::Network
                };
                return Err(TransportError::new(kind, RequestDelivery::MayHaveBeenSent));
            }
        };

        let status = response.status();
        let headers = response
            .headers_names()
            .into_iter()
            .filter_map(|name| response.header(&name).map(|value| (name, value.to_owned())))
            .collect::<Vec<_>>();
        Ok(HttpResponse::new(
            status,
            headers,
            Box::new(UreqBody {
                reader: response.into_reader(),
            }),
        ))
    }
}

struct UreqBody {
    reader: Box<dyn Read + Send + Sync + 'static>,
}

impl HttpResponseBody for UreqBody {
    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut bytes = vec![0; UREQ_READ_CHUNK_BYTES];
        loop {
            match self.reader.read(&mut bytes) {
                Ok(0) => return Ok(None),
                Ok(read) => {
                    bytes.truncate(read);
                    return Ok(Some(bytes));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let kind = match error.kind() {
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
                            TransportErrorKind::Timeout
                        }
                        _ => TransportErrorKind::Network,
                    };
                    return Err(TransportError::new(kind, RequestDelivery::ResponseStarted));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_and_request_debug_are_redacted() {
        let sentinel = "synthetic-key-never-render";
        let key = ApiKey::new(sentinel).unwrap();
        assert!(!format!("{key:?}").contains(sentinel));

        let request = HttpRequest::new(
            HttpMethod::Post,
            Url::parse("https://example.invalid/v1/test").unwrap(),
        )
        .unwrap()
        .with_api_key_header("authorization", key, "Bearer ")
        .unwrap()
        .with_json_body(br#"{"private":"synthetic-body-never-render"}"#.to_vec());
        let debug = format!("{request:?}");
        assert!(!debug.contains(sentinel));
        assert!(!debug.contains("synthetic-body"));
        assert!(debug.contains("body_bytes"));
    }

    #[test]
    fn api_keys_reject_header_injection() {
        assert_eq!(
            ApiKey::new("synthetic\r\nforged").unwrap_err(),
            ApiKeyError::InvalidHeaderValue
        );
    }
}
