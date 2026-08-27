//! AWS credential resolution for the Bedrock adapter.
//!
//! `corti-postprocess-providers` performs no ambient discovery, so every flavor of AWS credential is
//! resolved here, where the app already owns `aws-config` for the Transcribe backend. Only resolved,
//! short-lived material crosses into the adapter — never a profile path, an SSO cache file, or a role
//! session the adapter would have to manage.

use std::sync::{Arc, Condvar, Mutex};

use corti_postprocess::{CredentialSourceKind, CredentialState, ErrorCode};
use corti_postprocess_providers::{AwsCredentialSource, AwsCredentials, CredentialError};

use crate::{
    postprocess_config::{AwsCredentialMode, SecretPurpose},
    secret_store,
};

/// Refresh this far ahead of expiry so a session cannot lapse mid-stream.
const EXPIRY_SKEW_MS: i64 = 5 * 60 * 1000;
/// Assumed-role sessions are deliberately short; the resolver renews them rather than holding a long one.
/// Only the `aws` feature can call `sts:AssumeRole`.
#[cfg(feature = "aws")]
const ASSUME_ROLE_DURATION_SECONDS: i32 = 60 * 60;
#[cfg(feature = "aws")]
const ROLE_SESSION_NAME: &str = "corti-hosted-postprocess";
/// Ambient profile/SSO/IMDS discovery is never allowed to occupy a bounded preparation worker forever.
#[cfg(feature = "aws")]
const AWS_RESOLUTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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

/// Injected blocking resolver. Production is the only implementation allowed to inspect app-owned secret
/// slots or the AWS SDK chain; deterministic tests inject a counting backend and never touch either.
pub(crate) trait AwsResolverBackend: Send + Sync {
    fn resolve(&self, config: &BedrockCredentialConfig) -> Result<AwsCredentials, CredentialError>;
}

struct ProductionAwsResolverBackend;

impl AwsResolverBackend for ProductionAwsResolverBackend {
    fn resolve(&self, config: &BedrockCredentialConfig) -> Result<AwsCredentials, CredentialError> {
        match config.mode {
            AwsCredentialMode::StaticKeychain => stored_key_pair_credentials(),
            #[cfg(feature = "aws")]
            _ => chain_credentials(config),
            #[cfg(not(feature = "aws"))]
            _ => Err(CredentialError::Unavailable),
        }
    }
}

struct CredentialLease {
    config: BedrockCredentialConfig,
    generation: u64,
    credentials: AwsCredentials,
}

#[derive(Default)]
struct ResolverState {
    config: Option<BedrockCredentialConfig>,
    generation: u64,
    lease: Option<CredentialLease>,
    resolving: bool,
    rejection: Option<BedrockCredentialConfig>,
    last_error: Option<(BedrockCredentialConfig, CredentialError)>,
}

/// Shared resolution state. `state()` is a projection-only operation and can never create an AWS loader,
/// inspect a credential file, or probe IMDS. Resolution happens only when a Bedrock adapter/refresh asks for
/// credentials, and concurrent callers share one lease until expiry skew or explicit invalidation.
pub(crate) struct BedrockCredentialResolver {
    config: ConfigSource,
    backend: Arc<dyn AwsResolverBackend>,
    state: Mutex<ResolverState>,
    resolved: Condvar,
}

impl BedrockCredentialResolver {
    pub(crate) fn new(config: ConfigSource) -> Arc<Self> {
        Self::new_with_backend(config, Arc::new(ProductionAwsResolverBackend))
    }

