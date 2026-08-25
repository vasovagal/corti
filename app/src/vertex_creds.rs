//! Google Application Default Credentials resolution for the Vertex adapter.
//!
//! `corti-postprocess-providers` performs no ambient discovery, so ADC is resolved here — the app reads the
//! ADC JSON (the `GOOGLE_APPLICATION_CREDENTIALS` override, then the well-known macOS path), exchanges it for
//! a short-lived access token over HTTPS, and hands only that memory-only token to the adapter. Per ADR 0015
//! §5 the refresh token / service-account private key never leave this module: they are never logged, never
//! persisted, and are zeroized after the exchange. Resolution never shells out to `gcloud`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use corti_postprocess::ErrorCode;
use corti_postprocess_providers::{
    AccessToken, AdcAccessToken, AdcAccessTokenSource, CredentialError, VertexResolutionOutcome,
};
use serde::Deserialize;
use zeroize::{Zeroize as _, Zeroizing};

/// Refresh this far ahead of expiry so a cached lease cannot lapse mid-request.
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;
const OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const JWT_BEARER_GRANT: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const SERVICE_ACCOUNT_ASSERTION_TTL_SECS: i64 = 60 * 60;

/// The non-secret half of a Vertex connection, snapshotted from `hosted.toml`. Used only to build the
/// adapter's project routing; ADC resolution itself needs none of it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct VertexConnectionConfig {
    pub(crate) project: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) quota_project: Option<String>,
}

/// Reads the connection's non-secret configuration off the live `hosted.toml` snapshot.
pub(crate) type ConfigSource = Arc<dyn Fn() -> VertexConnectionConfig + Send + Sync>;

/// Shared resolution state. The credential-arming driver calls [`Self::resolve_outcome`] on the auth thread
/// while the adapter calls [`Self::access_token`] on the request path; both go through one type so they
/// cannot disagree, and both refresh the same in-memory lease.
pub(crate) struct VertexAdcResolver {
    config: ConfigSource,
    file: Box<dyn AdcFileSource>,
    endpoint: Box<dyn TokenEndpoint>,
    cached: Mutex<Option<ResolvedToken>>,
    /// A rejection sticks to the ADC file fingerprint that earned it, so re-resolving does not silently
    /// report readiness again after Google refused the credential. Re-running `gcloud auth application-default
    /// login` rewrites the file (new fingerprint) and clears it.
    rejection: Mutex<Option<String>>,
}

impl VertexAdcResolver {
    pub(crate) fn production(config: ConfigSource) -> Arc<Self> {
        Self::new(
            config,
            Box::new(WellKnownAdcFile),
            Box::new(OauthTokenEndpoint::new()),
        )
    }

