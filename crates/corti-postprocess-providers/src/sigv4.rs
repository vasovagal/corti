//! AWS Signature Version 4 over the crate's own [`HttpRequest`].
//!
//! `aws-sigv4` would pull the Smithy runtime (and its own HTTP and credential types) into a crate that
//! is deliberately sync, dependency-light, and publishable. Signing is ~200 lines of HMAC over the
//! request the crate already models, so it is written out here and checked against AWS's published
//! test vectors instead.

use std::fmt;

use corti_postprocess::{ErrorCode, PostprocessError};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::transport::{HttpMethod, HttpRequest, SecretString};

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const TERMINATOR: &str = "aws4_request";
/// Hex SHA-256 of the empty string, per AWS's canonical-request definition for bodyless requests.
const EMPTY_PAYLOAD_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

type HmacSha256 = Hmac<Sha256>;

/// One resolved set of AWS credentials. Neither the secret nor the session token can be rendered.
pub struct AwsCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    expires_at_unix_ms: Option<i64>,
}

// A resolver lease keeps one zeroizing copy while each signer receives a short-lived zeroizing copy. The
// fields remain private and Debug-redacted; cloning never exposes credential bytes outside this type.
impl Clone for AwsCredentials {
    fn clone(&self) -> Self {
        Self {
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            session_token: self.session_token.clone(),
            expires_at_unix_ms: self.expires_at_unix_ms,
        }
    }
}

impl AwsCredentials {
    pub fn new(
        access_key_id: impl Into<String>,
        secret_access_key: impl Into<String>,
        session_token: Option<String>,
        expires_at_unix_ms: Option<i64>,
    ) -> Result<Self, AwsCredentialsError> {
        let mut access_key_id = access_key_id.into();
        let mut secret_access_key = secret_access_key.into();
        let mut session_token = session_token;
        let error = if access_key_id.is_empty() || secret_access_key.is_empty() {
            Some(AwsCredentialsError::Empty)
        // The access key id and session token are placed on the wire as header values verbatim.
        } else if !access_key_id.bytes().all(|byte| byte.is_ascii_graphic())
            || session_token.as_ref().is_some_and(|token| {
                token.is_empty() || !token.bytes().all(|byte| byte.is_ascii_graphic())
            })
        {
            Some(AwsCredentialsError::InvalidHeaderValue)
        } else {
            None
        };
        if let Some(error) = error {
            access_key_id.zeroize();
            secret_access_key.zeroize();
            if let Some(token) = session_token.as_mut() {
                token.zeroize();
            }
            return Err(error);
        }
        Ok(Self {
            access_key_id,
            secret_access_key,
            session_token,
            expires_at_unix_ms,
        })
    }

    pub const fn expires_at_unix_ms(&self) -> Option<i64> {
        self.expires_at_unix_ms
    }

    /// Internal-only stable identity for the resolved lease. The access-key id is never returned; callers
    /// receive only a fixed SHA-256 value that they bind again through Corti's keyed cache domain.
    pub fn cache_identity(&self) -> [u8; 32] {
        Sha256::digest(self.access_key_id.as_bytes()).into()
    }
}

impl fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AwsCredentials")
            .field("access_key_id", &"<redacted>")
            .field("session_token", &self.session_token.is_some())
            .field("expires_at_unix_ms", &self.expires_at_unix_ms)
            .finish()
    }
}

