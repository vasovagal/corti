//! Native ChatGPT subscription authentication and Responses transport.
//!
//! This module implements OpenAI's Codex device authorization flow and fixed ChatGPT Codex HTTP
//! endpoints directly. It never launches or talks to a Codex app-server, and it never imports another
//! application's credentials. The application injects a private credential store (the production app uses
//! one non-synchronizing macOS Keychain item) and receives only secret-free readiness/display state.

use std::{
    collections::HashSet,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use corti_postprocess::{
    AdapterCapabilities, BillingBasis, CancellationToken, ConnectionScopeId, CredentialSourceKind,
    CredentialState, ErrorCode, HostedRequest, KnownTransport, ModelCatalog, ModelDescriptor,
    ModelId, NormalizedUsage, PostprocessError, ProviderAdapter, ProviderCacheMode,
    ProviderDescriptor, ProviderEventKind, ProviderEventSink, ProviderScope, ProviderTerminal,
    RawUsage, SupportTier,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::{
    common::{
        DISCARD_EVENT_SINK, DirectAdapterOptions, SendFailure, TextCollector, Timing,
        boundary_code, emit, http_status_code, json_bytes, parse_output, read_body_limited,
        request_timeout, send_with_retry, terminal_cache_observation, usage_cache_observations,
        validate_event_stream_response, validate_prompt_layout,
    },
    schema::{output_schema, output_schema_name},
    sse::{SseDecoder, SseEvent},
    transport::{
        AccessToken, Clock, HttpMethod, HttpRequest, HttpResponse, HttpTransport, SecretString,
        TransportErrorKind, WallClock,
    },
};

pub const CHATGPT_SUBSCRIPTION_ADAPTER_VERSION: u32 = 1;
pub const CHATGPT_DEVICE_VERIFICATION_URL: &str = "https://auth.openai.com/codex/device";
pub const CHATGPT_CONSERVATIVE_MAX_OUTPUT_TOKENS: u64 = 8 * 1024;
pub const CHATGPT_FALLBACK_CONTEXT_TOKENS: u64 = 128 * 1024;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// Version of the Codex model-catalog protocol shape this adapter was verified against. This is not
/// Corti's release number: the endpoint uses it to hide models that require a newer catalog client.
const CODEX_MODELS_CLIENT_VERSION: &str = "0.147.0";
const DEVICE_CODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
const DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const MODELS_URL: &str = "https://chatgpt.com/backend-api/codex/models";
const JWT_AUTH_CLAIM: &str = "https://api.openai.com/auth";
const AUTH_VERSION: u32 = 1;
const AUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEVICE_LOGIN_TIMEOUT_MICROS: u64 = 15 * 60 * 1_000_000;
const REFRESH_MARGIN_SECONDS: i64 = 60;
const MAX_AUTH_BODY_BYTES: usize = 1024 * 1024;
const MAX_CATALOG_BODY_BYTES: usize = 4 * 1024 * 1024;
const CATALOG_TIMEOUT_MICROS: u64 = 30_000_000;
const MAX_ACCOUNT_ID_BYTES: usize = 4 * 1024;

/// Combined clock contract used by OAuth expiry and bounded device polling.
pub trait ChatGptClock: Clock + WallClock {}
impl<T: Clock + WallClock + ?Sized> ChatGptClock for T {}

/// Content-free failures from the application-owned credential store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ChatGptStoreError {
    #[error("ChatGPT credential store is unavailable")]
    Unavailable,
}

/// The only persistence seam for a Corti-owned ChatGPT credential document.
///
/// Implementations must not log, display, or forward the bytes. Production stores the complete rotating
/// credential document in one non-synchronizing Keychain item.
pub trait ChatGptCredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Vec<u8>>, ChatGptStoreError>;
    fn save(&self, document: &[u8]) -> Result<(), ChatGptStoreError>;
    fn clear(&self) -> Result<(), ChatGptStoreError>;
}

/// Sanitized device-auth/refresh failure. No variant can carry an OAuth body, token, account id, or URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ChatGptAuthError {
    #[error("ChatGPT authorization is already in progress")]
    Busy,
    #[error("ChatGPT authorization is not in progress")]
    InvalidState,
    #[error("ChatGPT credential is absent")]
    Absent,
    #[error("ChatGPT rejected the credential")]
    Rejected,
    #[error("ChatGPT authentication network failure")]
    Network,
    #[error("ChatGPT authentication timed out")]
    Timeout,
    #[error("ChatGPT authentication protocol failure")]
    Protocol,
    #[error("ChatGPT credential store failure")]
    Store,
    #[error("ChatGPT authentication provider failure")]
    Provider,
}

impl ChatGptAuthError {
    pub const fn error_code(self) -> ErrorCode {
        match self {
            Self::Absent => ErrorCode::AuthUnarmed,
            Self::Rejected => ErrorCode::AuthRejected,
            Self::Network => ErrorCode::Network,
            Self::Timeout => ErrorCode::Timeout,
            Self::Busy | Self::InvalidState | Self::Protocol | Self::Provider => {
                ErrorCode::Provider
            }
            Self::Store => ErrorCode::AuthUnarmed,
        }
    }
}

/// Display-only values returned to the Preferences window.
#[derive(Clone, PartialEq, Eq)]
pub struct ChatGptDeviceAuthorization {
    verification_url: String,
    user_code: String,
    login_id: String,
}

impl ChatGptDeviceAuthorization {
    pub fn verification_url(&self) -> &str {
        &self.verification_url
    }

    pub fn user_code(&self) -> &str {
        &self.user_code
    }

    pub fn login_id(&self) -> &str {
        &self.login_id
    }
}

impl fmt::Debug for ChatGptDeviceAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptDeviceAuthorization")
            .field("verification_url", &self.verification_url)
            .field("user_code_bytes", &self.user_code.len())
            .field("login_id", &self.login_id)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatGptLoginPoll {
    Pending,
    Authorized { durable: bool },
    Denied,
    Expired,
}

#[derive(Clone)]
pub struct ChatGptSubscriptionAuth {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    transport: Mutex<Box<dyn HttpTransport>>,
    clock: Arc<dyn ChatGptClock>,
    store: Arc<dyn ChatGptCredentialStore>,
    state: Mutex<AuthState>,
    operation: Mutex<()>,
    next_login: AtomicU64,
}

impl fmt::Debug for ChatGptSubscriptionAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptSubscriptionAuth")
            .field("state", &self.credential_state())
            .field("transport", &"<injected>")
            .field("store", &"<injected>")
            .finish()
    }
}

#[derive(Default)]
struct AuthState {
    credentials: Option<StoredCredentials>,
    credentials_durable: bool,
    pending: Option<PendingLogin>,
    status: AuthStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AuthStatus {
    #[default]
    Normal,
    Resolving,
    Refreshing,
    Rejected,
    Error(ErrorCode),
}

struct PendingLogin {
    device_auth_id: String,
    user_code: String,
    login_id: String,
    interval: Duration,
    expires_at_micros: u64,
}

impl Drop for PendingLogin {
    fn drop(&mut self) {
        self.device_auth_id.zeroize();
    }
}

impl fmt::Debug for PendingLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingLogin")
            .field("device_auth_id", &"<redacted>")
            .field("user_code_bytes", &self.user_code.len())
            .field("login_id", &self.login_id)
            .field("interval", &self.interval)
            .field("expires_at_micros", &self.expires_at_micros)
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredCredentials {
    version: u32,
    access: String,
    refresh: String,
    expires_at: i64,
    account_id: String,
}

impl StoredCredentials {
    fn validate(&self) -> Result<(), ChatGptAuthError> {
        if self.version != AUTH_VERSION
            || self.access.is_empty()
            || self.refresh.is_empty()
            || self.account_id.is_empty()
            || self.account_id.len() > MAX_ACCOUNT_ID_BYTES
            || !self.access.bytes().all(|byte| byte.is_ascii_graphic())
            || !self.refresh.bytes().all(|byte| byte.is_ascii_graphic())
            || !self.account_id.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err(ChatGptAuthError::Protocol);
        }
        Ok(())
    }

    fn request_credential(&self) -> Result<ChatGptRequestCredential, ChatGptAuthError> {
        Ok(ChatGptRequestCredential {
            access: AccessToken::new(self.access.clone())
                .map_err(|_| ChatGptAuthError::Protocol)?,
            account_id: SecretString::new(self.account_id.clone()),
        })
    }
}