    fn new(
        config: ConfigSource,
        file: Box<dyn AdcFileSource>,
        endpoint: Box<dyn TokenEndpoint>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            file,
            endpoint,
            cached: Mutex::new(None),
            rejection: Mutex::new(None),
        })
    }

    pub(crate) fn config(&self) -> VertexConnectionConfig {
        (self.config)()
    }

    /// Secret-free projection for the credential-arming state machine.
    pub(crate) fn resolve_outcome(&self) -> VertexResolutionOutcome {
        match self.resolve_token() {
            Ok(resolved) => VertexResolutionOutcome::Ready {
                expires_at_unix_ms: resolved.expires_at_unix_ms,
            },
            Err(ResolveFailure::Absent | ResolveFailure::Unrecognized) => {
                VertexResolutionOutcome::Unarmed
            }
            Err(ResolveFailure::Rejected) => VertexResolutionOutcome::Rejected,
            Err(ResolveFailure::SupportGap | ResolveFailure::Local) => {
                VertexResolutionOutcome::Error {
                    code: ErrorCode::AuthUnarmed,
                }
            }
            Err(ResolveFailure::Timeout) => VertexResolutionOutcome::Error {
                code: ErrorCode::Timeout,
            },
            Err(ResolveFailure::Network) => VertexResolutionOutcome::Error {
                code: ErrorCode::Network,
            },
            Err(ResolveFailure::Provider) => VertexResolutionOutcome::Error {
                code: ErrorCode::Provider,
            },
        }
    }

    /// The lease the adapter attaches to an outbound request. Reuses a still-valid cached token so a stream
    /// does not re-exchange on every call.
    fn access_token(&self) -> Result<AdcAccessToken, CredentialError> {
        {
            let cached = self.cached.lock().unwrap();
            if let Some(resolved) = cached.as_ref()
                && !resolved.expires_at_unix_ms.is_some_and(is_expiring)
            {
                return resolved.lease();
            }
        }
        match self.resolve_token() {
            Ok(resolved) => resolved.lease(),
            Err(ResolveFailure::Absent | ResolveFailure::Unrecognized) => {
                Err(CredentialError::Absent)
            }
            Err(ResolveFailure::Rejected) => Err(CredentialError::Rejected),
            Err(_) => Err(CredentialError::Unavailable),
        }
    }

    fn mark_rejected(&self) {
        if let Some(file) = self.file.load() {
            *self.rejection.lock().unwrap() = Some(file.fingerprint);
        }
        *self.cached.lock().unwrap() = None;
    }

    fn resolve_token(&self) -> Result<ResolvedToken, ResolveFailure> {
        let Some(file) = self.file.load() else {
            *self.cached.lock().unwrap() = None;
            return Err(ResolveFailure::Absent);
        };
        if self.rejection.lock().unwrap().as_deref() == Some(file.fingerprint.as_str()) {
            return Err(ResolveFailure::Rejected);
        }
        let request = build_token_request(&file.bytes)?;
        match self.endpoint.exchange(&request) {
            Ok(grant) => {
                let expires_at_unix_ms = grant
                    .expires_in_secs
                    .checked_mul(1000)
                    .map(|ms| unix_millis().saturating_add(ms));
                let resolved = ResolvedToken {
                    token: grant.access_token,
                    expires_at_unix_ms,
                };
                *self.rejection.lock().unwrap() = None;
                *self.cached.lock().unwrap() = Some(resolved.clone());
                Ok(resolved)
            }
            Err(TokenEndpointError::Rejected) => {
                *self.rejection.lock().unwrap() = Some(file.fingerprint);
                *self.cached.lock().unwrap() = None;
                Err(ResolveFailure::Rejected)
            }
            Err(TokenEndpointError::Timeout) => Err(ResolveFailure::Timeout),
            Err(TokenEndpointError::Network) => Err(ResolveFailure::Network),
            Err(TokenEndpointError::Malformed) => Err(ResolveFailure::Provider),
        }
    }
}

/// The seam the adapter actually holds.
pub(crate) struct VertexAdapterCredentials {
    resolver: Arc<VertexAdcResolver>,
}

impl VertexAdapterCredentials {
    pub(crate) const fn new(resolver: Arc<VertexAdcResolver>) -> Self {
        Self { resolver }
    }
}

impl AdcAccessTokenSource for VertexAdapterCredentials {
    fn resolve_access_token(&mut self) -> Result<AdcAccessToken, CredentialError> {
        self.resolver.access_token()
    }

    fn mark_rejected(&mut self) {
        self.resolver.mark_rejected();
    }
}

#[derive(Clone)]
struct ResolvedToken {
    token: Zeroizing<String>,
    expires_at_unix_ms: Option<i64>,
}