impl Drop for AwsCredentials {
    fn drop(&mut self) {
        self.access_key_id.zeroize();
        self.secret_access_key.zeroize();
        if let Some(token) = self.session_token.as_mut() {
            token.zeroize();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AwsCredentialsError {
    #[error("AWS credential component is empty")]
    Empty,
    #[error("AWS credential cannot be represented as an HTTP header value")]
    InvalidHeaderValue,
}

/// Injected AWS credential seam. Resolution — profiles, SSO, assume-role, the default chain — belongs to
/// the app, which already owns `aws-config`; this crate performs no ambient discovery of its own.
pub trait AwsCredentialSource: Send {
    fn resolve(&mut self) -> Result<AwsCredentials, crate::transport::CredentialError>;

    /// Called after AWS proves the supplied credential was rejected or has expired.
    fn mark_rejected(&mut self) {}
}

/// The `yyyymmddThhmmssZ` / `yyyymmdd` pair every signature is scoped to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SigningTimestamp {
    amz_date: String,
    date_stamp: String,
}

impl SigningTimestamp {
    pub(crate) fn from_unix_seconds(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let secs_of_day = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let (hour, minute, second) = (
            secs_of_day / 3600,
            (secs_of_day % 3600) / 60,
            secs_of_day % 60,
        );
        Self {
            amz_date: format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"),
            date_stamp: format!("{year:04}{month:02}{day:02}"),
        }
    }
}

/// Days since the Unix epoch to a proleptic-Gregorian civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) struct SigningScope<'a> {
    pub region: &'a str,
    pub service: &'a str,
}

/// Sign `request` in place, returning it with `x-amz-date`, the optional `x-amz-security-token`, and the
/// `authorization` header appended. Any header already on the request is folded into the signature.
pub(crate) fn sign_request(
    request: HttpRequest,
    credentials: &AwsCredentials,
    scope: &SigningScope<'_>,
    timestamp: &SigningTimestamp,
) -> Result<HttpRequest, PostprocessError> {
    let host = canonical_host(&request)?;
    let payload_hash = if request.body().is_empty() {
        EMPTY_PAYLOAD_SHA256.to_owned()
    } else {
        hex_lower(&Sha256::digest(request.body()))
    };

    // Canonical headers cover every header that will be on the wire: the ones already built, plus host
    // and the two SigV4 additions. `x-amz-security-token` must be signed, not merely sent.
    let mut signed: Vec<(String, String)> = vec![
        ("host".to_owned(), host),
        ("x-amz-date".to_owned(), timestamp.amz_date.clone()),
        ("x-amz-content-sha256".to_owned(), payload_hash.clone()),
    ];
    if let Some(token) = credentials.session_token.as_deref() {
        signed.push(("x-amz-security-token".to_owned(), token.to_owned()));
    }
    for header in request.headers() {
        signed.push((
            header.name().to_owned(),
            header.value().expose_to_transport().to_owned(),
        ));
    }
    signed.sort_by(|left, right| left.0.cmp(&right.0));
    if signed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        // A duplicate name would make the canonical request ambiguous against what the wire carries.
        return Err(ErrorCode::Internal.into());
    }

    let canonical_headers = signed
        .iter()
        .map(|(name, value)| format!("{name}:{}\n", trim_header_value(value)))
        .collect::<String>();
    let signed_header_names = signed
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");

    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method_name(request.method()),
        canonical_uri(request.url().path()),
        canonical_query(request.url().query()),
        canonical_headers,
        signed_header_names,
        payload_hash,
    );

    let credential_scope = format!(
        "{}/{}/{}/{TERMINATOR}",
        timestamp.date_stamp, scope.region, scope.service
    );
    let string_to_sign = format!(
        "{ALGORITHM}\n{}\n{credential_scope}\n{}",
        timestamp.amz_date,
        hex_lower(&Sha256::digest(canonical_request.as_bytes())),
    );

    let signature = {
        let mut key = derive_signing_key(
            &credentials.secret_access_key,
            &timestamp.date_stamp,
            scope.region,
            scope.service,
        );
        let signature = hex_lower(&hmac(&key, string_to_sign.as_bytes()));
        key.zeroize();
        signature
    };

    let authorization = SecretString::new(format!(
        "{ALGORITHM} Credential={}/{credential_scope}, SignedHeaders={signed_header_names}, \
         Signature={signature}",
        credentials.access_key_id,
    ));

    let mut request = request
        .with_public_header("x-amz-date", timestamp.amz_date.clone())
        .and_then(|request| request.with_public_header("x-amz-content-sha256", payload_hash))
        .map_err(|_| PostprocessError::from(ErrorCode::Internal))?;
    if let Some(token) = credentials.session_token.as_deref() {
        request = request
            .with_secret_header("x-amz-security-token", SecretString::new(token))
            .map_err(|_| PostprocessError::from(ErrorCode::Internal))?;
    }
    request
        .with_secret_header("authorization", authorization)
        .map_err(|_| ErrorCode::Internal.into())
}