impl Drop for StoredCredentials {
    fn drop(&mut self) {
        self.access.zeroize();
        self.refresh.zeroize();
        self.account_id.zeroize();
    }
}

impl fmt::Debug for StoredCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCredentials")
            .field("version", &self.version)
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("account_id", &"<redacted>")
            .finish()
    }
}

struct ChatGptRequestCredential {
    access: AccessToken,
    account_id: SecretString,
}

impl fmt::Debug for ChatGptRequestCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChatGptRequestCredential(<redacted>)")
    }
}

impl ChatGptSubscriptionAuth {
    pub fn new(
        transport: Box<dyn HttpTransport>,
        clock: Arc<dyn ChatGptClock>,
        store: Arc<dyn ChatGptCredentialStore>,
    ) -> Self {
        let (credentials, status) = match load_credentials(store.as_ref()) {
            Ok(credentials) => (credentials, AuthStatus::Normal),
            Err(error) => (None, AuthStatus::Error(error.error_code())),
        };
        Self {
            inner: Arc::new(AuthInner {
                transport: Mutex::new(transport),
                clock,
                store,
                state: Mutex::new(AuthState {
                    credentials_durable: credentials.is_some(),
                    credentials,
                    pending: None,
                    status,
                }),
                operation: Mutex::new(()),
                next_login: AtomicU64::new(1),
            }),
        }
    }

    pub fn credential_state(&self) -> CredentialState {
        let state = self.inner.state.lock().unwrap();
        if let Some(pending) = state.pending.as_ref() {
            return CredentialState::DeviceAuthorization {
                verification_url: CHATGPT_DEVICE_VERIFICATION_URL.to_owned(),
                user_code: pending.user_code.clone(),
                login_id: pending.login_id.clone(),
            };
        }
        match state.status {
            AuthStatus::Resolving => CredentialState::Resolving,
            AuthStatus::Refreshing => CredentialState::Refreshing,
            AuthStatus::Rejected => CredentialState::Rejected,
            AuthStatus::Error(code) => CredentialState::Error { code },
            AuthStatus::Normal => match state.credentials.as_ref() {
                Some(_) if !state.credentials_durable => CredentialState::Error {
                    code: ErrorCode::Cache,
                },
                Some(credentials) => CredentialState::Ready {
                    expires_at_unix_ms: Some(credentials.expires_at.saturating_mul(1000)),
                    source: CredentialSourceKind::ChatGptDevice,
                },
                None => CredentialState::Absent,
            },
        }
    }

    pub fn start_device_login(&self) -> Result<ChatGptDeviceAuthorization, ChatGptAuthError> {
        let _operation = self.inner.operation.lock().unwrap();
        {
            let mut state = self.inner.state.lock().unwrap();
            if state.pending.is_some() {
                return Err(ChatGptAuthError::Busy);
            }
            state.status = AuthStatus::Resolving;
        }

        let result = self.start_device_login_inner();
        match result {
            Ok((device_auth_id, user_code, interval)) => {
                let login_number = self.inner.next_login.fetch_add(1, Ordering::Relaxed);
                let login_id = format!("corti-chatgpt-{login_number}");
                let expires_at_micros = self
                    .inner
                    .clock
                    .monotonic_micros()
                    .saturating_add(DEVICE_LOGIN_TIMEOUT_MICROS);
                let authorization = ChatGptDeviceAuthorization {
                    verification_url: CHATGPT_DEVICE_VERIFICATION_URL.to_owned(),
                    user_code: user_code.clone(),
                    login_id: login_id.clone(),
                };
                let mut state = self.inner.state.lock().unwrap();
                state.pending = Some(PendingLogin {
                    device_auth_id,
                    user_code,
                    login_id,
                    interval,
                    expires_at_micros,
                });
                state.status = AuthStatus::Normal;
                Ok(authorization)
            }
            Err(error) => {
                self.record_login_error(error);
                Err(error)
            }
        }
    }

    fn start_device_login_inner(&self) -> Result<(String, String, Duration), ChatGptAuthError> {
        let body = json_bytes_auth(&json!({"client_id": CLIENT_ID}))?;
        let response = self.send(
            HttpRequest::new(HttpMethod::Post, parse_fixed_url(DEVICE_CODE_URL)?)
                .map_err(|_| ChatGptAuthError::Protocol)?
                .with_public_header("accept", "application/json")
                .map_err(|_| ChatGptAuthError::Protocol)?
                .with_public_header("content-type", "application/json")
                .map_err(|_| ChatGptAuthError::Protocol)?
                .with_json_body(body)
                .with_timeout(AUTH_REQUEST_TIMEOUT),
        )?;
        let status = response.status();
        let mut bytes = read_auth_body(response)?;
        if !(200..300).contains(&status) {
            bytes.zeroize();
            return Err(auth_status_error(status));
        }
        let parsed = serde_json::from_slice::<DeviceCodeResponse>(&bytes)
            .map_err(|_| ChatGptAuthError::Protocol);
        bytes.zeroize();
        let parsed = parsed?;
        if parsed.device_auth_id.trim().is_empty()
            || parsed.user_code.trim().is_empty()
            || parsed.user_code.len() > 256
            || parsed.user_code.chars().any(char::is_control)
        {
            return Err(ChatGptAuthError::Protocol);
        }
        let interval = parse_poll_interval(&parsed.interval)?;
        Ok((parsed.device_auth_id, parsed.user_code, interval))
    }

    pub fn poll_interval(&self, login_id: &str) -> Result<Duration, ChatGptAuthError> {
        self.inner
            .state
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .filter(|pending| pending.login_id == login_id)
            .map(|pending| pending.interval)
            .ok_or(ChatGptAuthError::InvalidState)
    }

    pub fn current_login_id(&self) -> Result<String, ChatGptAuthError> {
        self.inner
            .state
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .map(|pending| pending.login_id.clone())
            .ok_or(ChatGptAuthError::InvalidState)
    }