impl ResolvedToken {
    fn lease(&self) -> Result<AdcAccessToken, CredentialError> {
        let token =
            AccessToken::new(self.token.to_string()).map_err(|_| CredentialError::Unavailable)?;
        Ok(AdcAccessToken::new(token, self.expires_at_unix_ms))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolveFailure {
    Absent,
    Unrecognized,
    SupportGap,
    Local,
    Rejected,
    Timeout,
    Network,
    Provider,
}

/// One ADC file's bytes plus a secret-free change fingerprint (the file's modification time in production).
struct AdcFile {
    bytes: Zeroizing<Vec<u8>>,
    fingerprint: String,
}

/// Injected ADC file discovery. Production reads the real path; tests supply bytes directly.
trait AdcFileSource: Send + Sync {
    fn load(&self) -> Option<AdcFile>;
}

/// Injected token endpoint. Production POSTs to Google's OAuth endpoint; tests script the grant/rejection.
trait TokenEndpoint: Send + Sync {
    fn exchange(&self, request: &TokenRequest) -> Result<TokenGrant, TokenEndpointError>;
}

struct TokenRequest {
    url: String,
    form: Vec<(&'static str, Zeroizing<String>)>,
}

struct TokenGrant {
    access_token: Zeroizing<String>,
    expires_in_secs: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenEndpointError {
    Rejected,
    Timeout,
    Network,
    Malformed,
}

enum BuildFailure {
    Unrecognized,
    SupportGap,
    Local,
}

impl From<BuildFailure> for ResolveFailure {
    fn from(failure: BuildFailure) -> Self {
        match failure {
            BuildFailure::Unrecognized => Self::Unrecognized,
            BuildFailure::SupportGap => Self::SupportGap,
            BuildFailure::Local => Self::Local,
        }
    }
}

fn build_token_request(bytes: &[u8]) -> Result<TokenRequest, BuildFailure> {
    let document: AdcDocument =
        serde_json::from_slice(bytes).map_err(|_| BuildFailure::Unrecognized)?;
    match document {
        AdcDocument::AuthorizedUser(mut user) => {
            let form = vec![
                ("grant_type", Zeroizing::new("refresh_token".to_owned())),
                (
                    "client_id",
                    Zeroizing::new(std::mem::take(&mut user.client_id)),
                ),
                (
                    "client_secret",
                    Zeroizing::new(std::mem::take(&mut user.client_secret)),
                ),
                (
                    "refresh_token",
                    Zeroizing::new(std::mem::take(&mut user.refresh_token)),
                ),
            ];
            Ok(TokenRequest {
                url: OAUTH_TOKEN_URL.to_owned(),
                form,
            })
        }
        AdcDocument::ServiceAccount(account) => service_account_request(account),
        AdcDocument::ExternalAccount
        | AdcDocument::ExternalAccountAuthorizedUser
        | AdcDocument::ImpersonatedServiceAccount => Err(BuildFailure::SupportGap),
    }
}

fn service_account_request(mut account: ServiceAccount) -> Result<TokenRequest, BuildFailure> {
    let token_uri = account
        .token_uri
        .take()
        .filter(|uri| !uri.is_empty())
        .unwrap_or_else(|| OAUTH_TOKEN_URL.to_owned());
    let assertion = sign_service_account_assertion(&account.client_email, &token_uri, &account);
    account.private_key.zeroize();
    let assertion = assertion.map_err(|()| BuildFailure::Local)?;
    let form = vec![
        ("grant_type", Zeroizing::new(JWT_BEARER_GRANT.to_owned())),
        ("assertion", assertion),
    ];
    Ok(TokenRequest {
        url: token_uri,
        form,
    })
}

fn sign_service_account_assertion(
    client_email: &str,
    audience: &str,
    account: &ServiceAccount,
) -> Result<Zeroizing<String>, ()> {
    let now = unix_secs();
    let header = base64url(br#"{"alg":"RS256","typ":"JWT"}"#);
    let claims = serde_json::json!({
        "iss": client_email,
        "scope": CLOUD_PLATFORM_SCOPE,
        "aud": audience,
        "iat": now,
        "exp": now + SERVICE_ACCOUNT_ASSERTION_TTL_SECS,
    });
    let claims = base64url(serde_json::to_vec(&claims).map_err(|_| ())?.as_slice());
    let signing_input = format!("{header}.{claims}");
    let signature = rsa_pkcs1_sha256(&account.private_key, signing_input.as_bytes())?;
    Ok(Zeroizing::new(format!(
        "{signing_input}.{}",
        base64url(&signature)
    )))
}

fn rsa_pkcs1_sha256(private_key_pem: &str, message: &[u8]) -> Result<Vec<u8>, ()> {
    use ring::rand::SystemRandom;
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};

    let mut der = pem_to_der(private_key_pem).ok_or(())?;
    let key_pair = RsaKeyPair::from_pkcs8(&der).map_err(|_| ());
    der.zeroize();
    let key_pair = key_pair?;
    let mut signature = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            message,
            &mut signature,
        )
        .map_err(|_| ())?;
    Ok(signature)
}

fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    let mut body = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("-----") {
            continue;
        }
        body.push_str(line);
    }
    let der = base64::engine::general_purpose::STANDARD.decode(&body).ok();
    body.zeroize();
    der
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AdcDocument {
    AuthorizedUser(AuthorizedUser),
    ServiceAccount(ServiceAccount),
    ExternalAccount,
    ExternalAccountAuthorizedUser,
    ImpersonatedServiceAccount,
}

#[derive(Deserialize)]
struct AuthorizedUser {
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct ServiceAccount {
    client_email: String,
    private_key: String,
    #[serde(default)]
    token_uri: Option<String>,
}

impl Drop for AuthorizedUser {
    fn drop(&mut self) {
        self.client_secret.zeroize();
        self.refresh_token.zeroize();
    }
}

impl Drop for ServiceAccount {
    fn drop(&mut self) {
        self.private_key.zeroize();
    }
}

fn is_expiring(expires_at_unix_ms: i64) -> bool {
    expires_at_unix_ms.saturating_sub(EXPIRY_SKEW_MS) <= unix_millis()
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or(0)
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// Reads ADC from `GOOGLE_APPLICATION_CREDENTIALS`, else the well-known macOS path. IO/absence both surface
/// as "not armed"; a set-but-missing override is treated the same rather than as a hard error.
struct WellKnownAdcFile;

impl AdcFileSource for WellKnownAdcFile {
    fn load(&self) -> Option<AdcFile> {
        let path = match std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS") {
            Some(path) if !path.is_empty() => std::path::PathBuf::from(path),
            _ => {
                let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
                std::path::Path::new(&home)
                    .join(".config/gcloud/application_default_credentials.json")
            }
        };
        let bytes = Zeroizing::new(std::fs::read(&path).ok()?);
        let fingerprint = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or_else(|| "0".to_owned(), |elapsed| elapsed.as_millis().to_string());
        Some(AdcFile { bytes, fingerprint })
    }
}

struct OauthTokenEndpoint {
    agent: ureq::Agent,
}

impl OauthTokenEndpoint {
    fn new() -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .https_only(true)
                .redirects(0)
                .timeout(Duration::from_secs(30))
                .build(),
        }
    }
}

impl TokenEndpoint for OauthTokenEndpoint {
    fn exchange(&self, request: &TokenRequest) -> Result<TokenGrant, TokenEndpointError> {
        let form = request
            .form
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();
        match self.agent.post(&request.url).send_form(&form) {
            Ok(response) => parse_grant(response),
            Err(ureq::Error::Status(400 | 401, _)) => Err(TokenEndpointError::Rejected),
            Err(ureq::Error::Status(_, _)) => Err(TokenEndpointError::Network),
            Err(ureq::Error::Transport(transport)) => Err(if transport_timed_out(&transport) {
                TokenEndpointError::Timeout
            } else {
                TokenEndpointError::Network
            }),
        }
    }
}

fn parse_grant(response: ureq::Response) -> Result<TokenGrant, TokenEndpointError> {
    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        expires_in: i64,
    }

