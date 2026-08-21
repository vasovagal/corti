//! Experimental, broker-owned Codex app-server authentication boundary.
//!
//! This entire module is compile-gated. It deliberately contains no process launcher, filesystem path,
//! credential-file parser, token field, or `ProviderAdapter` implementation. The injected broker owns the
//! app-server protocol, persistence, refresh, and OS-keyring material; Corti sees only device-code display
//! fields and a secret-free readiness result.

use std::fmt;

use corti_postprocess::{CredentialSourceKind, CredentialState, ErrorCode};
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodexAppServerGate {
    /// Per-build/runtime experimental switch. Defaults off even in a feature-enabled build.
    pub experimental_enabled: bool,
    /// Explicit product/legal approval for app-server use. A user preference alone is insufficient.
    pub product_approved: bool,
}

impl CodexAppServerGate {
    pub const fn approved(self) -> bool {
        self.experimental_enabled && self.product_approved
    }
}

/// Secret-free broker launch posture attested by the injected app-owned process boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CodexBrokerPosture {
    pub local_stdio_only: bool,
    pub dedicated_private_home: bool,
    pub os_keyring: bool,
    pub empty_private_working_directory: bool,
    pub deny_all_approvals: bool,
    pub tools_disabled: bool,
}

impl CodexBrokerPosture {
    pub const fn satisfies_policy(self) -> bool {
        self.local_stdio_only
            && self.dedicated_private_home
            && self.os_keyring
            && self.empty_private_working_directory
            && self.deny_all_approvals
            && self.tools_disabled
    }
}

/// Display-only fields from broker-owned `account/login/start {type:"chatgptDeviceCode"}`.
///
/// These values are intended for the user. They are not subscription tokens, and this type has no generic
/// payload or extension map in which a token could be smuggled through the interface.
#[derive(Clone, PartialEq, Eq)]
pub struct CodexDeviceAuthorization {
    verification_url: String,
    user_code: String,
    login_id: String,
}