    pub fn poll_device_login(&self, login_id: &str) -> Result<ChatGptLoginPoll, ChatGptAuthError> {
        let _operation = self.inner.operation.lock().unwrap();
        let (device_auth_id, user_code, expires_at_micros) = {
            let state = self.inner.state.lock().unwrap();
            let pending = state
                .pending
                .as_ref()
                .filter(|pending| pending.login_id == login_id)
                .ok_or(ChatGptAuthError::InvalidState)?;
            (
                Zeroizing::new(pending.device_auth_id.clone()),
                pending.user_code.clone(),
                pending.expires_at_micros,
            )
        };
        if self.inner.clock.monotonic_micros() >= expires_at_micros {
            self.finish_login_without_credential(AuthStatus::Normal);
            return Ok(ChatGptLoginPoll::Expired);
        }

        let body = json_bytes_auth(&json!({
            "device_auth_id": device_auth_id.as_str(),
            "user_code": user_code,
        }))?;
        let response = match self.send(
            HttpRequest::new(HttpMethod::Post, parse_fixed_url(DEVICE_TOKEN_URL)?)
                .map_err(|_| ChatGptAuthError::Protocol)?
                .with_public_header("accept", "application/json")
                .map_err(|_| ChatGptAuthError::Protocol)?
                .with_public_header("content-type", "application/json")
                .map_err(|_| ChatGptAuthError::Protocol)?
                .with_json_body(body)
                .with_timeout(AUTH_REQUEST_TIMEOUT),
        ) {
            Ok(response) => response,
            Err(error @ (ChatGptAuthError::Network | ChatGptAuthError::Timeout)) => {
                self.backoff_pending();
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let status = response.status();
        let mut bytes = read_auth_body(response)?;
        if (200..300).contains(&status) {
            let parsed = serde_json::from_slice::<DeviceAuthorizationResponse>(&bytes)
                .map_err(|_| ChatGptAuthError::Protocol);
            bytes.zeroize();
            let mut parsed = parsed?;
            if parsed.authorization_code.is_empty() || parsed.code_verifier.is_empty() {
                return Err(ChatGptAuthError::Protocol);
            }
            let code = Zeroizing::new(std::mem::take(&mut parsed.authorization_code));
            let verifier = Zeroizing::new(std::mem::take(&mut parsed.code_verifier));
            let credentials = self.exchange_authorization(&code, &verifier)?;
            let durable = self.install_credentials(credentials);
            return Ok(ChatGptLoginPoll::Authorized { durable });
        }

        let error_code = oauth_error_code(&bytes);
        bytes.zeroize();
        if matches!(
            error_code.as_deref(),
            Some("expired_token" | "deviceauth_authorization_expired")
        ) {
            self.finish_login_without_credential(AuthStatus::Normal);
            return Ok(ChatGptLoginPoll::Expired);
        }
        if matches!(
            error_code.as_deref(),
            Some("access_denied" | "authorization_declined")
        ) {
            self.finish_login_without_credential(AuthStatus::Rejected);
            return Ok(ChatGptLoginPoll::Denied);
        }
        if status == 403
            || status == 404
            || error_code.as_deref() == Some("deviceauth_authorization_pending")
        {
            return Ok(ChatGptLoginPoll::Pending);
        }
        if status == 429 || error_code.as_deref() == Some("slow_down") {
            self.backoff_pending();
            return Ok(ChatGptLoginPoll::Pending);
        }
        let error = auth_status_error(status);
        if error == ChatGptAuthError::Provider {
            self.backoff_pending();
        } else if !matches!(error, ChatGptAuthError::Network | ChatGptAuthError::Timeout) {
            self.record_login_error(error);
        }
        Err(error)
    }

    pub fn cancel_device_login(&self, login_id: &str) -> Result<(), ChatGptAuthError> {
        let _operation = self.inner.operation.lock().unwrap();
        let mut state = self.inner.state.lock().unwrap();
        if state
            .pending
            .as_ref()
            .is_none_or(|pending| pending.login_id != login_id)
        {
            return Err(ChatGptAuthError::InvalidState);
        }
        state.pending = None;
        state.status = AuthStatus::Normal;
        Ok(())
    }

    pub fn sign_out(&self) -> Result<(), ChatGptAuthError> {
        let _operation = self.inner.operation.lock().unwrap();
        self.inner
            .store
            .clear()
            .map_err(|_| ChatGptAuthError::Store)?;
        *self.inner.state.lock().unwrap() = AuthState::default();
        Ok(())
    }

    /// Opaque, account-specific scope identity for cache/request fencing. The account id itself never leaves
    /// this module; switching ChatGPT accounts necessarily yields a different local scope.
    pub fn connection_scope_id(&self) -> Result<ConnectionScopeId, ChatGptAuthError> {
        let state = self.inner.state.lock().unwrap();
        let credentials = state.credentials.as_ref().ok_or(ChatGptAuthError::Absent)?;
        let mut digest = Sha256::new();
        digest.update(b"corti-chatgpt-scope-v1\0");
        digest.update(credentials.account_id.as_bytes());
        let value = URL_SAFE_NO_PAD.encode(digest.finalize());
        ConnectionScopeId::new(format!("chatgpt-v1-{value}"))
            .map_err(|_| ChatGptAuthError::Protocol)
    }

    fn request_credential(
        &self,
        force_refresh: bool,
    ) -> Result<ChatGptRequestCredential, ChatGptAuthError> {
        let _operation = self.inner.operation.lock().unwrap();
        let needs_refresh = {
            let state = self.inner.state.lock().unwrap();
            if state.pending.is_some() || state.status == AuthStatus::Resolving {
                return Err(ChatGptAuthError::Busy);
            }
            if state.status == AuthStatus::Rejected {
                return Err(ChatGptAuthError::Rejected);
            }
            let credentials = state.credentials.as_ref().ok_or(ChatGptAuthError::Absent)?;
            force_refresh
                || self.inner.clock.unix_seconds()
                    >= credentials
                        .expires_at
                        .saturating_sub(REFRESH_MARGIN_SECONDS)
        };
        if needs_refresh {
            let (refresh, expected_account) = {
                let mut state = self.inner.state.lock().unwrap();
                state.status = AuthStatus::Refreshing;
                let credentials = state.credentials.as_ref().ok_or(ChatGptAuthError::Absent)?;
                (
                    Zeroizing::new(credentials.refresh.clone()),
                    Zeroizing::new(credentials.account_id.clone()),
                )
            };
            match self.refresh_credentials(&refresh) {
                Ok(credentials) if credentials.account_id == expected_account.as_str() => {
                    self.install_credentials(credentials);
                }
                Ok(_) => {
                    self.record_auth_error(ChatGptAuthError::Protocol);
                    return Err(ChatGptAuthError::Protocol);
                }
                Err(error) => {
                    self.record_auth_error(error);
                    return Err(error);
                }
            }
        }
        let mut state = self.inner.state.lock().unwrap();
        state.status = AuthStatus::Normal;
        state
            .credentials
            .as_ref()
            .ok_or(ChatGptAuthError::Absent)?
            .request_credential()
    }

    fn exchange_authorization(
        &self,
        code: &str,
        verifier: &str,
    ) -> Result<StoredCredentials, ChatGptAuthError> {
        self.request_token(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", DEVICE_REDIRECT_URI),
        ])
    }

    fn refresh_credentials(&self, refresh: &str) -> Result<StoredCredentials, ChatGptAuthError> {
        self.request_token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CLIENT_ID),
        ])
    }

    fn request_token(&self, form: &[(&str, &str)]) -> Result<StoredCredentials, ChatGptAuthError> {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        serializer.extend_pairs(form.iter().copied());
        let mut body = serializer.finish().into_bytes();
        let response = self.send(
            HttpRequest::new(HttpMethod::Post, parse_fixed_url(TOKEN_URL)?)
                .map_err(|_| ChatGptAuthError::Protocol)?
                .with_public_header("accept", "application/json")
                .map_err(|_| ChatGptAuthError::Protocol)?
                .with_public_header("content-type", "application/x-www-form-urlencoded")
                .map_err(|_| ChatGptAuthError::Protocol)?
                .with_body(std::mem::take(&mut body))
                .with_timeout(AUTH_REQUEST_TIMEOUT),
        );
        body.zeroize();
        let response = response?;
        let status = response.status();
        let mut bytes = read_auth_body(response)?;
        if !(200..300).contains(&status) {
            let code = oauth_error_code(&bytes);
            bytes.zeroize();
            return Err(if matches!(code.as_deref(), Some("invalid_grant")) {
                ChatGptAuthError::Rejected
            } else {
                auth_status_error(status)
            });
        }
        let parsed =
            serde_json::from_slice::<TokenResponse>(&bytes).map_err(|_| ChatGptAuthError::Protocol);
        bytes.zeroize();
        let mut parsed = parsed?;
        if parsed.access_token.is_empty()
            || parsed.refresh_token.is_empty()
            || parsed.expires_in == 0
        {
            return Err(ChatGptAuthError::Protocol);
        }
        let access = std::mem::take(&mut parsed.access_token);
        let refresh = std::mem::take(&mut parsed.refresh_token);
        let account_id = extract_account_id(&access)?;
        let expires_in = i64::try_from(parsed.expires_in).unwrap_or(i64::MAX);
        let credentials = StoredCredentials {
            version: AUTH_VERSION,
            access,
            refresh,
            expires_at: self.inner.clock.unix_seconds().saturating_add(expires_in),
            account_id,
        };
        credentials.validate()?;
        Ok(credentials)
    }

    fn install_credentials(&self, credentials: StoredCredentials) -> bool {
        let persisted = persist_credentials(self.inner.store.as_ref(), &credentials);
        let durable = persisted.is_ok();
        {
            let mut state = self.inner.state.lock().unwrap();
            state.credentials = Some(credentials);
            state.credentials_durable = durable;
            state.pending = None;
            state.status = AuthStatus::Normal;
        }
        if let Err(error) = persisted {
            // The provider has already rotated the refresh token. Keeping the new in-memory credential is
            // the only way this process can continue; returning to the invalid predecessor would be worse.
            tracing::error!(
                event = "chatgpt_credential_save_failed",
                error = %error,
                "ChatGPT credential rotated but could not be persisted; continuing with the in-memory token"
            );
        }
        durable
    }

    fn backoff_pending(&self) {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(pending) = state.pending.as_mut() {
            pending.interval = pending
                .interval
                .saturating_add(Duration::from_secs(5))
                .min(Duration::from_secs(30));
        }
    }

    fn finish_login_without_credential(&self, status: AuthStatus) {
        let mut state = self.inner.state.lock().unwrap();
        state.pending = None;
        state.status = if state.credentials.is_some() {
            AuthStatus::Normal
        } else {
            status
        };
    }

    fn record_login_error(&self, error: ChatGptAuthError) {
        let mut state = self.inner.state.lock().unwrap();
        state.pending = None;
        state.status = if state.credentials.is_some() {
            AuthStatus::Normal
        } else if error == ChatGptAuthError::Rejected {
            AuthStatus::Rejected
        } else {
            AuthStatus::Error(error.error_code())
        };
    }

    fn record_auth_error(&self, error: ChatGptAuthError) {
        let mut state = self.inner.state.lock().unwrap();
        state.pending = None;
        state.status = if error == ChatGptAuthError::Rejected {
            AuthStatus::Rejected
        } else {
            AuthStatus::Error(error.error_code())
        };
    }

    fn mark_rejected(&self) {
        self.inner.state.lock().unwrap().status = AuthStatus::Rejected;
    }

    fn send(&self, request: HttpRequest) -> Result<HttpResponse, ChatGptAuthError> {
        let cancel = CancellationToken::new();
        self.inner
            .transport
            .lock()
            .unwrap()
            .send(&request, &cancel)
            .map_err(|error| match error.kind {
                TransportErrorKind::Timeout => ChatGptAuthError::Timeout,
                TransportErrorKind::Network | TransportErrorKind::Canceled => {
                    ChatGptAuthError::Network
                }
                TransportErrorKind::Protocol => ChatGptAuthError::Protocol,
            })
    }
}