    // Read into a zeroizing buffer rather than `into_json` so the raw access token never lingers in an
    // un-zeroized String (and to avoid pulling ureq's `json` feature into the build).
    let body = Zeroizing::new(
        response
            .into_string()
            .map_err(|_| TokenEndpointError::Malformed)?,
    );
    let mut parsed: TokenResponse =
        serde_json::from_str(&body).map_err(|_| TokenEndpointError::Malformed)?;
    if parsed.access_token.is_empty() {
        return Err(TokenEndpointError::Malformed);
    }
    let grant = TokenGrant {
        access_token: Zeroizing::new(std::mem::take(&mut parsed.access_token)),
        expires_in_secs: parsed.expires_in,
    };
    Ok(grant)
}

fn transport_timed_out(transport: &ureq::Transport) -> bool {
    use std::error::Error as _;
    use std::io;

    transport
        .source()
        .and_then(|source| source.downcast_ref::<io::Error>())
        .is_some_and(|source| {
            matches!(
                source.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PRIVATE_KEY: &str = include_str!("../test-assets/vertex-service-account-key.pem");

    fn config() -> ConfigSource {
        Arc::new(VertexConnectionConfig::default)
    }

    struct StaticFile {
        json: String,
        fingerprint: String,
    }

    impl AdcFileSource for StaticFile {
        fn load(&self) -> Option<AdcFile> {
            Some(AdcFile {
                bytes: Zeroizing::new(self.json.clone().into_bytes()),
                fingerprint: self.fingerprint.clone(),
            })
        }
    }

    struct MissingFile;

    impl AdcFileSource for MissingFile {
        fn load(&self) -> Option<AdcFile> {
            None
        }
    }

    struct ScriptedEndpoint {
        result: Mutex<Vec<Result<TokenGrant, TokenEndpointError>>>,
        calls: Mutex<usize>,
        last_form: Mutex<Vec<(&'static str, String)>>,
    }

    impl ScriptedEndpoint {
        fn new(results: Vec<Result<TokenGrant, TokenEndpointError>>) -> Self {
            Self {
                result: Mutex::new(results),
                calls: Mutex::new(0),
                last_form: Mutex::new(Vec::new()),
            }
        }

        fn always(result: Result<TokenGrant, TokenEndpointError>) -> Self {
            Self::new(vec![result])
        }
    }

    impl TokenEndpoint for ScriptedEndpoint {
        fn exchange(&self, request: &TokenRequest) -> Result<TokenGrant, TokenEndpointError> {
            *self.calls.lock().unwrap() += 1;
            *self.last_form.lock().unwrap() = request
                .form
                .iter()
                .map(|(name, value)| (*name, value.to_string()))
                .collect();
            let mut results = self.result.lock().unwrap();
            if results.len() > 1 {
                results.remove(0)
            } else {
                results.first().cloned_grant()
            }
        }
    }

    trait CloneGrant {
        fn cloned_grant(&self) -> Result<TokenGrant, TokenEndpointError>;
    }

    impl CloneGrant for Option<&Result<TokenGrant, TokenEndpointError>> {
        fn cloned_grant(&self) -> Result<TokenGrant, TokenEndpointError> {
            match self {
                Some(Ok(grant)) => Ok(TokenGrant {
                    access_token: grant.access_token.clone(),
                    expires_in_secs: grant.expires_in_secs,
                }),
                Some(Err(error)) => Err(*error),
                None => Err(TokenEndpointError::Network),
            }
        }
    }

    fn grant(token: &str, expires_in_secs: i64) -> Result<TokenGrant, TokenEndpointError> {
        Ok(TokenGrant {
            access_token: Zeroizing::new(token.to_owned()),
            expires_in_secs,
        })
    }

    const AUTHORIZED_USER_JSON: &str = r#"{
        "type": "authorized_user",
        "client_id": "synthetic.apps.googleusercontent.com",
        "client_secret": "synthetic-secret",
        "refresh_token": "synthetic-refresh"
    }"#;

    fn service_account_json() -> String {
        serde_json::json!({
            "type": "service_account",
            "client_email": "synthetic@synthetic.iam.gserviceaccount.com",
            "private_key": TEST_PRIVATE_KEY,
            "token_uri": OAUTH_TOKEN_URL,
        })
        .to_string()
    }

    fn resolver(
        file: Box<dyn AdcFileSource>,
        endpoint: Box<dyn TokenEndpoint>,
    ) -> Arc<VertexAdcResolver> {
        VertexAdcResolver::new(config(), file, endpoint)
    }

    #[test]
    fn authorized_user_json_produces_a_refresh_token_grant_form() {
        let request = build_token_request(AUTHORIZED_USER_JSON.as_bytes())
            .ok()
            .unwrap();
        assert_eq!(request.url, OAUTH_TOKEN_URL);
        let form: Vec<(&str, &str)> = request
            .form
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        assert!(form.contains(&("grant_type", "refresh_token")));
        assert!(form.contains(&("refresh_token", "synthetic-refresh")));
        assert!(form.contains(&("client_secret", "synthetic-secret")));
    }

    #[test]
    fn service_account_json_produces_a_signed_jwt_bearer_assertion() {
        let request = build_token_request(service_account_json().as_bytes())
            .ok()
            .unwrap();
        assert_eq!(request.url, OAUTH_TOKEN_URL);
        let grant_type = request
            .form
            .iter()
            .find(|(name, _)| *name == "grant_type")
            .map(|(_, value)| value.to_string());
        assert_eq!(grant_type.as_deref(), Some(JWT_BEARER_GRANT));
        let assertion = request
            .form
            .iter()
            .find(|(name, _)| *name == "assertion")
            .map(|(_, value)| value.to_string())
            .unwrap();
        assert_eq!(assertion.split('.').count(), 3, "header.claims.signature");
    }

    #[test]
    fn malformed_and_unknown_documents_are_unrecognized() {
        assert!(matches!(
            build_token_request(b"not json"),
            Err(BuildFailure::Unrecognized)
        ));
        assert!(matches!(
            build_token_request(br#"{"type":"authorized_user","client_id":"only-id"}"#),
            Err(BuildFailure::Unrecognized)
        ));
        assert!(matches!(
            build_token_request(br#"{"type":"some_future_kind"}"#),
            Err(BuildFailure::Unrecognized)
        ));
    }

    #[test]
    fn workload_identity_documents_are_a_support_gap() {
        assert!(matches!(
            build_token_request(br#"{"type":"external_account","audience":"x"}"#),
            Err(BuildFailure::SupportGap)
        ));
    }

    #[test]
    fn an_absent_file_is_unarmed() {
        let resolver = resolver(
            Box::new(MissingFile),
            Box::new(ScriptedEndpoint::always(grant("t", 3600))),
        );
        assert_eq!(resolver.resolve_outcome(), VertexResolutionOutcome::Unarmed);
    }

    #[test]
    fn a_successful_exchange_is_ready_with_an_expiry() {
        let before = unix_millis();
        let resolver = resolver(
            Box::new(StaticFile {
                json: AUTHORIZED_USER_JSON.to_owned(),
                fingerprint: "1".to_owned(),
            }),
            Box::new(ScriptedEndpoint::always(grant("synthetic-access", 3600))),
        );
        match resolver.resolve_outcome() {
            VertexResolutionOutcome::Ready {
                expires_at_unix_ms: Some(expires_at),
            } => assert!(expires_at >= before + 3600 * 1000),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn a_token_endpoint_rejection_is_rejected() {
        let resolver = resolver(
            Box::new(StaticFile {
                json: AUTHORIZED_USER_JSON.to_owned(),
                fingerprint: "1".to_owned(),
            }),
            Box::new(ScriptedEndpoint::always(Err(TokenEndpointError::Rejected))),
        );
        assert_eq!(
            resolver.resolve_outcome(),
            VertexResolutionOutcome::Rejected
        );
    }

    #[test]
    fn transport_and_support_failures_map_to_distinct_error_codes() {
        let network = resolver(
            Box::new(StaticFile {
                json: AUTHORIZED_USER_JSON.to_owned(),
                fingerprint: "1".to_owned(),
            }),
            Box::new(ScriptedEndpoint::always(Err(TokenEndpointError::Network))),
        );
        assert_eq!(
            network.resolve_outcome(),
            VertexResolutionOutcome::Error {
                code: ErrorCode::Network
            }
        );

        let support_gap = resolver(
            Box::new(StaticFile {
                json: r#"{"type":"external_account"}"#.to_owned(),
                fingerprint: "1".to_owned(),
            }),
            Box::new(ScriptedEndpoint::always(grant("t", 3600))),
        );
        assert_eq!(
            support_gap.resolve_outcome(),
            VertexResolutionOutcome::Error {
                code: ErrorCode::AuthUnarmed
            }
        );
    }

    #[test]
    fn a_rejection_sticks_to_its_file_and_clears_when_the_file_changes() {
        let file = Box::new(SwappableFile::new(AUTHORIZED_USER_JSON, "fingerprint-a"));
        let handle = file.clone();
        let endpoint = ScriptedEndpoint::new(vec![
            Err(TokenEndpointError::Rejected),
            grant("synthetic-access", 3600),
        ]);
        let resolver = resolver(file, Box::new(endpoint));

        assert_eq!(
            resolver.resolve_outcome(),
            VertexResolutionOutcome::Rejected
        );
        // Re-resolving the same file must stay Rejected without another exchange.
        assert_eq!(
            resolver.resolve_outcome(),
            VertexResolutionOutcome::Rejected
        );

        handle.set_fingerprint("fingerprint-b");
        assert!(matches!(
            resolver.resolve_outcome(),
            VertexResolutionOutcome::Ready { .. }
        ));
    }

    #[test]
    fn the_adapter_source_leases_a_token_and_marks_rejection_back_to_the_resolver() {
        let resolver = resolver(
            Box::new(StaticFile {
                json: AUTHORIZED_USER_JSON.to_owned(),
                fingerprint: "fingerprint-a".to_owned(),
            }),
            Box::new(ScriptedEndpoint::always(grant("synthetic-access", 3600))),
        );
        let mut credentials = VertexAdapterCredentials::new(resolver.clone());
        assert!(credentials.resolve_access_token().is_ok());

        credentials.mark_rejected();
        assert_eq!(
            resolver.resolve_outcome(),
            VertexResolutionOutcome::Rejected
        );
        assert!(matches!(
            credentials.resolve_access_token(),
            Err(CredentialError::Rejected)
        ));
    }

    #[derive(Clone)]
    struct SwappableFile {
        json: String,
        fingerprint: Arc<Mutex<String>>,
    }

    impl SwappableFile {
        fn new(json: &str, fingerprint: &str) -> Self {
            Self {
                json: json.to_owned(),
                fingerprint: Arc::new(Mutex::new(fingerprint.to_owned())),
            }
        }

        fn set_fingerprint(&self, fingerprint: &str) {
            *self.fingerprint.lock().unwrap() = fingerprint.to_owned();
        }
    }

    impl AdcFileSource for SwappableFile {
        fn load(&self) -> Option<AdcFile> {
            Some(AdcFile {
                bytes: Zeroizing::new(self.json.clone().into_bytes()),
                fingerprint: self.fingerprint.lock().unwrap().clone(),
            })
        }
    }
}
