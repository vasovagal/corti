//! AWS credential resolution for the Bedrock adapter.
//!
//! `corti-postprocess-providers` performs no ambient discovery, so every flavor of AWS credential is
//! resolved here, where the app already owns `aws-config` for the Transcribe backend. Only resolved,
//! short-lived material crosses into the adapter — never a profile path, an SSO cache file, or a role
//! session the adapter would have to manage.

use std::sync::{Arc, Mutex};

use corti_postprocess::{CredentialSourceKind, CredentialState, ErrorCode};
use corti_postprocess_providers::{AwsCredentialSource, AwsCredentials, CredentialError};

use crate::{
    keychain,
    postprocess_config::{AwsCredentialMode, SecretPurpose},
};

/// Refresh this far ahead of expiry so a session cannot lapse mid-stream.
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;
/// Assumed-role sessions are deliberately short; the resolver renews them rather than holding a long one.
/// Only the `aws` feature can call `sts:AssumeRole`.
#[cfg(feature = "aws")]
const ASSUME_ROLE_DURATION_SECONDS: i32 = 60 * 60;
#[cfg(feature = "aws")]
const ROLE_SESSION_NAME: &str = "corti-hosted-postprocess";

/// The non-secret half of a Bedrock connection, snapshotted from `hosted.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BedrockCredentialConfig {
    pub(crate) mode: AwsCredentialMode,
    pub(crate) profile: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) role_arn: Option<String>,
}

impl BedrockCredentialConfig {
    /// The label the UI shows for a ready credential. It names the flavor only.
    pub(crate) const fn source_kind(&self) -> CredentialSourceKind {
        match self.mode {
            AwsCredentialMode::DefaultChain => CredentialSourceKind::AwsDefaultChain,
            AwsCredentialMode::Profile => CredentialSourceKind::AwsProfile,
            AwsCredentialMode::StaticKeychain => CredentialSourceKind::AwsStaticKeychain,
            AwsCredentialMode::AssumeRole => CredentialSourceKind::AwsAssumedRole,
            AwsCredentialMode::Sso => CredentialSourceKind::AwsSso,
        }
    }
}

/// Reads the connection's non-secret configuration. Backed by the live `hosted.toml` snapshot, so a
/// mode or profile change takes effect without any separate re-sync step.
pub(crate) type ConfigSource = Arc<dyn Fn() -> BedrockCredentialConfig + Send + Sync>;

/// Shared resolution state. The coordinator reads `state()` for the pane while the adapter calls
/// `resolve()` on the request path; both go through this one type so they cannot disagree.
pub(crate) struct BedrockCredentialResolver {
    config: ConfigSource,
    rejection: Mutex<Option<BedrockCredentialConfig>>,
}

impl BedrockCredentialResolver {
    pub(crate) fn new(config: ConfigSource) -> Arc<Self> {
        Arc::new(Self {
            config,
            rejection: Mutex::new(None),
        })
    }

    pub(crate) fn config(&self) -> BedrockCredentialConfig {
        (self.config)()
    }

    /// Secret-free projection for the Settings pane. An expiring session reports `Refreshing` rather
    /// than a hard failure, which is what lets the pane show renewal instead of an error.
    pub(crate) fn state(&self) -> CredentialState {
        let config = self.config();
        // A rejection sticks to the configuration that earned it. Re-resolving would otherwise report
        // Ready again straight after AWS refused the credential; editing the mode, profile, or role
        // clears it, which is exactly the user action that could plausibly fix it.
        if self.rejection.lock().unwrap().as_ref() == Some(&config) {
            return CredentialState::Rejected;
        }
        match self.resolve_credentials(&config) {
            Ok(credentials) => {
                let expires_at_unix_ms = credentials.expires_at_unix_ms();
                if expires_at_unix_ms.is_some_and(is_expiring) {
                    CredentialState::Refreshing
                } else {
                    CredentialState::Ready {
                        expires_at_unix_ms,
                        source: config.source_kind(),
                    }
                }
            }
            Err(CredentialError::Absent) => CredentialState::Absent,
            // An expired SSO or role session is a rejection the user can fix by re-logging in; the
            // "run aws sso login" hint belongs to the UI layer, not to this type.
            Err(CredentialError::Rejected) => CredentialState::Rejected,
            Err(CredentialError::Unavailable) => CredentialState::Error {
                code: ErrorCode::AuthUnarmed,
            },
        }
    }