fn load_credentials(
    store: &dyn ChatGptCredentialStore,
) -> Result<Option<StoredCredentials>, ChatGptAuthError> {
    let Some(mut document) = store.load().map_err(|_| ChatGptAuthError::Store)? else {
        return Ok(None);
    };
    let parsed = serde_json::from_slice::<StoredCredentials>(&document)
        .map_err(|_| ChatGptAuthError::Protocol);
    document.zeroize();
    let credentials = parsed?;
    credentials.validate()?;
    Ok(Some(credentials))
}

fn persist_credentials(
    store: &dyn ChatGptCredentialStore,
    credentials: &StoredCredentials,
) -> Result<(), ChatGptAuthError> {
    let mut document = serde_json::to_vec(credentials).map_err(|_| ChatGptAuthError::Protocol)?;
    let saved = store.save(&document).map_err(|_| ChatGptAuthError::Store);
    document.zeroize();
    saved
}

fn parse_poll_interval(value: &Value) -> Result<Duration, ChatGptAuthError> {
    let seconds = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(string) => string.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
    .ok_or(ChatGptAuthError::Protocol)?;
    Ok(Duration::from_secs_f64(seconds.clamp(1.0, 30.0)))
}

fn auth_status_error(status: u16) -> ChatGptAuthError {
    match status {
        401 | 403 => ChatGptAuthError::Rejected,
        408 | 504 => ChatGptAuthError::Timeout,
        429 | 500..=599 => ChatGptAuthError::Provider,
        _ => ChatGptAuthError::Protocol,
    }
}

fn read_auth_body(mut response: HttpResponse) -> Result<Vec<u8>, ChatGptAuthError> {
    let mut bytes = Vec::new();
    loop {
        let Some(chunk) = response
            .body_mut()
            .next_chunk()
            .map_err(|error| match error.kind {
                TransportErrorKind::Timeout => ChatGptAuthError::Timeout,
                TransportErrorKind::Network | TransportErrorKind::Canceled => {
                    ChatGptAuthError::Network
                }
                TransportErrorKind::Protocol => ChatGptAuthError::Protocol,
            })?
        else {
            return Ok(bytes);
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_AUTH_BODY_BYTES {
            bytes.zeroize();
            return Err(ChatGptAuthError::Protocol);
        }
        bytes.extend_from_slice(&chunk);
    }
}

fn json_bytes_auth(value: &Value) -> Result<Vec<u8>, ChatGptAuthError> {
    serde_json::to_vec(value).map_err(|_| ChatGptAuthError::Protocol)
}

fn oauth_error_code(body: &[u8]) -> Option<String> {
    match serde_json::from_slice::<Value>(body).ok()?.get("error")? {
        Value::String(code) => Some(code.clone()),
        Value::Object(object) => object
            .get("code")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn extract_account_id(access: &str) -> Result<String, ChatGptAuthError> {
    let payload = access.split('.').nth(1).ok_or(ChatGptAuthError::Protocol)?;
    let mut bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| ChatGptAuthError::Protocol)?;
    let parsed = serde_json::from_slice::<Value>(&bytes).map_err(|_| ChatGptAuthError::Protocol);
    bytes.zeroize();
    parsed?
        .get(JWT_AUTH_CLAIM)
        .and_then(|claim| claim.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|account| !account.is_empty() && account.len() <= MAX_ACCOUNT_ID_BYTES)
        .map(ToOwned::to_owned)
        .ok_or(ChatGptAuthError::Protocol)
}

fn parse_fixed_url(value: &str) -> Result<Url, ChatGptAuthError> {
    let url = Url::parse(value).map_err(|_| ChatGptAuthError::Protocol)?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err(ChatGptAuthError::Protocol);
    }
    Ok(url)
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    interval: Value,
}

#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    authorization_code: String,
    code_verifier: String,
}