impl CodexDeviceAuthorization {
    pub fn new(
        verification_url: impl Into<String>,
        user_code: impl Into<String>,
        login_id: impl Into<String>,
    ) -> Result<Self, CodexAuthorizationError> {
        let verification_url = verification_url.into();
        let user_code = user_code.into();
        let login_id = login_id.into();
        validate_verification_url(&verification_url)?;
        validate_display_field(&user_code, 256)?;
        validate_display_field(&login_id, 1024)?;
        Ok(Self {
            verification_url,
            user_code,
            login_id,
        })
    }

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

impl fmt::Debug for CodexDeviceAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexDeviceAuthorization")
            .field("verification_url", &"<display-only-redacted>")
            .field("user_code_bytes", &self.user_code.len())
            .field("login_id_bytes", &self.login_id.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CodexAuthorizationError {
    #[error("invalid device verification URL")]
    InvalidVerificationUrl,
    #[error("invalid device authorization display field")]
    InvalidDisplayField,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexLoginPoll {
    Pending,
    Authorized,
    Denied,
    Expired,
}

/// Fixed, content-free broker failures. Raw JSON-RPC errors and process output never cross this seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CodexBrokerError {
    #[error("Codex broker exited")]
    Exited,
    #[error("Codex broker protocol failure")]
    Protocol,
    #[error("Codex broker unavailable")]
    Unavailable,
}

impl CodexBrokerError {
    pub const fn error_code(self) -> ErrorCode {
        match self {
            Self::Exited => ErrorCode::BrokerExited,
            Self::Protocol | Self::Unavailable => ErrorCode::Provider,
        }
    }
}

/// Typed broker interface for the only approved Codex authentication flow.
///
/// There is intentionally no raw JSON method and no token-returning method. Implementations own app-server
/// token parsing, persistence, and refresh behind an OS keyring.
pub trait CodexAppServerBroker: Send {
    fn posture(&self) -> CodexBrokerPosture;

    fn start_device_code_login(&mut self) -> Result<CodexDeviceAuthorization, CodexBrokerError>;

    fn poll_device_code_login(
        &mut self,
        login_id: &str,
    ) -> Result<CodexLoginPoll, CodexBrokerError>;

    fn cancel_device_code_login(&mut self, login_id: &str) -> Result<(), CodexBrokerError>;

    /// Best-effort termination used when the protocol or policy boundary fails.
    fn terminate(&mut self) {}
}

#[derive(Clone, PartialEq, Eq)]
pub enum CodexDeviceCodeState {
    Disconnected,
    Starting,
    DeviceAuthorization(CodexDeviceAuthorization),
    Ready,
    Rejected,
    Expired,
    Error(ErrorCode),
}

impl CodexDeviceCodeState {
    pub fn credential_state(&self) -> CredentialState {
        match self {
            Self::Disconnected => CredentialState::Absent,
            Self::Starting => CredentialState::Resolving,
            Self::DeviceAuthorization(authorization) => CredentialState::DeviceAuthorization {
                verification_url: authorization.verification_url.clone(),
                user_code: authorization.user_code.clone(),
                login_id: authorization.login_id.clone(),
            },
            Self::Ready => CredentialState::Ready {
                expires_at_unix_ms: None,
                source: CredentialSourceKind::BrokerKeyring,
            },
            Self::Rejected => CredentialState::Rejected,
            Self::Expired => CredentialState::Absent,
            Self::Error(code) => CredentialState::Error { code: *code },
        }
    }
}

impl fmt::Debug for CodexDeviceCodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => f.write_str("Disconnected"),
            Self::Starting => f.write_str("Starting"),
            Self::DeviceAuthorization(authorization) => f
                .debug_tuple("DeviceAuthorization")
                .field(authorization)
                .finish(),
            Self::Ready => f.write_str("Ready(<broker-keyring>)"),
            Self::Rejected => f.write_str("Rejected"),
            Self::Expired => f.write_str("Expired"),
            Self::Error(code) => f.debug_tuple("Error").field(code).finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CodexDeviceCodeError {
    #[error("Codex app-server is blocked by compile/product policy")]
    PolicyBlocked,
    #[error("Codex broker does not satisfy the required isolation posture")]
    UnsafeBroker,
    #[error("device-code operation is invalid in the current state")]
    InvalidState,
    #[error("Codex broker failed")]
    Broker(CodexBrokerError),
}

/// Approval-gated device-code state machine. Synchronous methods are intentional: process/runtime ownership
/// remains in the injected broker and no ambient Codex installation is consulted by this crate.
pub struct CodexDeviceCodeMachine<B: CodexAppServerBroker> {
    broker: B,
    state: CodexDeviceCodeState,
}

impl<B: CodexAppServerBroker> CodexDeviceCodeMachine<B> {
    pub fn new(mut broker: B, gate: CodexAppServerGate) -> Result<Self, CodexDeviceCodeError> {
        if !gate.approved() {
            return Err(CodexDeviceCodeError::PolicyBlocked);
        }
        if !broker.posture().satisfies_policy() {
            broker.terminate();
            return Err(CodexDeviceCodeError::UnsafeBroker);
        }
        Ok(Self {
            broker,
            state: CodexDeviceCodeState::Disconnected,
        })
    }

    pub const fn state(&self) -> &CodexDeviceCodeState {
        &self.state
    }

    pub fn start(&mut self) -> Result<&CodexDeviceCodeState, CodexDeviceCodeError> {
        if !matches!(
            self.state,
            CodexDeviceCodeState::Disconnected
                | CodexDeviceCodeState::Rejected
                | CodexDeviceCodeState::Expired
                | CodexDeviceCodeState::Error(_)
        ) {
            return Err(CodexDeviceCodeError::InvalidState);
        }
        self.state = CodexDeviceCodeState::Starting;
        match self.broker.start_device_code_login() {
            Ok(authorization) => {
                self.state = CodexDeviceCodeState::DeviceAuthorization(authorization);
                Ok(&self.state)
            }
            Err(error) => {
                self.state = CodexDeviceCodeState::Error(error.error_code());
                if error == CodexBrokerError::Protocol {
                    self.broker.terminate();
                }
                Err(CodexDeviceCodeError::Broker(error))
            }
        }
    }

    pub fn poll(&mut self) -> Result<&CodexDeviceCodeState, CodexDeviceCodeError> {
        let CodexDeviceCodeState::DeviceAuthorization(authorization) = &self.state else {
            return Err(CodexDeviceCodeError::InvalidState);
        };
        let login_id = authorization.login_id.clone();
        match self.broker.poll_device_code_login(&login_id) {
            Ok(CodexLoginPoll::Pending) => {}
            Ok(CodexLoginPoll::Authorized) => self.state = CodexDeviceCodeState::Ready,
            Ok(CodexLoginPoll::Denied) => self.state = CodexDeviceCodeState::Rejected,
            Ok(CodexLoginPoll::Expired) => self.state = CodexDeviceCodeState::Expired,
            Err(error) => {
                self.state = CodexDeviceCodeState::Error(error.error_code());
                if matches!(error, CodexBrokerError::Exited | CodexBrokerError::Protocol) {
                    self.broker.terminate();
                }
                return Err(CodexDeviceCodeError::Broker(error));
            }
        }
        Ok(&self.state)
    }

    pub fn cancel(&mut self) -> Result<(), CodexDeviceCodeError> {
        let CodexDeviceCodeState::DeviceAuthorization(authorization) = &self.state else {
            return Err(CodexDeviceCodeError::InvalidState);
        };
        let login_id = authorization.login_id.clone();
        if let Err(error) = self.broker.cancel_device_code_login(&login_id) {
            self.state = CodexDeviceCodeState::Error(error.error_code());
            if matches!(error, CodexBrokerError::Exited | CodexBrokerError::Protocol) {
                self.broker.terminate();
            }
            return Err(CodexDeviceCodeError::Broker(error));
        }
        self.state = CodexDeviceCodeState::Disconnected;
        Ok(())
    }

    pub fn mark_broker_exited(&mut self) {
        self.broker.terminate();
        self.state = CodexDeviceCodeState::Error(ErrorCode::BrokerExited);
    }

    /// A policy violation (for example an attempted tool or approval request observed by the broker) is
    /// fail-closed and permanently terminates this machine's broker session.
    pub fn reject_policy_violation(&mut self) {
        self.broker.terminate();
        self.state = CodexDeviceCodeState::Error(ErrorCode::PolicyBlocked);
    }
}

impl<B: CodexAppServerBroker> fmt::Debug for CodexDeviceCodeMachine<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodexDeviceCodeMachine")
            .field("state", &self.state)
            .field("broker", &"<injected-no-token-interface>")
            .finish()
    }
}