    pub(crate) fn new_with_backend(
        config: ConfigSource,
        backend: Arc<dyn AwsResolverBackend>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            backend,
            state: Mutex::new(ResolverState::default()),
            resolved: Condvar::new(),
        })
    }

    pub(crate) fn config(&self) -> BedrockCredentialConfig {
        (self.config)()
    }

    /// Secret-free, nonblocking projection for Settings and startup. A lease is Ready only after an explicit
    /// on-demand resolution has succeeded; merely compiling or configuring Bedrock never samples ambient AWS.
    pub(crate) fn state(&self) -> CredentialState {
        let config = self.config();
        let state = self.state.lock().unwrap();
        if state.rejection.as_ref() == Some(&config) {
            return CredentialState::Rejected;
        }
        if state.resolving && state.config.as_ref() == Some(&config) {
            return CredentialState::Resolving;
        }
        if let Some(lease) = state
            .lease
            .as_ref()
            .filter(|lease| lease.config == config && lease.generation == state.generation)
        {
            let expires_at_unix_ms = lease.credentials.expires_at_unix_ms();
            return if expires_at_unix_ms.is_some_and(is_expiring) {
                CredentialState::Refreshing
            } else {
                CredentialState::Ready {
                    expires_at_unix_ms,
                    source: config.source_kind(),
                }
            };
        }
        match state
            .last_error
            .as_ref()
            .filter(|(failed_config, _)| failed_config == &config)
            .map(|(_, error)| *error)
        {
            Some(CredentialError::Rejected) => CredentialState::Rejected,
            Some(CredentialError::Unavailable) => CredentialState::Error {
                code: ErrorCode::AuthUnarmed,
            },
            Some(CredentialError::Absent) | None => CredentialState::Absent,
        }
    }

    pub(crate) fn resolve_state(&self) -> CredentialState {
        let config = self.config();
        match self.resolve_credentials(&config) {
            Ok(credentials) if credentials.expires_at_unix_ms().is_some_and(is_expiring) => {
                CredentialState::Refreshing
            }
            Ok(credentials) => CredentialState::Ready {
                expires_at_unix_ms: credentials.expires_at_unix_ms(),
                source: config.source_kind(),
            },
            Err(CredentialError::Absent) => CredentialState::Absent,
            Err(CredentialError::Rejected) => CredentialState::Rejected,
            Err(CredentialError::Unavailable) => CredentialState::Error {
                code: ErrorCode::AuthUnarmed,
            },
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state.lock().unwrap().generation
    }

    pub(crate) fn cache_identity(&self) -> Option<[u8; 32]> {
        let config = self.config();
        let state = self.state.lock().unwrap();
        state
            .lease
            .as_ref()
            .filter(|lease| lease.config == config && lease.generation == state.generation)
            .map(|lease| lease.credentials.cache_identity())
    }

    pub(crate) fn invalidate(&self) {
        let mut state = self.state.lock().unwrap();
        state.generation = state.generation.saturating_add(1);
        state.config = Some(self.config());
        state.lease = None;
        state.rejection = None;
        state.last_error = None;
        self.resolved.notify_all();
    }

    fn mark_rejected(&self) {
        let config = self.config();
        let mut state = self.state.lock().unwrap();
        state.generation = state.generation.saturating_add(1);
        state.config = Some(config.clone());
        state.lease = None;
        state.rejection = Some(config);
        state.last_error = None;
        self.resolved.notify_all();
    }

    fn resolve_credentials(
        &self,
        requested_config: &BedrockCredentialConfig,
    ) -> Result<AwsCredentials, CredentialError> {
        // One demand is permanently bound to the configuration it observed. If Settings invalidates that
        // configuration while an AWS loader is blocked, this caller becomes stale; it must never follow the
        // replacement into a second default-chain/IMDS attempt.
        let config = requested_config.clone();
        loop {
            if self.config() != config {
                return Err(CredentialError::Unavailable);
            }
            let mut state = self.state.lock().unwrap();
            if state.config.as_ref() != Some(&config) {
                state.generation = state.generation.saturating_add(1);
                state.config = Some(config.clone());
                state.lease = None;
                state.rejection = None;
                state.last_error = None;
            }
            if state.rejection.as_ref() == Some(&config) {
                return Err(CredentialError::Rejected);
            }
            if let Some(lease) = state
                .lease
                .as_ref()
                .filter(|lease| lease.config == config && lease.generation == state.generation)
            {
                if !lease
                    .credentials
                    .expires_at_unix_ms()
                    .is_some_and(is_expiring)
                {
                    return Ok(lease.credentials.clone());
                }
                // Renewal receives a fresh generation so catalog keys cannot reuse account data resolved
                // under the expiring lease.
                state.generation = state.generation.saturating_add(1);
                state.lease = None;
                state.last_error = None;
            }
            if state.resolving {
                state = self.resolved.wait(state).unwrap();
                drop(state);
                continue;
            }
            state.resolving = true;
            let generation = state.generation;
            drop(state);

            let result = self.backend.resolve(&config);
            let latest_config = self.config();
            let mut state = self.state.lock().unwrap();
            state.resolving = false;
            let still_current = latest_config == config
                && state.config.as_ref() == Some(&config)
                && state.generation == generation;
            if still_current {
                match &result {
                    Ok(credentials) => {
                        state.lease = Some(CredentialLease {
                            config: config.clone(),
                            generation,
                            credentials: credentials.clone(),
                        });
                        state.last_error = None;
                    }
                    Err(error) => {
                        state.lease = None;
                        state.last_error = Some((config.clone(), *error));
                    }
                }
            }
            self.resolved.notify_all();
            drop(state);
            if still_current {
                return result;
            }
            return Err(CredentialError::Unavailable);
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
fn stored_key_pair_credentials() -> Result<AwsCredentials, CredentialError> {
    use zeroize::{Zeroize as _, Zeroizing};

    fn slot(purpose: SecretPurpose) -> Result<Option<Zeroizing<String>>, CredentialError> {
        let Some(bytes) = secret_store::read(purpose).map_err(|_| CredentialError::Unavailable)?
        else {
            return Ok(None);
        };
        let value = String::from_utf8(bytes).map_err(|error| {
            let mut bytes = error.into_bytes();
            bytes.zeroize();
            CredentialError::Unavailable
        })?;
        Ok((!value.is_empty()).then(|| Zeroizing::new(value)))
    }

    let access_key_id = slot(SecretPurpose::AwsAccessKeyId)?.ok_or(CredentialError::Absent)?;
    let secret_access_key =
        slot(SecretPurpose::AwsSecretAccessKey)?.ok_or(CredentialError::Absent)?;
    let session_token = slot(SecretPurpose::AwsSessionToken)?;
    // The constructor owns its copies and zeroizes every validation error; these source buffers zeroize on
    // all earlier slot-loading failures as well.
    AwsCredentials::new(
        access_key_id.as_str().to_owned(),
        secret_access_key.as_str().to_owned(),
        session_token
            .as_ref()
            .map(|token| token.as_str().to_owned()),
        None,
    )
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
        tokio::time::timeout(AWS_RESOLUTION_TIMEOUT, async {
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
        .await
        .map_err(|_| CredentialError::Unavailable)?
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
        self.resolver.mark_rejected();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    struct CountingBackend {
        calls: AtomicUsize,
        delay: Duration,
    }

    impl CountingBackend {
        fn new(delay: Duration) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                delay,
            }
        }
    }

    impl AwsResolverBackend for CountingBackend {
        fn resolve(
            &self,
            _config: &BedrockCredentialConfig,
        ) -> Result<AwsCredentials, CredentialError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            AwsCredentials::new(
                "synthetic-access-id",
                "synthetic-secret",
                None,
                Some(unix_millis() + EXPIRY_SKEW_MS * 4),
            )
            .map_err(|_| CredentialError::Unavailable)
        }
    }

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
    fn state_never_invokes_the_injected_aws_backend() {
        let backend = Arc::new(CountingBackend::new(Duration::ZERO));
        let resolver = BedrockCredentialResolver::new_with_backend(
            Arc::new(|| config(AwsCredentialMode::DefaultChain)),
            backend.clone(),
        );
        assert_eq!(resolver.state(), CredentialState::Absent);
        assert_eq!(resolver.state(), CredentialState::Absent);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrent_resolution_single_flights_one_lease_per_generation() {
        let backend = Arc::new(CountingBackend::new(Duration::from_millis(30)));
        let resolver = BedrockCredentialResolver::new_with_backend(
            Arc::new(|| config(AwsCredentialMode::Profile)),
            backend.clone(),
        );
        let workers = (0..8)
            .map(|_| {
                let resolver = resolver.clone();
                std::thread::spawn(move || resolver.resolve_state())
            })
            .collect::<Vec<_>>();
        for worker in workers {
            assert!(matches!(
                worker.join().unwrap(),
                CredentialState::Ready { .. }
            ));
        }
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        let first_generation = resolver.generation();

        resolver.invalidate();
        assert!(resolver.generation() > first_generation);
        assert_eq!(resolver.state(), CredentialState::Absent);
        assert!(matches!(
            resolver.resolve_state(),
            CredentialState::Ready { .. }
        ));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_invalidated_slow_attempt_never_follows_the_replacement_configuration() {
        struct BlockingOnceBackend {
            calls: AtomicUsize,
            entered: mpsc::Sender<()>,
            release: Arc<(Mutex<bool>, Condvar)>,
        }

        impl AwsResolverBackend for BlockingOnceBackend {
            fn resolve(
                &self,
                _config: &BedrockCredentialConfig,
            ) -> Result<AwsCredentials, CredentialError> {
                let call = self.calls.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    let _ = self.entered.send(());
                    let (lock, ready) = &*self.release;
                    let mut released = lock.lock().unwrap();
                    while !*released {
                        released = ready.wait(released).unwrap();
                    }
                }
                AwsCredentials::new(
                    "synthetic-access-id",
                    "synthetic-secret",
                    None,
                    Some(unix_millis() + EXPIRY_SKEW_MS * 4),
                )
                .map_err(|_| CredentialError::Unavailable)
            }
        }

        let mode = Arc::new(Mutex::new(AwsCredentialMode::Profile));
        let observed = mode.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let backend = Arc::new(BlockingOnceBackend {
            calls: AtomicUsize::new(0),
            entered: entered_tx,
            release: release.clone(),
        });
        let resolver = BedrockCredentialResolver::new_with_backend(
            Arc::new(move || config(*observed.lock().unwrap())),
            backend.clone(),
        );
        let worker = {
            let resolver = resolver.clone();
            std::thread::spawn(move || resolver.resolve_state())
        };
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        *mode.lock().unwrap() = AwsCredentialMode::DefaultChain;
        resolver.invalidate();
        {
            let (lock, ready) = &*release;
            *lock.lock().unwrap() = true;
            ready.notify_all();
        }
        assert!(matches!(
            worker.join().unwrap(),
            CredentialState::Error {
                code: ErrorCode::AuthUnarmed
            }
        ));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(resolver.state(), CredentialState::Absent);
        assert!(matches!(
            resolver.resolve_state(),
            CredentialState::Ready { .. }
        ));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn static_keychain_mode_reports_absent_without_resolving() {
        // Projection does not inspect either app-owned slots or the ambient chain.
        let backend = Arc::new(CountingBackend::new(Duration::ZERO));
        let resolver = BedrockCredentialResolver::new_with_backend(
            Arc::new(|| BedrockCredentialConfig {
                mode: AwsCredentialMode::StaticKeychain,
                profile: None,
                region: Some("us-east-1".into()),
                role_arn: None,
            }),
            backend.clone(),
        );
        assert_eq!(resolver.state(), CredentialState::Absent);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn explicit_invalidation_clears_same_configuration_rejection_and_renews_once() {
        let backend = Arc::new(CountingBackend::new(Duration::ZERO));
        let resolver = BedrockCredentialResolver::new_with_backend(
            Arc::new(|| config(AwsCredentialMode::Profile)),
            backend.clone(),
        );
        assert!(matches!(
            resolver.resolve_state(),
            CredentialState::Ready { .. }
        ));
        BedrockAdapterCredentials::new(resolver.clone()).mark_rejected();
        assert_eq!(resolver.state(), CredentialState::Rejected);
        resolver.invalidate();
        assert_eq!(resolver.state(), CredentialState::Absent);
        assert!(matches!(
            resolver.resolve_state(),
            CredentialState::Ready { .. }
        ));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
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