impl Drop for DeviceAuthorizationResponse {
    fn drop(&mut self) {
        self.authorization_code.zeroize();
        self.code_verifier.zeroize();
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

impl Drop for TokenResponse {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

pub struct ChatGptSubscriptionAdapter {
    transport: Box<dyn HttpTransport>,
    clock: Box<dyn Clock>,
    auth: ChatGptSubscriptionAuth,
    options: DirectAdapterOptions,
    catalog: ModelCatalog,
}

impl ChatGptSubscriptionAdapter {
    pub fn new(
        transport: Box<dyn HttpTransport>,
        clock: Box<dyn Clock>,
        auth: ChatGptSubscriptionAuth,
    ) -> Self {
        Self {
            transport,
            clock,
            auth,
            options: DirectAdapterOptions::default(),
            catalog: ModelCatalog { models: Vec::new() },
        }
    }

    pub fn with_options(mut self, options: DirectAdapterOptions) -> Result<Self, PostprocessError> {
        self.options = options.validate()?;
        Ok(self)
    }

    fn catalog_inner(&mut self, scope: &ProviderScope) -> Result<ModelCatalog, PostprocessError> {
        if scope.region.is_some() {
            return Err(ErrorCode::PolicyBlocked.into());
        }
        let now = self.clock.monotonic_micros();
        let deadline =
            corti_postprocess::MonotonicDeadline(now.saturating_add(CATALOG_TIMEOUT_MICROS));
        let cancel = CancellationToken::new();
        let credential = self
            .auth
            .request_credential(false)
            .map_err(|error| PostprocessError::from(error.error_code()))?;
        let mut exchange = self.send_catalog(credential, &cancel, deadline)?;
        if exchange.response.status() == 401 {
            let credential = self
                .auth
                .request_credential(true)
                .map_err(|error| PostprocessError::from(error.error_code()))?;
            exchange = self.send_catalog(credential, &cancel, deadline)?;
        }
        if exchange.response.status() != 200 {
            let code = http_status_code(exchange.response.status());
            if code == ErrorCode::AuthRejected {
                self.auth.mark_rejected();
            }
            return Err(code.into());
        }
        let bytes = read_body_limited(
            &mut exchange.response,
            MAX_CATALOG_BODY_BYTES,
            &cancel,
            deadline,
            &*self.clock,
        )?;
        let listing: ChatGptModelList =
            serde_json::from_slice(&bytes).map_err(|_| ErrorCode::MalformedOutput)?;
        let descriptor = KnownTransport::ChatGptSubscription.descriptor();
        let mut seen = HashSet::new();
        let mut models = Vec::new();
        for model in listing.models {
            if !seen.insert(model.slug.clone()) {
                return Err(ErrorCode::MalformedOutput.into());
            }
            if !model.supported_in_api
                || matches!(model.visibility.as_deref(), Some("hide" | "none"))
            {
                continue;
            }
            let context = model
                .context_window
                .or(model.max_context_window)
                .and_then(|value| u64::try_from(value).ok())
                .filter(|value| *value > CHATGPT_CONSERVATIVE_MAX_OUTPUT_TOKENS)
                .unwrap_or(CHATGPT_FALLBACK_CONTEXT_TOKENS);
            models.push(ModelDescriptor {
                provider: descriptor.provider.clone(),
                transport: descriptor.transport.clone(),
                support_tier: SupportTier::Experimental,
                exact_model_id: ModelId::new(model.slug).map_err(|_| ErrorCode::MalformedOutput)?,
                account_scoped_available: true,
                region: None,
                max_context_tokens: context,
                max_output_tokens: CHATGPT_CONSERVATIVE_MAX_OUTPUT_TOKENS,
                capabilities: AdapterCapabilities {
                    text_input: true,
                    text_output: true,
                    streaming: true,
                    structured_output: true,
                    explicit_prefix_cache: false,
                    implicit_cache_may_apply: false,
                },
                billing_basis: BillingBasis::IncludedSubscription,
                tariff_version: None,
                deprecated: false,
                benchmarked_for_live: false,
            });
        }
        let catalog = ModelCatalog { models };
        self.catalog = catalog.clone();
        Ok(catalog)
    }

    fn send_catalog(
        &mut self,
        credential: ChatGptRequestCredential,
        cancel: &CancellationToken,
        deadline: corti_postprocess::MonotonicDeadline,
    ) -> Result<crate::common::Exchange, PostprocessError> {
        let mut url = parse_fixed_url(MODELS_URL)
            .map_err(|error| PostprocessError::from(error.error_code()))?;
        url.query_pairs_mut()
            .append_pair("client_version", CODEX_MODELS_CLIENT_VERSION);
        let now = self.clock.monotonic_micros();
        let wire = authenticated_request(
            HttpRequest::new(HttpMethod::Get, url)
                .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
                .with_public_header("accept", "application/json")
                .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
                .with_timeout(request_timeout(deadline, now)?),
            credential,
        )?;
        send_with_retry(
            &mut *self.transport,
            &*self.clock,
            &wire,
            cancel,
            deadline,
            None,
        )
        .map_err(|failure| PostprocessError::from(failure.code))
    }

    fn execute_inner(
        &mut self,
        request: &HostedRequest,
        cancel: &CancellationToken,
        sink: &dyn ProviderEventSink,
    ) -> Result<ProviderTerminal, ExecFailure> {
        let total_start = self.clock.monotonic_micros();
        let mut timing = Timing::new(total_start);
        if let Some(code) = boundary_code(cancel, request.deadline, total_start) {
            return Err(ExecFailure::new(code, false));
        }
        validate_chatgpt_request(request, &self.catalog, self.options.max_output_tokens)
            .map_err(ExecFailure::from_error)?;

        let auth_start = self.clock.monotonic_micros();
        let credential = self
            .auth
            .request_credential(false)
            .map_err(|error| ExecFailure::new(error.error_code(), false))?;
        timing.auth_us = Some(self.clock.monotonic_micros().saturating_sub(auth_start));
        let body = chatgpt_request_body(request, self.options.max_output_tokens)
            .map_err(ExecFailure::from_error)?;
        let wire = build_chatgpt_wire(
            request,
            credential,
            body,
            request_timeout(request.deadline, self.clock.monotonic_micros())
                .map_err(ExecFailure::from_error)?,
        )
        .map_err(ExecFailure::from_error)?;
        let mut exchange = send_with_retry(
            &mut *self.transport,
            &*self.clock,
            &wire,
            cancel,
            request.deadline,
            Some((request, sink)),
        )
        .map_err(ExecFailure::from_send)?;

        if exchange.response.status() == 401 {
            let credential = self
                .auth
                .request_credential(true)
                .map_err(|error| ExecFailure::new(error.error_code(), true))?;
            let body = chatgpt_request_body(request, self.options.max_output_tokens)
                .map_err(ExecFailure::from_error)?;
            let wire = build_chatgpt_wire(
                request,
                credential,
                body,
                request_timeout(request.deadline, self.clock.monotonic_micros())
                    .map_err(ExecFailure::from_error)?,
            )
            .map_err(ExecFailure::from_error)?;
            exchange = send_with_retry(
                &mut *self.transport,
                &*self.clock,
                &wire,
                cancel,
                request.deadline,
                None,
            )
            .map_err(|failure| ExecFailure {
                code: failure.code,
                usage: None,
                dispatched: true,
            })?;
        }
        timing.exchange = Some(exchange.times);
        if exchange.response.status() != 200 {
            let code = http_status_code(exchange.response.status());
            if code == ErrorCode::AuthRejected {
                self.auth.mark_rejected();
            }
            return Err(ExecFailure::new(code, true));
        }
        validate_event_stream_response(&exchange.response)
            .map_err(|error| ExecFailure::new(error.code, true))?;

        let mut decoder = SseDecoder::new(self.options.max_stream_bytes);
        let mut state = ChatGptStreamState::new(self.options.max_stream_bytes);
        loop {
            if let Some(code) =
                boundary_code(cancel, request.deadline, self.clock.monotonic_micros())
            {
                exchange.response.body_mut().cancel();
                return Err(ExecFailure {
                    code,
                    usage: state.terminal_usage,
                    dispatched: true,
                });
            }
            let next = exchange
                .response
                .body_mut()
                .next_chunk()
                .map_err(|error| ExecFailure {
                    code: crate::common::transport_code(error, cancel),
                    usage: state.terminal_usage,
                    dispatched: true,
                })?;
            let Some(chunk) = next else {
                break;
            };
            let events = decoder.push(&chunk).map_err(|error| ExecFailure {
                code: error.code,
                usage: state.terminal_usage,
                dispatched: true,
            })?;
            let mut buffered_boundary = None;
            for event in events {
                if buffered_boundary.is_none() {
                    buffered_boundary =
                        boundary_code(cancel, request.deadline, self.clock.monotonic_micros());
                }
                let event_sink: &dyn ProviderEventSink = if buffered_boundary.is_some() {
                    &DISCARD_EVENT_SINK
                } else {
                    sink
                };
                state
                    .process(event, request, event_sink, &*self.clock)
                    .map_err(ExecFailure::with_dispatched)?;
                if buffered_boundary.is_none() {
                    buffered_boundary =
                        boundary_code(cancel, request.deadline, self.clock.monotonic_micros());
                }
            }
            if let Some(code) = buffered_boundary {
                exchange.response.body_mut().cancel();
                return Err(ExecFailure {
                    code,
                    usage: state.terminal_usage,
                    dispatched: true,
                });
            }
        }
        for event in decoder.finish().map_err(|error| ExecFailure {
            code: error.code,
            usage: state.terminal_usage,
            dispatched: true,
        })? {
            state
                .process(event, request, sink, &*self.clock)
                .map_err(ExecFailure::with_dispatched)?;
        }
        state.finish().map_err(ExecFailure::with_dispatched)?;
        timing.first_text_at = state.text.first_text_at();
        timing.terminal_at = state.terminal_at;

        if let Some(code) = boundary_code(cancel, request.deadline, self.clock.monotonic_micros()) {
            return Err(ExecFailure {
                code,
                usage: state.terminal_usage,
                dispatched: true,
            });
        }
        let parse_start = self.clock.monotonic_micros();
        let output = parse_output(request.prompt.task(), state.text.text()).map_err(|error| {
            ExecFailure {
                code: error.code,
                usage: state.terminal_usage,
                dispatched: true,
            }
        })?;
        let completed_at = self.clock.monotonic_micros();
        timing.parse_us = Some(completed_at.saturating_sub(parse_start));
        let usage = state
            .terminal_usage
            .unwrap_or_else(NormalizedUsage::unknown);
        Ok(ProviderTerminal {
            output,
            usage,
            latency: timing.latency(completed_at),
            cache: terminal_cache_observation(usage),
        })
    }
}

impl fmt::Debug for ChatGptSubscriptionAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatGptSubscriptionAdapter")
            .field(
                "descriptor",
                &KnownTransport::ChatGptSubscription.descriptor(),
            )
            .field("options", &self.options)
            .field("catalog_models", &self.catalog.models.len())
            .field("auth", &"<Corti-owned-device-credential>")
            .finish()
    }
}

impl ProviderAdapter for ChatGptSubscriptionAdapter {
    fn descriptor(&self) -> ProviderDescriptor {
        KnownTransport::ChatGptSubscription.descriptor()
    }