    fn resolve_credentials(
        &self,
        config: &BedrockCredentialConfig,
    ) -> Result<AwsCredentials, CredentialError> {
        match config.mode {
            AwsCredentialMode::StaticKeychain => keychain_credentials(),
            #[cfg(feature = "aws")]
            _ => chain_credentials(config),
            #[cfg(not(feature = "aws"))]
            _ => Err(CredentialError::Unavailable),
        }
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

/// The static keypair a user pasted through the secure-entry sheet.
fn keychain_credentials() -> Result<AwsCredentials, CredentialError> {
    use zeroize::Zeroize as _;

    fn slot(purpose: SecretPurpose) -> Result<Option<String>, CredentialError> {
        let Some(bytes) = keychain::read(purpose).map_err(|_| CredentialError::Unavailable)? else {
            return Ok(None);
        };
        let value = String::from_utf8(bytes).map_err(|error| {
            let mut bytes = error.into_bytes();
            bytes.zeroize();
            CredentialError::Unavailable
        })?;
        Ok(Some(value).filter(|value| !value.is_empty()))
    }

    let access_key_id = slot(SecretPurpose::AwsAccessKeyId)?.ok_or(CredentialError::Absent)?;
    let secret_access_key =
        slot(SecretPurpose::AwsSecretAccessKey)?.ok_or(CredentialError::Absent)?;
    let session_token = slot(SecretPurpose::AwsSessionToken)?;
    AwsCredentials::new(access_key_id, secret_access_key, session_token, None)
        .map_err(|_| CredentialError::Unavailable)
}

/// Default chain, named profile, SSO, and assume-role all run through `aws-config`, which already knows
/// how to read `~/.aws`, the SSO token cache, and the environment.
#[cfg(feature = "aws")]
fn chain_credentials(config: &BedrockCredentialConfig) -> Result<AwsCredentials, CredentialError> {
    use aws_config::{BehaviorVersion, Region};
    use aws_credential_types::provider::ProvideCredentials as _;

    // A throwaway current-thread runtime, matching how `transcribe.rs` drives its one async load. This
    // resolver is only ever called from the blocking hosted worker, never from inside a runtime.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| CredentialError::Unavailable)?;

    runtime.block_on(async {
        let mut loader = aws_config::defaults(BehaviorVersion::latest());
        if let Some(region) = config.region.as_deref().filter(|value| !value.is_empty()) {
            loader = loader.region(Region::new(region.to_owned()));
        }
        // The profile is what makes SSO work too: an SSO profile resolves through the same chain.
        if matches!(
            config.mode,
            AwsCredentialMode::Profile | AwsCredentialMode::Sso | AwsCredentialMode::AssumeRole
        ) && let Some(profile) = config.profile.as_deref().filter(|value| !value.is_empty())
        {
            loader = loader.profile_name(profile.to_owned());
        }
        let sdk = loader.load().await;

        if config.mode == AwsCredentialMode::AssumeRole {
            let role_arn = config
                .role_arn
                .as_deref()
                .filter(|value| !value.is_empty())
                .ok_or(CredentialError::Absent)?;
            let assumed = aws_sdk_sts::Client::new(&sdk)
                .assume_role()
                .role_arn(role_arn)
                .role_session_name(ROLE_SESSION_NAME)
                .duration_seconds(ASSUME_ROLE_DURATION_SECONDS)
                .send()
                .await
                .map_err(|_| CredentialError::Rejected)?
                .credentials
                .ok_or(CredentialError::Rejected)?;
            return AwsCredentials::new(
                assumed.access_key_id,
                assumed.secret_access_key,
                Some(assumed.session_token),
                Some(assumed.expiration.to_millis().unwrap_or(0)),
            )
            .map_err(|_| CredentialError::Unavailable);
        }

        let provider = sdk.credentials_provider().ok_or(CredentialError::Absent)?;
        // An expired SSO token surfaces here; it is a rejection the user resolves with `aws sso login`.
        let resolved = provider
            .provide_credentials()
            .await
            .map_err(|_| CredentialError::Rejected)?;
        AwsCredentials::new(
            resolved.access_key_id(),
            resolved.secret_access_key(),
            resolved.session_token().map(str::to_owned),
            resolved
                .expiry()
                .and_then(|expiry| expiry.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok()),
        )
        .map_err(|_| CredentialError::Unavailable)
    })
}

/// The seam the adapter actually holds.
pub(crate) struct BedrockAdapterCredentials {
    resolver: Arc<BedrockCredentialResolver>,
}

impl BedrockAdapterCredentials {
    pub(crate) const fn new(resolver: Arc<BedrockCredentialResolver>) -> Self {
        Self { resolver }
    }
}

impl AwsCredentialSource for BedrockAdapterCredentials {
    fn resolve(&mut self) -> Result<AwsCredentials, CredentialError> {
        let config = self.resolver.config();
        self.resolver.resolve_credentials(&config)
    }

    fn mark_rejected(&mut self) {
        let config = self.resolver.config();
        *self.resolver.rejection.lock().unwrap() = Some(config);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mode: AwsCredentialMode) -> BedrockCredentialConfig {
        BedrockCredentialConfig {
            mode,
            profile: Some("synthetic-profile".into()),
            region: Some("us-east-1".into()),
            role_arn: Some("arn:aws:iam::123456789012:role/synthetic".into()),
        }
    }

    #[test]
    fn each_mode_projects_its_own_secret_free_source_label() {
        for (mode, expected) in [
            (
                AwsCredentialMode::DefaultChain,
                CredentialSourceKind::AwsDefaultChain,
            ),
            (AwsCredentialMode::Profile, CredentialSourceKind::AwsProfile),
            (
                AwsCredentialMode::StaticKeychain,
                CredentialSourceKind::AwsStaticKeychain,
            ),
            (
                AwsCredentialMode::AssumeRole,
                CredentialSourceKind::AwsAssumedRole,
            ),
            (AwsCredentialMode::Sso, CredentialSourceKind::AwsSso),
        ] {
            assert_eq!(config(mode).source_kind(), expected);
        }
    }

    #[test]
    fn a_session_inside_the_refresh_window_is_expiring() {
        let now = unix_millis();
        assert!(is_expiring(now));
        assert!(is_expiring(now + EXPIRY_SKEW_MS / 2));
        assert!(!is_expiring(now + EXPIRY_SKEW_MS * 4));
    }

    #[test]
    fn the_resolver_reads_the_live_configuration_on_every_call() {
        let mode = Arc::new(Mutex::new(AwsCredentialMode::Profile));
        let observed = mode.clone();
        let resolver =
            BedrockCredentialResolver::new(Arc::new(move || config(*observed.lock().unwrap())));
        assert_eq!(resolver.config().mode, AwsCredentialMode::Profile);
        *mode.lock().unwrap() = AwsCredentialMode::AssumeRole;
        assert_eq!(resolver.config().mode, AwsCredentialMode::AssumeRole);
        assert_eq!(
            resolver.config().source_kind(),
            CredentialSourceKind::AwsAssumedRole
        );
    }

    #[test]
    fn static_keychain_mode_reports_absent_when_no_keypair_is_stored() {
        // The slots are empty in a test environment, so this exercises the absent path without
        // touching the chain or the network.
        let resolver = BedrockCredentialResolver::new(Arc::new(|| BedrockCredentialConfig {
            mode: AwsCredentialMode::StaticKeychain,
            profile: None,
            region: Some("us-east-1".into()),
            role_arn: None,
        }));
        assert!(matches!(
            resolver.state(),
            CredentialState::Absent | CredentialState::Error { .. }
        ));
    }

    #[test]
    fn a_rejection_sticks_to_its_configuration_and_clears_when_it_changes() {
        // The static-keychain mode keeps this off the AWS chain entirely: the slots are empty in a test
        // environment, so an unrejected resolution is deterministically `Absent`.
        let profile = Arc::new(Mutex::new(None::<String>));
        let observed = profile.clone();
        let resolver = BedrockCredentialResolver::new(Arc::new(move || BedrockCredentialConfig {
            mode: AwsCredentialMode::StaticKeychain,
            profile: observed.lock().unwrap().clone(),
            region: Some("us-east-1".into()),
            role_arn: None,
        }));
        assert_eq!(resolver.state(), CredentialState::Absent);

        BedrockAdapterCredentials::new(resolver.clone()).mark_rejected();
        assert_eq!(resolver.state(), CredentialState::Rejected);
        // Re-resolving must not silently report readiness again.
        assert_eq!(resolver.state(), CredentialState::Rejected);

        *profile.lock().unwrap() = Some("synthetic-other-profile".into());
        assert_eq!(resolver.state(), CredentialState::Absent);
    }
}