fn validate_verification_url(value: &str) -> Result<(), CodexAuthorizationError> {
    let url = Url::parse(value).map_err(|_| CodexAuthorizationError::InvalidVerificationUrl)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || value.len() > 4096
    {
        return Err(CodexAuthorizationError::InvalidVerificationUrl);
    }
    Ok(())
}

fn validate_display_field(value: &str, max_bytes: usize) -> Result<(), CodexAuthorizationError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(CodexAuthorizationError::InvalidDisplayField);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Default)]
    struct Calls {
        starts: usize,
        polls: usize,
        cancels: usize,
        terminated: usize,
    }

    struct FakeBroker {
        calls: Arc<Mutex<Calls>>,
        posture: CodexBrokerPosture,
        polls: Vec<CodexLoginPoll>,
    }

    impl CodexAppServerBroker for FakeBroker {
        fn posture(&self) -> CodexBrokerPosture {
            self.posture
        }

        fn start_device_code_login(
            &mut self,
        ) -> Result<CodexDeviceAuthorization, CodexBrokerError> {
            self.calls.lock().unwrap().starts += 1;
            CodexDeviceAuthorization::new(
                "https://example.invalid/device",
                "SYNTHETIC-CODE",
                "synthetic-login-id",
            )
            .map_err(|_| CodexBrokerError::Protocol)
        }

        fn poll_device_code_login(
            &mut self,
            _login_id: &str,
        ) -> Result<CodexLoginPoll, CodexBrokerError> {
            self.calls.lock().unwrap().polls += 1;
            Ok(self.polls.remove(0))
        }

        fn cancel_device_code_login(&mut self, _login_id: &str) -> Result<(), CodexBrokerError> {
            self.calls.lock().unwrap().cancels += 1;
            Ok(())
        }

        fn terminate(&mut self) {
            self.calls.lock().unwrap().terminated += 1;
        }
    }

    fn secure_posture() -> CodexBrokerPosture {
        CodexBrokerPosture {
            local_stdio_only: true,
            dedicated_private_home: true,
            os_keyring: true,
            empty_private_working_directory: true,
            deny_all_approvals: true,
            tools_disabled: true,
        }
    }

    #[test]
    fn approval_gate_prevents_any_login_start() {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let broker = FakeBroker {
            calls: calls.clone(),
            posture: secure_posture(),
            polls: Vec::new(),
        };
        let error = CodexDeviceCodeMachine::new(broker, CodexAppServerGate::default()).unwrap_err();
        assert_eq!(error, CodexDeviceCodeError::PolicyBlocked);
        assert_eq!(calls.lock().unwrap().starts, 0);
    }

    #[test]
    fn device_code_flow_exposes_only_display_fields_and_broker_readiness() {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let broker = FakeBroker {
            calls: calls.clone(),
            posture: secure_posture(),
            polls: vec![CodexLoginPoll::Pending, CodexLoginPoll::Authorized],
        };
        let mut machine = CodexDeviceCodeMachine::new(
            broker,
            CodexAppServerGate {
                experimental_enabled: true,
                product_approved: true,
            },
        )
        .unwrap();
        let state = machine.start().unwrap();
        let debug = format!("{state:?}");
        assert!(!debug.contains("SYNTHETIC-CODE"));
        assert!(matches!(
            state.credential_state(),
            CredentialState::DeviceAuthorization { .. }
        ));
        assert!(matches!(
            machine.poll().unwrap(),
            CodexDeviceCodeState::DeviceAuthorization(_)
        ));
        assert!(matches!(
            machine.poll().unwrap(),
            CodexDeviceCodeState::Ready
        ));
        assert!(matches!(
            machine.state().credential_state(),
            CredentialState::Ready {
                source: CredentialSourceKind::BrokerKeyring,
                ..
            }
        ));
        assert_eq!(calls.lock().unwrap().polls, 2);
    }

    #[test]
    fn unsafe_broker_and_policy_violation_fail_closed() {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let broker = FakeBroker {
            calls: calls.clone(),
            posture: CodexBrokerPosture::default(),
            polls: Vec::new(),
        };
        let error = CodexDeviceCodeMachine::new(
            broker,
            CodexAppServerGate {
                experimental_enabled: true,
                product_approved: true,
            },
        )
        .unwrap_err();
        assert_eq!(error, CodexDeviceCodeError::UnsafeBroker);
        assert_eq!(calls.lock().unwrap().terminated, 1);

        let broker = FakeBroker {
            calls: calls.clone(),
            posture: secure_posture(),
            polls: Vec::new(),
        };
        let mut machine = CodexDeviceCodeMachine::new(
            broker,
            CodexAppServerGate {
                experimental_enabled: true,
                product_approved: true,
            },
        )
        .unwrap();
        machine.reject_policy_violation();
        assert_eq!(calls.lock().unwrap().terminated, 2);
        assert!(matches!(
            machine.state(),
            CodexDeviceCodeState::Error(ErrorCode::PolicyBlocked)
        ));
    }
}