    fn catalog(&mut self, scope: &ProviderScope) -> Result<ModelCatalog, PostprocessError> {
        self.catalog_inner(scope)
    }

    fn execute(
        &mut self,
        request: &HostedRequest,
        cancel: &CancellationToken,
        sink: &dyn ProviderEventSink,
    ) -> Result<ProviderTerminal, PostprocessError> {
        emit(sink, request, ProviderEventKind::Queued);
        match self.execute_inner(request, cancel, sink) {
            Ok(terminal) => {
                for observation in usage_cache_observations(terminal.usage) {
                    emit(sink, request, ProviderEventKind::CacheObserved(observation));
                }
                emit(sink, request, ProviderEventKind::Completed(terminal.usage));
                Ok(terminal)
            }
            Err(failure) => {
                if failure.code == ErrorCode::AuthRejected {
                    self.auth.mark_rejected();
                }
                if let Some(reason) = cancel.reason() {
                    emit(
                        sink,
                        request,
                        ProviderEventKind::Canceled {
                            reason,
                            terminal_usage: failure.usage,
                            provider_billing_may_still_occur: failure.dispatched,
                        },
                    );
                } else {
                    emit(
                        sink,
                        request,
                        ProviderEventKind::Failed {
                            code: failure.code,
                            terminal_usage: failure.usage,
                        },
                    );
                }
                Err(failure.code.into())
            }
        }
    }
}

fn authenticated_request(
    request: HttpRequest,
    credential: ChatGptRequestCredential,
) -> Result<HttpRequest, PostprocessError> {
    request
        .with_access_token_header("authorization", credential.access, "Bearer ")
        .and_then(|request| request.with_secret_header("chatgpt-account-id", credential.account_id))
        .and_then(|request| request.with_public_header("originator", "corti"))
        .and_then(|request| {
            request.with_public_header("user-agent", format!("corti/{}", env!("CARGO_PKG_VERSION")))
        })
        .map_err(|_| ErrorCode::Internal.into())
}

fn build_chatgpt_wire(
    _request: &HostedRequest,
    credential: ChatGptRequestCredential,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<HttpRequest, PostprocessError> {
    let request = HttpRequest::new(
        HttpMethod::Post,
        parse_fixed_url(RESPONSES_URL).map_err(|error| error.error_code())?,
    )
    .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
    .with_public_header("accept", "text/event-stream")
    .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
    .with_public_header("content-type", "application/json")
    .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
    .with_public_header("openai-beta", "responses=experimental")
    .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
    .with_json_body(body)
    .with_timeout(timeout);
    authenticated_request(request, credential)
}

fn validate_chatgpt_request(
    request: &HostedRequest,
    catalog: &ModelCatalog,
    configured_max_output: u64,
) -> Result<(), PostprocessError> {
    let descriptor = KnownTransport::ChatGptSubscription.descriptor();
    if request.provider != descriptor.provider || request.transport != descriptor.transport {
        return Err(ErrorCode::PolicyBlocked.into());
    }
    validate_prompt_layout(request)?;
    if request.cache_policy.provider != ProviderCacheMode::Unavailable {
        return Err(ErrorCode::PolicyBlocked.into());
    }
    let model = catalog
        .find_exact(&request.model, None)
        .ok_or_else(|| PostprocessError::from(ErrorCode::ModelUnavailable))?;
    if !model.account_scoped_available
        || model.support_tier != SupportTier::Experimental
        || !model.capabilities.text_input
        || !model.capabilities.text_output
        || !model.capabilities.streaming
        || !model.capabilities.structured_output
        || configured_max_output > model.max_output_tokens
    {
        return Err(ErrorCode::ModelUnavailable.into());
    }
    Ok(())
}

fn chatgpt_request_body(
    request: &HostedRequest,
    max_output_tokens: u64,
) -> Result<Vec<u8>, PostprocessError> {
    let instructions = request
        .prompt
        .messages()
        .iter()
        .filter(|message| message.role() == corti_postprocess::PromptRole::Developer)
        .map(|message| message.content())
        .collect::<Vec<_>>()
        .join("\n\n");
    let input = request
        .prompt
        .messages()
        .iter()
        .filter(|message| message.role() == corti_postprocess::PromptRole::User)
        .map(|message| {
            json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": message.content()}],
            })
        })
        .collect::<Vec<_>>();
    json_bytes(&json!({
        "model": request.model.as_str(),
        "store": false,
        "stream": true,
        "instructions": instructions,
        "input": input,
        "max_output_tokens": max_output_tokens,
        "text": {
            "verbosity": "low",
            "format": {
                "type": "json_schema",
                "name": output_schema_name(request.prompt.task()),
                "strict": true,
                "schema": output_schema(request.prompt.task()),
            }
        }
    }))
}

#[derive(Deserialize)]
struct ChatGptModelList {
    #[serde(default)]
    models: Vec<ChatGptModel>,
}

#[derive(Deserialize)]
struct ChatGptModel {
    slug: String,
    #[serde(default)]
    supported_in_api: bool,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_context_window: Option<i64>,
}

struct ChatGptStreamState {
    text: TextCollector,
    completed: bool,
    terminal_usage: Option<NormalizedUsage>,
    terminal_at: Option<u64>,
}

impl ChatGptStreamState {
    fn new(max_bytes: usize) -> Self {
        Self {
            text: TextCollector::new(max_bytes),
            completed: false,
            terminal_usage: None,
            terminal_at: None,
        }
    }

    fn process(
        &mut self,
        event: SseEvent,
        request: &HostedRequest,
        sink: &dyn ProviderEventSink,
        clock: &dyn Clock,
    ) -> Result<(), ExecFailure> {
        if event.data == "[DONE]" {
            return Ok(());
        }
        let envelope: ChatGptEnvelope = serde_json::from_str(&event.data)
            .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
        if self.completed {
            return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
        }
        match envelope.kind.as_str() {
            "response.output_text.delta" => {
                let payload: ChatGptTextDelta = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                self.text
                    .push(&payload.delta, request, sink, clock)
                    .map_err(|error| ExecFailure::new(error.code, true))?;
            }
            "response.completed" => {
                let payload: ChatGptCompleted = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                if payload.response.model != request.model.as_str()
                    || payload
                        .response
                        .status
                        .as_deref()
                        .is_some_and(|status| status != "completed")
                {
                    return Err(ExecFailure::new(ErrorCode::ModelUnavailable, true));
                }
                self.terminal_usage = Some(
                    payload
                        .response
                        .usage
                        .map(normalize_chatgpt_usage)
                        .transpose()
                        .map_err(|code| ExecFailure::new(code, true))?
                        .unwrap_or_else(NormalizedUsage::unknown),
                );
                self.completed = true;
                self.terminal_at = Some(clock.monotonic_micros());
            }
            "response.failed" | "response.incomplete" | "error" => {
                let payload: ChatGptFailure = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::Provider, true))?;
                let code = if envelope.kind == "response.incomplete" {
                    ErrorCode::MalformedOutput
                } else {
                    chatgpt_error_code(payload.code.as_deref())
                };
                return Err(ExecFailure::new(code, true));
            }
            _ => {}
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), ExecFailure> {
        if !self.completed || self.text.text().is_empty() {
            return Err(ExecFailure {
                code: ErrorCode::MalformedOutput,
                usage: self.terminal_usage,
                dispatched: true,
            });
        }
        Ok(())
    }
}

fn normalize_chatgpt_usage(usage: ChatGptUsage) -> Result<NormalizedUsage, ErrorCode> {
    let complete = usage.input_tokens.is_some() && usage.output_tokens.is_some();
    NormalizedUsage::try_from(RawUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_read_tokens: usage.input_tokens_details.cached_tokens,
        cached_write_tokens: None,
        reasoning_tokens: usage.output_tokens_details.reasoning_tokens,
        usage_complete: complete,
    })
    .map_err(|_| ErrorCode::MalformedOutput)
}

fn chatgpt_error_code(code: Option<&str>) -> ErrorCode {
    match code {
        Some("invalid_api_key" | "authentication_error") => ErrorCode::AuthRejected,
        Some("permission_denied") => ErrorCode::Permission,
        Some("insufficient_quota" | "usage_limit_reached") => ErrorCode::Quota,
        Some("rate_limit_exceeded") => ErrorCode::RateLimited,
        Some("model_not_found") => ErrorCode::ModelUnavailable,
        Some(_) | None => ErrorCode::Provider,
    }
}