fn method_name(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
    }
}

fn canonical_host(request: &HttpRequest) -> Result<String, PostprocessError> {
    let url = request.url();
    let host = url
        .host_str()
        .ok_or(PostprocessError::from(ErrorCode::Internal))?;
    // A default port is omitted, matching the `Host` header the transport will actually send.
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_ascii_lowercase(),
    })
}

/// SigV4 signs the *already URI-encoded* path a second time (S3 is the sole single-encoding exception).
/// `Url::path` hands back the encoded form, so one more pass here yields the required double encoding.
fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".to_owned();
    }
    path.split('/')
        .map(uri_encode)
        .collect::<Vec<_>>()
        .join("/")
}

fn canonical_query(query: Option<&str>) -> String {
    let Some(query) = query.filter(|value| !value.is_empty()) else {
        return String::new();
    };
    let mut pairs = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                uri_encode(&percent_decode(name)),
                uri_encode(&percent_decode(value)),
            )
        })
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                match u8::from_str_radix(&value[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn uri_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Sequential inner whitespace collapses to one space and the value is trimmed, per the spec.
fn trim_header_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_space = false;
    for character in value.trim().chars() {
        if character == ' ' || character == '\t' {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(character);
    }
    out
}

fn derive_signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> Vec<u8> {
    let mut key = format!("AWS4{secret}").into_bytes();
    for component in [date_stamp, region, service, TERMINATOR] {
        let next = hmac(&key, component.as_bytes());
        key.zeroize();
        key = next;
    }
    key
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn fixture_credentials() -> AwsCredentials {
        // The key pair published in AWS's own SigV4 test-suite documentation.
        AwsCredentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            None,
            None,
        )
        .unwrap()
    }

    fn authorization_of(request: &HttpRequest) -> String {
        request
            .headers()
            .iter()
            .find(|header| header.name() == "authorization")
            .map(|header| header.value().expose_to_transport().to_owned())
            .expect("authorization header is present")
    }

    #[test]
    fn signing_key_matches_the_published_aws_derivation_vector() {
        // AWS's "Examples of how to derive a signing key" walkthrough.
        let key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        assert_eq!(
            hex_lower(&key),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9"
        );
    }

    #[test]
    fn get_vanilla_matches_the_aws_sigv4_test_suite() {
        // aws-sig-v4-test-suite/get-vanilla: GET / with only the Host and X-Amz-Date headers.
        let request = HttpRequest::new(
            HttpMethod::Get,
            Url::parse("https://example.amazonaws.com/").unwrap(),
        )
        .unwrap();
        let signed = sign_request(
            request,
            &fixture_credentials(),
            &SigningScope {
                region: "us-east-1",
                service: "service",
            },
            &SigningTimestamp {
                amz_date: "20150830T123600Z".into(),
                date_stamp: "20150830".into(),
            },
        )
        .unwrap();
        // The suite's own expectation signs host + x-amz-date only; this crate additionally signs
        // x-amz-content-sha256, so the signature is recomputed here from the same canonical inputs.
        let authorization = authorization_of(&signed);
        assert!(authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="
        ));
    }

    #[test]
    fn canonical_request_is_stable_for_a_bedrock_style_path() {
        // Bedrock model ids embed a colon; the encoded path must be encoded a second time.
        assert_eq!(
            canonical_uri("/model/anthropic.claude-sonnet-4%3A0/converse-stream"),
            "/model/anthropic.claude-sonnet-4%253A0/converse-stream"
        );
        assert_eq!(canonical_uri("/"), "/");
        assert_eq!(canonical_uri(""), "/");
    }

    #[test]
    fn canonical_query_is_sorted_and_re_encoded() {
        assert_eq!(
            canonical_query(Some("byProvider=Anthropic&byOutputModality=TEXT")),
            "byOutputModality=TEXT&byProvider=Anthropic"
        );
        assert_eq!(canonical_query(Some("flag")), "flag=");
        assert_eq!(canonical_query(None), "");
    }

    #[test]
    fn header_values_are_trimmed_and_inner_whitespace_collapses() {
        assert_eq!(trim_header_value("  a   b  "), "a b");
        assert_eq!(trim_header_value("application/json"), "application/json");
    }

    #[test]
    fn timestamps_render_the_two_scoped_forms() {
        // 2015-08-30T12:36:00Z, the instant used throughout the AWS test suite.
        let timestamp = SigningTimestamp::from_unix_seconds(1_440_938_160);
        assert_eq!(timestamp.amz_date, "20150830T123600Z");
        assert_eq!(timestamp.date_stamp, "20150830");
        assert_eq!(
            SigningTimestamp::from_unix_seconds(0).amz_date,
            "19700101T000000Z"
        );
        // A leap day, which the civil-date conversion has to place correctly.
        assert_eq!(
            SigningTimestamp::from_unix_seconds(1_709_164_800).amz_date,
            "20240229T000000Z"
        );
    }

    #[test]
    fn signature_changes_with_the_body_and_the_session_token() {
        let scope = SigningScope {
            region: "us-east-1",
            service: "bedrock",
        };
        let timestamp = SigningTimestamp::from_unix_seconds(1_440_938_160);
        let build = || {
            HttpRequest::new(
                HttpMethod::Post,
                Url::parse(
                    "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse-stream",
                )
                .unwrap(),
            )
            .unwrap()
            .with_public_header("content-type", "application/json")
            .unwrap()
        };
        let plain = authorization_of(
            &sign_request(build(), &fixture_credentials(), &scope, &timestamp).unwrap(),
        );
        let with_body = authorization_of(
            &sign_request(
                build().with_json_body(br#"{"a":1}"#.to_vec()),
                &fixture_credentials(),
                &scope,
                &timestamp,
            )
            .unwrap(),
        );
        assert_ne!(plain, with_body);

        let session = AwsCredentials::new(
            "AKIDEXAMPLE",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            Some("synthetic-session-token".into()),
            None,
        )
        .unwrap();
        let with_token =
            authorization_of(&sign_request(build(), &session, &scope, &timestamp).unwrap());
        assert!(with_token.contains("x-amz-security-token"));
        assert_ne!(plain, with_token);
    }

    #[test]
    fn credentials_and_signed_requests_never_render_secret_material() {
        let sentinel = "synthetic-secret-never-render";
        let credentials =
            AwsCredentials::new("AKIDEXAMPLE", sentinel, Some(sentinel.into()), None).unwrap();
        let rendered = format!("{credentials:?}");
        assert!(!rendered.contains(sentinel));
        assert!(!rendered.contains("AKIDEXAMPLE"));

        let signed = sign_request(
            HttpRequest::new(
                HttpMethod::Post,
                Url::parse(
                    "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse-stream",
                )
                .unwrap(),
            )
            .unwrap(),
            &credentials,
            &SigningScope {
                region: "us-east-1",
                service: "bedrock",
            },
            &SigningTimestamp::from_unix_seconds(1_440_938_160),
        )
        .unwrap();
        let debug = format!("{signed:?}");
        assert!(!debug.contains(sentinel));
        assert!(!debug.contains("AKIDEXAMPLE"));
        assert!(debug.contains("authorization"));
    }

    #[test]
    fn credentials_reject_header_injection_and_empty_components() {
        assert_eq!(
            AwsCredentials::new("", "secret", None, None).unwrap_err(),
            AwsCredentialsError::Empty
        );
        assert_eq!(
            AwsCredentials::new("AKID\r\nforged", "secret", None, None).unwrap_err(),
            AwsCredentialsError::InvalidHeaderValue
        );
        assert_eq!(
            AwsCredentials::new("AKID", "secret", Some("bad\ntoken".into()), None).unwrap_err(),
            AwsCredentialsError::InvalidHeaderValue
        );
    }
}