#[derive(Deserialize)]
struct ChatGptEnvelope {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct ChatGptTextDelta {
    delta: String,
}

#[derive(Deserialize)]
struct ChatGptCompleted {
    response: ChatGptCompletedResponse,
}

#[derive(Deserialize)]
struct ChatGptCompletedResponse {
    model: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    usage: Option<ChatGptUsage>,
}

#[derive(Deserialize)]
struct ChatGptFailure {
    #[serde(default)]
    code: Option<String>,
}

#[derive(Deserialize)]
struct ChatGptUsage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    input_tokens_details: ChatGptInputTokenDetails,
    #[serde(default)]
    output_tokens_details: ChatGptOutputTokenDetails,
}

#[derive(Default, Deserialize)]
struct ChatGptInputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<i64>,
}

#[derive(Default, Deserialize)]
struct ChatGptOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
struct ExecFailure {
    code: ErrorCode,
    usage: Option<NormalizedUsage>,
    dispatched: bool,
}

impl ExecFailure {
    const fn new(code: ErrorCode, dispatched: bool) -> Self {
        Self {
            code,
            usage: None,
            dispatched,
        }
    }

    fn from_error(error: PostprocessError) -> Self {
        Self::new(error.code, false)
    }

    fn from_send(failure: SendFailure) -> Self {
        Self::new(failure.code, failure.dispatched)
    }

    const fn with_dispatched(mut self) -> Self {
        self.dispatched = true;
        self
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        sync::atomic::{AtomicI64, AtomicU64},
    };

    use super::*;
    use crate::transport::{HttpResponseBody, RequestDelivery, TransportError};

    #[derive(Default)]
    struct MemoryStore {
        document: Mutex<Option<Vec<u8>>>,
    }

    impl ChatGptCredentialStore for MemoryStore {
        fn load(&self) -> Result<Option<Vec<u8>>, ChatGptStoreError> {
            Ok(self.document.lock().unwrap().clone())
        }

        fn save(&self, document: &[u8]) -> Result<(), ChatGptStoreError> {
            *self.document.lock().unwrap() = Some(document.to_vec());
            Ok(())
        }

        fn clear(&self) -> Result<(), ChatGptStoreError> {
            *self.document.lock().unwrap() = None;
            Ok(())
        }
    }

    struct FailingSaveStore {
        document: Mutex<Option<Vec<u8>>>,
    }

    impl ChatGptCredentialStore for FailingSaveStore {
        fn load(&self) -> Result<Option<Vec<u8>>, ChatGptStoreError> {
            Ok(self.document.lock().unwrap().clone())
        }

        fn save(&self, _document: &[u8]) -> Result<(), ChatGptStoreError> {
            Err(ChatGptStoreError::Unavailable)
        }

        fn clear(&self) -> Result<(), ChatGptStoreError> {
            *self.document.lock().unwrap() = None;
            Ok(())
        }
    }

    struct TestClock {
        monotonic: AtomicU64,
        unix: AtomicI64,
    }

    impl TestClock {
        fn new(unix: i64) -> Self {
            Self {
                monotonic: AtomicU64::new(10),
                unix: AtomicI64::new(unix),
            }
        }
    }

    impl Clock for TestClock {
        fn monotonic_micros(&self) -> u64 {
            self.monotonic.fetch_add(10, Ordering::Relaxed)
        }
    }

    impl WallClock for TestClock {
        fn unix_seconds(&self) -> i64 {
            self.unix.load(Ordering::Relaxed)
        }
    }

    struct BytesBody(Option<Vec<u8>>);

    impl HttpResponseBody for BytesBody {
        fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(self.0.take())
        }
    }

    fn response(status: u16, body: Value) -> HttpResponse {
        HttpResponse::new(
            status,
            vec![("content-type".into(), "application/json".into())],
            Box::new(BytesBody(Some(serde_json::to_vec(&body).unwrap()))),
        )
    }

    type CapturedAuthRequests = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    struct ScriptedTransport {
        responses: Arc<Mutex<VecDeque<HttpResponse>>>,
        requests: CapturedAuthRequests,
    }

    impl HttpTransport for ScriptedTransport {
        fn send(
            &mut self,
            request: &HttpRequest,
            _cancel: &CancellationToken,
        ) -> Result<HttpResponse, TransportError> {
            self.requests
                .lock()
                .unwrap()
                .push((request.url().as_str().to_owned(), request.body().to_vec()));
            self.responses.lock().unwrap().pop_front().ok_or_else(|| {
                TransportError::new(TransportErrorKind::Protocol, RequestDelivery::NotSent)
            })
        }
    }

    fn jwt(account: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&json!({JWT_AUTH_CLAIM: {"chatgpt_account_id": account}})).unwrap(),
        );
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn device_flow_persists_a_corti_owned_rotating_credential() {
        let responses = Arc::new(Mutex::new(VecDeque::from([
            response(
                200,
                json!({"device_auth_id":"device-secret","user_code":"ABCD-EFGH","interval":1}),
            ),
            response(403, json!({"error":"deviceauth_authorization_pending"})),
            response(
                200,
                json!({"authorization_code":"authorization-secret","code_verifier":"verifier-secret"}),
            ),
            response(
                200,
                json!({"access_token":jwt("account-one"),"refresh_token":"refresh-one","expires_in":3600}),
            ),
        ])));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let store = Arc::new(MemoryStore::default());
        let clock = Arc::new(TestClock::new(1_000));
        let auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses,
                requests: requests.clone(),
            }),
            clock,
            store.clone(),
        );

        let display = auth.start_device_login().unwrap();
        assert_eq!(display.verification_url(), CHATGPT_DEVICE_VERIFICATION_URL);
        assert_eq!(display.user_code(), "ABCD-EFGH");
        assert!(!format!("{display:?}").contains("ABCD-EFGH"));
        assert_eq!(
            auth.poll_device_login(display.login_id()).unwrap(),
            ChatGptLoginPoll::Pending
        );
        assert_eq!(
            auth.poll_device_login(display.login_id()).unwrap(),
            ChatGptLoginPoll::Authorized { durable: true }
        );
        assert!(matches!(
            auth.credential_state(),
            CredentialState::Ready {
                source: CredentialSourceKind::ChatGptDevice,
                ..
            }
        ));
        let stored = store.document.lock().unwrap().clone().unwrap();
        let stored_text = String::from_utf8(stored).unwrap();
        assert!(stored_text.contains("refresh-one"));
        assert!(!format!("{auth:?}").contains("refresh-one"));

        let requests = requests.lock().unwrap();
        assert!(requests[0].0.starts_with(DEVICE_CODE_URL));
        assert!(requests[3].0.starts_with(TOKEN_URL));
        let token_form = String::from_utf8(requests[3].1.clone()).unwrap();
        assert!(token_form.contains("grant_type=authorization_code"));
        assert!(token_form.contains("code_verifier=verifier-secret"));
    }

    #[test]
    fn failed_replacement_login_restores_the_retained_credential() {
        let store = Arc::new(MemoryStore::default());
        persist_credentials(
            store.as_ref(),
            &StoredCredentials {
                version: AUTH_VERSION,
                access: jwt("account-one"),
                refresh: "refresh-one".into(),
                expires_at: 10_000,
                account_id: "account-one".into(),
            },
        )
        .unwrap();
        let auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            Arc::new(TestClock::new(1_000)),
            store,
        );
        assert!(matches!(
            auth.start_device_login(),
            Err(ChatGptAuthError::Protocol)
        ));
        assert!(matches!(
            auth.credential_state(),
            CredentialState::Ready { .. }
        ));
        drop(auth.request_credential(false).unwrap());
    }

    #[test]
    fn explicit_403_denial_and_expiry_are_terminal_not_generic_pending() {
        for (code, expected) in [
            ("access_denied", ChatGptLoginPoll::Denied),
            ("expired_token", ChatGptLoginPoll::Expired),
        ] {
            let responses = Arc::new(Mutex::new(VecDeque::from([
                response(
                    200,
                    json!({"device_auth_id":"device","user_code":"CODE","interval":1}),
                ),
                response(403, json!({"error":code})),
            ])));
            let auth = ChatGptSubscriptionAuth::new(
                Box::new(ScriptedTransport {
                    responses,
                    requests: Arc::new(Mutex::new(Vec::new())),
                }),
                Arc::new(TestClock::new(1_000)),
                Arc::new(MemoryStore::default()),
            );
            let display = auth.start_device_login().unwrap();
            assert_eq!(
                auth.poll_device_login(display.login_id()).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn canceled_login_worker_cannot_poll_or_cancel_a_replacement_login() {
        let responses = Arc::new(Mutex::new(VecDeque::from([
            response(
                200,
                json!({"device_auth_id":"device-one","user_code":"CODE-ONE","interval":1}),
            ),
            response(
                200,
                json!({"device_auth_id":"device-two","user_code":"CODE-TWO","interval":1}),
            ),
        ])));
        let auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses,
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            Arc::new(TestClock::new(1_000)),
            Arc::new(MemoryStore::default()),
        );
        let first = auth.start_device_login().unwrap();
        auth.cancel_device_login(first.login_id()).unwrap();
        let second = auth.start_device_login().unwrap();
        assert_ne!(first.login_id(), second.login_id());
        assert_eq!(
            auth.poll_interval(first.login_id()),
            Err(ChatGptAuthError::InvalidState)
        );
        assert_eq!(
            auth.poll_device_login(first.login_id()),
            Err(ChatGptAuthError::InvalidState)
        );
        assert_eq!(
            auth.cancel_device_login(first.login_id()),
            Err(ChatGptAuthError::InvalidState)
        );
        assert_eq!(auth.current_login_id().unwrap(), second.login_id());
        assert!(matches!(
            auth.request_credential(false),
            Err(ChatGptAuthError::Busy)
        ));
        assert!(matches!(
            auth.credential_state(),
            CredentialState::DeviceAuthorization { ref user_code, .. } if user_code == "CODE-TWO"
        ));
    }

    #[test]
    fn initial_login_with_a_failed_store_is_usable_only_in_memory_and_never_reported_ready() {
        let responses = Arc::new(Mutex::new(VecDeque::from([
            response(
                200,
                json!({"device_auth_id":"device-secret","user_code":"ABCD-EFGH","interval":1}),
            ),
            response(
                200,
                json!({"authorization_code":"authorization-secret","code_verifier":"verifier-secret"}),
            ),
            response(
                200,
                json!({"access_token":jwt("account-one"),"refresh_token":"refresh-one","expires_in":3600}),
            ),
        ])));
        let store = Arc::new(FailingSaveStore {
            document: Mutex::new(None),
        });
        let auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses,
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            Arc::new(TestClock::new(1_000)),
            store,
        );
        let display = auth.start_device_login().unwrap();
        assert_eq!(
            auth.poll_device_login(display.login_id()).unwrap(),
            ChatGptLoginPoll::Authorized { durable: false }
        );
        assert!(matches!(
            auth.credential_state(),
            CredentialState::Error {
                code: ErrorCode::Cache
            }
        ));
        assert!(auth.connection_scope_id().is_ok());
    }

    #[test]
    fn startup_loads_credentials_and_scope_changes_with_the_account() {
        let store = Arc::new(MemoryStore::default());
        let first = StoredCredentials {
            version: AUTH_VERSION,
            access: jwt("account-one"),
            refresh: "refresh-one".into(),
            expires_at: 10_000,
            account_id: "account-one".into(),
        };
        persist_credentials(store.as_ref(), &first).unwrap();
        let auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            Arc::new(TestClock::new(1_000)),
            store,
        );
        assert!(matches!(
            auth.credential_state(),
            CredentialState::Ready { .. }
        ));
        let scope = auth.connection_scope_id().unwrap();
        assert!(scope.as_str().starts_with("chatgpt-v1-"));
        assert!(!scope.as_str().contains("account-one"));

        let second_store = Arc::new(MemoryStore::default());
        persist_credentials(
            second_store.as_ref(),
            &StoredCredentials {
                version: AUTH_VERSION,
                access: jwt("account-two"),
                refresh: "refresh-two".into(),
                expires_at: 10_000,
                account_id: "account-two".into(),
            },
        )
        .unwrap();
        let second_auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            Arc::new(TestClock::new(1_000)),
            second_store,
        );
        assert_ne!(scope, second_auth.connection_scope_id().unwrap());
    }

    #[test]
    fn forced_refresh_rotates_and_persists_the_replacement_token_pair() {
        let store = Arc::new(MemoryStore::default());
        let original = StoredCredentials {
            version: AUTH_VERSION,
            access: jwt("account-one"),
            refresh: "refresh-one".into(),
            expires_at: 10_000,
            account_id: "account-one".into(),
        };
        persist_credentials(store.as_ref(), &original).unwrap();
        let responses = Arc::new(Mutex::new(VecDeque::from([response(
            200,
            json!({
                "access_token": jwt("account-one"),
                "refresh_token": "refresh-two",
                "expires_in": 7200
            }),
        )])));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses,
                requests: requests.clone(),
            }),
            Arc::new(TestClock::new(1_000)),
            store.clone(),
        );

        drop(auth.request_credential(true).unwrap());
        let stored = String::from_utf8(store.document.lock().unwrap().clone().unwrap()).unwrap();
        assert!(stored.contains("refresh-two"));
        assert!(!stored.contains("refresh-one"));
        let form = String::from_utf8(requests.lock().unwrap()[0].1.clone()).unwrap();
        assert!(form.contains("grant_type=refresh_token"));
        assert!(form.contains("refresh_token=refresh-one"));
        assert!(!format!("{auth:?}").contains("refresh-two"));
    }

    #[test]
    fn refresh_cannot_silently_switch_the_account_scope() {
        let store = Arc::new(MemoryStore::default());
        let original = StoredCredentials {
            version: AUTH_VERSION,
            access: jwt("account-one"),
            refresh: "refresh-one".into(),
            expires_at: 10_000,
            account_id: "account-one".into(),
        };
        persist_credentials(store.as_ref(), &original).unwrap();
        let auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses: Arc::new(Mutex::new(VecDeque::from([response(
                    200,
                    json!({
                        "access_token": jwt("account-two"),
                        "refresh_token": "refresh-two",
                        "expires_in": 7200
                    }),
                )]))),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            Arc::new(TestClock::new(1_000)),
            store.clone(),
        );
        assert!(matches!(
            auth.request_credential(true),
            Err(ChatGptAuthError::Protocol)
        ));
        let stored = String::from_utf8(store.document.lock().unwrap().clone().unwrap()).unwrap();
        assert!(stored.contains("refresh-one"));
        assert!(!stored.contains("refresh-two"));
    }

    #[test]
    fn refresh_store_failure_keeps_the_rotated_token_for_this_call_but_marks_auth_non_durable() {
        let original = StoredCredentials {
            version: AUTH_VERSION,
            access: jwt("account-one"),
            refresh: "refresh-one".into(),
            expires_at: 10_000,
            account_id: "account-one".into(),
        };
        let store = Arc::new(FailingSaveStore {
            document: Mutex::new(Some(serde_json::to_vec(&original).unwrap())),
        });
        let auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses: Arc::new(Mutex::new(VecDeque::from([response(
                    200,
                    json!({
                        "access_token": jwt("account-one"),
                        "refresh_token": "refresh-two",
                        "expires_in": 7200
                    }),
                )]))),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            Arc::new(TestClock::new(1_000)),
            store,
        );
        drop(auth.request_credential(true).unwrap());
        assert!(matches!(
            auth.credential_state(),
            CredentialState::Error {
                code: ErrorCode::Cache
            }
        ));
    }

    #[test]
    fn malformed_or_missing_store_never_exposes_a_credential() {
        let store = Arc::new(MemoryStore::default());
        *store.document.lock().unwrap() = Some(br#"{"version":1,"access":"sentinel"}"#.to_vec());
        let auth = ChatGptSubscriptionAuth::new(
            Box::new(ScriptedTransport {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                requests: Arc::new(Mutex::new(Vec::new())),
            }),
            Arc::new(TestClock::new(1_000)),
            store,
        );
        assert!(matches!(
            auth.credential_state(),
            CredentialState::Error { .. }
        ));
        assert!(!format!("{auth:?}").contains("sentinel"));
    }
}
