//! Non-secret hosted post-processing preferences.
//!
//! This document is deliberately separate from [`crate::config::AppConfig`]: hosted controls need their
//! own monotonic revision and must never become enabled as a side effect of saving transcription settings
//! or connecting a credential. Secret values are unrepresentable here; fixed [`SecretReference`] values
//! are lookup handles for the app-owned `secret_store` boundary.

// This phase intentionally lands the boundary before coordinator/Settings wiring.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use corti_postprocess::{
    ConnectionScopeId, LocalCacheMode, ModelId, ProviderCacheMode, ProviderId, TransportId,
};
use serde::{Deserialize, Serialize};

use crate::private_file::{atomic_write_private, read_private};

/// Schema 2 (v0.18) adds lane deadlines, the Vertex thinking policy, the lexicon switch, and question
/// subscriptions. A schema-1 document is migrated on load; an older binary refuses schema 2 and runs
/// with hosted egress off, which is the documented downgrade posture.
pub(crate) const HOSTED_PREFERENCES_SCHEMA: u32 = 2;
const LEGACY_HOSTED_PREFERENCES_SCHEMA: u32 = 1;
pub(crate) const EGRESS_DISCLOSURE_VERSION: u32 = 1;
pub(crate) const PINNED_AUTO_DISCLOSURE_VERSION: u32 = 1;
pub(crate) const PROVIDER_CACHE_DISCLOSURE_VERSION: u32 = 1;
const DEFAULT_FINAL_DEADLINE_SECONDS: u32 = 90;
const MAX_FINAL_DEADLINE_SECONDS: u32 = 10 * 60;
pub(crate) const DEFAULT_LIVE_FIRST_TEXT_SECONDS: u32 = 8;
pub(crate) const DEFAULT_LIVE_DEADLINE_SECONDS: u32 = 20;
pub(crate) const DEFAULT_QUESTION_DEADLINE_SECONDS: u32 = 45;
const MAX_LIVE_FIRST_TEXT_SECONDS: u32 = 60;
const MAX_LIVE_DEADLINE_SECONDS: u32 = 120;
const MAX_QUESTION_DEADLINE_SECONDS: u32 = 300;
const MAX_SUBSCRIPTIONS: usize = 16;
const MAX_SUBSCRIPTION_TEMPLATE_BYTES: usize = 32 * 1024;
/// The subscription the schema-1 pinned template migrates into.
pub(crate) const MIGRATED_PINNED_SUBSCRIPTION_ID: &str = "pinned";
const MAX_HOSTED_PREFERENCES_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_VERTEX_MODELS: usize = 32;

/// The host facility that owns a secret. No secret bytes, path, account id, or user-supplied label can be
/// represented by this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SecretBackend {
    MacosNonSynchronizingGenericPassword,
}

/// Fixed app-owned secret slots. `secret_store` maps each handle to one private file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // Explicit Key suffixes keep fixed secret slots unambiguous.
pub(crate) enum SecretPurpose {
    OpenAiApiKey,
    ChatGptSubscriptionCredential,
    AnthropicApiKey,
    PostprocessCacheMasterKey,
    AwsAccessKeyId,
    AwsSecretAccessKey,
    AwsSessionToken,
}

impl SecretPurpose {
    /// Fixed file name for this slot in the private secret store. Values are versioned so a format
    /// change can never
    /// silently reinterpret an existing item.
    pub(crate) const fn slot_name(self) -> &'static str {
        match self {
            Self::OpenAiApiKey => "openai-api-key-v1",
            Self::ChatGptSubscriptionCredential => "chatgpt-subscription-credential-v1",
            Self::AnthropicApiKey => "anthropic-api-key-v1",
            Self::PostprocessCacheMasterKey => "encrypted-store-master-v1",
            Self::AwsAccessKeyId => "aws-access-key-id-v1",
            Self::AwsSecretAccessKey => "aws-secret-access-key-v1",
            Self::AwsSessionToken => "aws-session-token-v1",
        }
    }
}

/// How AWS credentials are resolved for Bedrock. This is a non-secret preference; the values it selects
/// live in `~/.aws` or Corti's private secret store, never in this document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AwsCredentialMode {
    #[default]
    DefaultChain,
    Profile,
    StaticKeychain,
    AssumeRole,
    Sso,
}

impl AwsCredentialMode {
    pub(crate) const fn requires_profile(self) -> bool {
        matches!(self, Self::Profile | Self::Sso)
    }

    pub(crate) const fn requires_role_arn(self) -> bool {
        matches!(self, Self::AssumeRole)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretReference {
    backend: SecretBackend,
    purpose: SecretPurpose,
}

impl SecretReference {
    pub(crate) const fn openai_api_key() -> Self {
        Self::keychain(SecretPurpose::OpenAiApiKey)
    }

    pub(crate) const fn anthropic_api_key() -> Self {
        Self::keychain(SecretPurpose::AnthropicApiKey)
    }

    pub(crate) const fn cache_master_key() -> Self {
        Self::keychain(SecretPurpose::PostprocessCacheMasterKey)
    }

    pub(crate) const fn aws_access_key_id() -> Self {
        Self::keychain(SecretPurpose::AwsAccessKeyId)
    }

    pub(crate) const fn aws_secret_access_key() -> Self {
        Self::keychain(SecretPurpose::AwsSecretAccessKey)
    }

    pub(crate) const fn aws_session_token() -> Self {
        Self::keychain(SecretPurpose::AwsSessionToken)
    }

    const fn keychain(purpose: SecretPurpose) -> Self {
        Self {
            backend: SecretBackend::MacosNonSynchronizingGenericPassword,
            purpose,
        }
    }

    pub(crate) const fn purpose(self) -> SecretPurpose {
        self.purpose
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProviderScopePreferences {
    /// Opaque Corti-local scope identity; never a provider account id.
    pub(crate) connection_scope_id: Option<ConnectionScopeId>,
    /// Human-selected non-secret connection label.
    pub(crate) alias: Option<String>,
    /// Vertex configuration values are intentionally non-secret. They are not telemetry fields.
    pub(crate) project: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) quota_project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DirectProviderPreferences {
    pub(crate) scope: ProviderScopePreferences,
    pub(crate) credential: SecretReference,
    /// Provider-side retention/caching remains independently disclosed and off by default.
    pub(crate) provider_cache_acknowledgement_version: Option<u32>,
}

impl DirectProviderPreferences {
    fn openai() -> Self {
        Self {
            scope: ProviderScopePreferences::default(),
            credential: SecretReference::openai_api_key(),
            provider_cache_acknowledgement_version: None,
        }
    }

    fn anthropic() -> Self {
        Self {
            scope: ProviderScopePreferences::default(),
            credential: SecretReference::anthropic_api_key(),
            provider_cache_acknowledgement_version: None,
        }
    }
}

/// Bedrock's non-secret connection facts. The credential *mode*, the `~/.aws` profile name, the region,
/// and the role ARN are all configuration; no key material is representable here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct BedrockProviderPreferences {
    pub(crate) scope: ProviderScopePreferences,
    pub(crate) credential_mode: AwsCredentialMode,
    /// A `~/.aws/config` or `~/.aws/credentials` profile name. Used by the profile and SSO modes.
    pub(crate) profile: Option<String>,
    /// The role assumed after the base chain resolves, for the assume-role mode.
    pub(crate) role_arn: Option<String>,
    pub(crate) provider_cache_acknowledgement_version: Option<u32>,
}

impl Default for BedrockProviderPreferences {
    fn default() -> Self {
        Self {
            scope: ProviderScopePreferences::default(),
            credential_mode: AwsCredentialMode::DefaultChain,
            profile: None,
            role_arn: None,
            provider_cache_acknowledgement_version: None,
        }
    }
}

/// Whether the Vertex adapter sends its per-model thinking control (`Auto`) or omits it (`Omit`), the
/// escape hatch when a model rejects the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThinkingPreference {
    #[default]
    Auto,
    Omit,
}

/// One saved question subscription (schema 2). Enum-like fields are plain strings here so the document
/// stays readable and the coordinator, not the config module, decides what each preset means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct QuestionSubscriptionPreferences {
    /// Stable id: `[a-z0-9-]{1,32}`; rides `target_id` on every call the subscription makes.
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) template: String,
    pub(crate) enabled: bool,
    /// `none` | `asked_of_me` | `running_summary` | `topic_watch`.
    pub(crate) preset: String,
    /// `paragraph` | `bullets` | `json_questions`.
    pub(crate) output: String,
    pub(crate) trigger: SubscriptionTriggerPreferences,
    pub(crate) context: SubscriptionContextPreferences,
    /// Names the owner answers to, for the `asked_of_me` preset's cheap pre-filter.
    pub(crate) name_hints: Vec<String>,
}

impl Default for QuestionSubscriptionPreferences {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            template: String::new(),
            enabled: false,
            preset: "none".to_owned(),
            output: "paragraph".to_owned(),
            trigger: SubscriptionTriggerPreferences::default(),
            context: SubscriptionContextPreferences::default(),
            name_hints: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SubscriptionTriggerPreferences {
    pub(crate) quiet_ms: u64,
    pub(crate) min_new_words: u64,
    pub(crate) min_new_speech_ms: u64,
    pub(crate) min_interval_ms: u64,
    /// `all` | `them` | `me`.
    pub(crate) on_speakers: String,
}

impl Default for SubscriptionTriggerPreferences {
    fn default() -> Self {
        Self {
            quiet_ms: 750,
            min_new_words: 40,
            min_new_speech_ms: 30_000,
            min_interval_ms: 0,
            on_speakers: "all".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SubscriptionContextPreferences {
    /// `whole` | `last_minutes` | `last_rows`.
    pub(crate) window: String,
    pub(crate) minutes: u32,
    pub(crate) rows: u32,
}

impl Default for SubscriptionContextPreferences {
    fn default() -> Self {
        Self {
            window: "whole".to_owned(),
            minutes: 0,
            rows: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ProviderPreferences {
    pub(crate) vertex: ProviderScopePreferences,
    /// Exact Vertex model ids typed by the operator. Vertex publishes no per-project listing of the models
    /// a caller may invoke, so this is the only way to reach a model Corti does not already know about.
    pub(crate) vertex_models: Vec<String>,
    pub(crate) vertex_provider_cache_acknowledgement_version: Option<u32>,
    /// Escape hatch for the Gemini thinking control (schema 2).
    pub(crate) vertex_thinking: ThinkingPreference,
    pub(crate) openai: DirectProviderPreferences,
    pub(crate) anthropic: DirectProviderPreferences,
    pub(crate) bedrock: BedrockProviderPreferences,
    /// Read-only migration sink for schema-v1 files written before the app-server proposal was removed.
    /// New saves omit it; no runtime behavior consumes the legacy approval bit.
    #[serde(rename = "codex_experimental_approved", skip_serializing)]
    legacy_codex_experimental_approved: bool,
}

impl Default for ProviderPreferences {
    fn default() -> Self {
        Self {
            vertex: ProviderScopePreferences::default(),
            vertex_models: Vec::new(),
            vertex_provider_cache_acknowledgement_version: None,
            vertex_thinking: ThinkingPreference::Auto,
            openai: DirectProviderPreferences::openai(),
            anthropic: DirectProviderPreferences::anthropic(),
            bedrock: BedrockProviderPreferences::default(),
            legacy_codex_experimental_approved: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LanePreferences {
    pub(crate) enabled: bool,
    pub(crate) provider: Option<ProviderId>,
    pub(crate) transport: Option<TransportId>,
    pub(crate) model: Option<ModelId>,
    pub(crate) local_cache: LocalCacheMode,
    pub(crate) provider_cache: ProviderCacheMode,
}

impl Default for LanePreferences {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: None,
            transport: None,
            model: None,
            local_cache: LocalCacheMode::Reusable,
            provider_cache: ProviderCacheMode::Off,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct HostedPreferenceValues {
    pub(crate) master_enabled: bool,
    pub(crate) live: LanePreferences,
    pub(crate) final_lane: LanePreferences,
    pub(crate) questions: LanePreferences,
    pub(crate) providers: ProviderPreferences,
    pub(crate) default_steering: String,
    pub(crate) pinned_question_template: String,
    pub(crate) pinned_auto_enabled: bool,
    pub(crate) pinned_auto_acknowledgement_version: Option<u32>,
    pub(crate) final_deadline_seconds: u32,
    pub(crate) egress_acknowledgement_version: Option<u32>,
    pub(crate) show_history_diagnostics: bool,
    pub(crate) show_live_metrics_by_default: bool,
    // ----- schema 2 -----
    /// Live first-text deadline, measured from the response headers.
    pub(crate) live_first_text_seconds: u32,
    /// Live terminal deadline from dispatch.
    pub(crate) live_deadline_seconds: u32,
    /// Question terminal deadline from enqueue.
    pub(crate) question_deadline_seconds: u32,
    /// Whether the learned correction lexicon participates in cleanup and prompts.
    pub(crate) lexicon_enabled: bool,
    /// Question subscriptions; the schema-1 pinned template migrates into the first one.
    pub(crate) subscriptions: Vec<QuestionSubscriptionPreferences>,
    /// Acknowledgement that automatic subscriptions make repeated paid calls.
    pub(crate) subscriptions_auto_acknowledgement_version: Option<u32>,
}

impl Default for HostedPreferenceValues {
    fn default() -> Self {
        Self {
            master_enabled: false,
            live: LanePreferences::default(),
            final_lane: LanePreferences::default(),
            questions: LanePreferences::default(),
            providers: ProviderPreferences::default(),
            default_steering: String::new(),
            pinned_question_template: String::new(),
            pinned_auto_enabled: false,
            pinned_auto_acknowledgement_version: None,
            final_deadline_seconds: DEFAULT_FINAL_DEADLINE_SECONDS,
            egress_acknowledgement_version: None,
            show_history_diagnostics: false,
            show_live_metrics_by_default: false,
            live_first_text_seconds: DEFAULT_LIVE_FIRST_TEXT_SECONDS,
            live_deadline_seconds: DEFAULT_LIVE_DEADLINE_SECONDS,
            question_deadline_seconds: DEFAULT_QUESTION_DEADLINE_SECONDS,
            lexicon_enabled: true,
            subscriptions: Vec::new(),
            subscriptions_auto_acknowledgement_version: None,
        }
    }
}

impl HostedPreferenceValues {
    /// Whether the owner acknowledged provider-side caching for `provider` (`openai`, `anthropic`,
    /// `google`, `amazon`). Experimental transports have no controllable policy and report `false`.
    pub(crate) fn provider_cache_acknowledged(&self, provider: &str) -> bool {
        let version = match provider {
            "openai" => self.providers.openai.provider_cache_acknowledgement_version,
            "anthropic" => {
                self.providers
                    .anthropic
                    .provider_cache_acknowledgement_version
            }
            "google" => self.providers.vertex_provider_cache_acknowledgement_version,
            "amazon" => {
                self.providers
                    .bedrock
                    .provider_cache_acknowledgement_version
            }
            _ => None,
        };
        version == Some(PROVIDER_CACHE_DISCLOSURE_VERSION)
    }

    /// Record or withdraw the provider-side caching acknowledgement. Returns `false` for a provider that
    /// has no such control.
    pub(crate) fn set_provider_cache_acknowledged(
        &mut self,
        provider: &str,
        acknowledged: bool,
    ) -> bool {
        let value = acknowledged.then_some(PROVIDER_CACHE_DISCLOSURE_VERSION);
        match provider {
            "openai" => self.providers.openai.provider_cache_acknowledgement_version = value,
            "anthropic" => {
                self.providers
                    .anthropic
                    .provider_cache_acknowledgement_version = value
            }
            "google" => self.providers.vertex_provider_cache_acknowledgement_version = value,
            "amazon" => {
                self.providers
                    .bedrock
                    .provider_cache_acknowledgement_version = value
            }
            _ => return false,
        }
        true
    }
}

/// Separately revisioned hosted preferences document. Fields are nested under `preferences` so schema and
/// revision remain unmistakably document metadata rather than request controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct HostedPreferences {
    schema: u32,
    revision: u64,
    preferences: HostedPreferenceValues,
}

impl Default for HostedPreferences {
    fn default() -> Self {
        Self {
            schema: HOSTED_PREFERENCES_SCHEMA,
            revision: 0,
            preferences: HostedPreferenceValues::default(),
        }
    }
}

impl HostedPreferences {
    pub(crate) const fn schema(&self) -> u32 {
        self.schema
    }

    pub(crate) const fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) const fn values(&self) -> &HostedPreferenceValues {
        &self.preferences
    }

    /// Replace the non-secret values and increment only this document's revision. A semantic no-op keeps
    /// the old revision, which makes later patch conflict checks deterministic.
    pub(crate) fn revise(&self, update: impl FnOnce(&mut HostedPreferenceValues)) -> Result<Self> {
        let mut preferences = self.preferences.clone();
        update(&mut preferences);
        if preferences == self.preferences {
            return Ok(self.clone());
        }
        let revision = self
            .revision
            .checked_add(1)
            .context("hosted preferences revision overflow")?;
        let revised = Self {
            schema: HOSTED_PREFERENCES_SCHEMA,
            revision,
            preferences,
        };
        revised.validate()?;
        Ok(revised)
    }

    pub(crate) fn load() -> Result<Self> {
        Self::load_at(&hosted_preferences_path()?)
    }

    pub(crate) fn save(&self) -> Result<()> {
        self.save_at(&hosted_preferences_path()?)
    }

    fn load_at(path: &Path) -> Result<Self> {
        let Some(bytes) = read_private(path, "hosted preferences", MAX_HOSTED_PREFERENCES_BYTES)?
        else {
            return Ok(Self::default());
        };
        let text = std::str::from_utf8(&bytes)
            .with_context(|| format!("decoding hosted preferences {}", path.display()))?;
        let mut document: Self = toml::from_str(text)
            .with_context(|| format!("parsing hosted preferences {}", path.display()))?;
        if document.schema == LEGACY_HOSTED_PREFERENCES_SCHEMA {
            document = document.migrate_v1_to_v2();
        }
        document.validate()?;
        Ok(document)
    }

    /// Schema 1 → 2: every new field already carries its default from `serde(default)`; the one semantic
    /// move is the single pinned template becoming the first question subscription. The `pinned_*`
    /// fields stay authoritative until the subscription coordinator replaces them, so both are kept in
    /// step here rather than one being dropped.
    fn migrate_v1_to_v2(mut self) -> Self {
        self.schema = HOSTED_PREFERENCES_SCHEMA;
        let values = &mut self.preferences;
        let template = values.pinned_question_template.trim();
        if !template.is_empty()
            && !values
                .subscriptions
                .iter()
                .any(|subscription| subscription.id == MIGRATED_PINNED_SUBSCRIPTION_ID)
        {
            values.subscriptions.insert(
                0,
                QuestionSubscriptionPreferences {
                    id: MIGRATED_PINNED_SUBSCRIPTION_ID.to_owned(),
                    title: "Pinned question".to_owned(),
                    template: template.to_owned(),
                    enabled: values.pinned_auto_enabled,
                    ..QuestionSubscriptionPreferences::default()
                },
            );
        }
        if values.subscriptions_auto_acknowledgement_version.is_none() {
            values.subscriptions_auto_acknowledgement_version =
                values.pinned_auto_acknowledgement_version;
        }
        self
    }

    fn save_at(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let body = toml::to_string_pretty(self).context("serializing hosted preferences")?;
        atomic_write_private(path, body.as_bytes(), "hosted preferences")
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == HOSTED_PREFERENCES_SCHEMA,
            "unsupported hosted preferences schema {} (expected {})",
            self.schema,
            HOSTED_PREFERENCES_SCHEMA
        );
        ensure!(
            (1..=MAX_FINAL_DEADLINE_SECONDS).contains(&self.preferences.final_deadline_seconds),
            "hosted final deadline must be between 1 and {MAX_FINAL_DEADLINE_SECONDS} seconds"
        );
        ensure!(
            (1..=MAX_LIVE_FIRST_TEXT_SECONDS).contains(&self.preferences.live_first_text_seconds),
            "hosted live first-text deadline must be between 1 and {MAX_LIVE_FIRST_TEXT_SECONDS} seconds"
        );
        ensure!(
            (2..=MAX_LIVE_DEADLINE_SECONDS).contains(&self.preferences.live_deadline_seconds),
            "hosted live deadline must be between 2 and {MAX_LIVE_DEADLINE_SECONDS} seconds"
        );
        ensure!(
            (5..=MAX_QUESTION_DEADLINE_SECONDS)
                .contains(&self.preferences.question_deadline_seconds),
            "hosted question deadline must be between 5 and {MAX_QUESTION_DEADLINE_SECONDS} seconds"
        );
        validate_subscriptions(&self.preferences.subscriptions)?;
        if self.preferences.master_enabled {
            ensure!(
                self.preferences.egress_acknowledgement_version == Some(EGRESS_DISCLOSURE_VERSION),
                "hosted egress cannot be enabled without the current disclosure acknowledgement"
            );
        }
        if self.preferences.pinned_auto_enabled {
            ensure!(
                self.preferences.pinned_auto_acknowledgement_version
                    == Some(PINNED_AUTO_DISCLOSURE_VERSION),
                "automatic pinned questions require the repeated-call acknowledgement"
            );
        }
        validate_direct_provider(
            &self.preferences.providers.openai,
            SecretPurpose::OpenAiApiKey,
        )?;
        validate_direct_provider(
            &self.preferences.providers.anthropic,
            SecretPurpose::AnthropicApiKey,
        )?;
        validate_bedrock_provider(&self.preferences.providers.bedrock)?;
        validate_vertex_models(&self.preferences.providers.vertex_models)?;
        for lane in [
            &self.preferences.live,
            &self.preferences.final_lane,
            &self.preferences.questions,
        ] {
            if matches!(
                lane.provider_cache,
                ProviderCacheMode::ExplicitStablePrefix | ProviderCacheMode::UnavoidableImplicit
            ) {
                let acknowledged = match lane.provider.as_ref().map(ProviderId::as_str) {
                    Some("openai") => {
                        self.preferences
                            .providers
                            .openai
                            .provider_cache_acknowledgement_version
                    }
                    Some("anthropic") => {
                        self.preferences
                            .providers
                            .anthropic
                            .provider_cache_acknowledgement_version
                    }
                    Some("google") => {
                        self.preferences
                            .providers
                            .vertex_provider_cache_acknowledgement_version
                    }
                    Some("amazon") => {
                        self.preferences
                            .providers
                            .bedrock
                            .provider_cache_acknowledgement_version
                    }
                    // Experimental transports have no controllable provider-cache policy.
                    _ => None,
                };
                ensure!(
                    acknowledged == Some(PROVIDER_CACHE_DISCLOSURE_VERSION),
                    "provider caching requires the current provider-retention acknowledgement"
                );
            }
        }
        Ok(())
    }
}

fn validate_direct_provider(
    provider: &DirectProviderPreferences,
    expected: SecretPurpose,
) -> Result<()> {
    if provider.credential.purpose() != expected {
        bail!("hosted credential reference does not match its provider slot");
    }
    Ok(())
}

fn validate_subscriptions(subscriptions: &[QuestionSubscriptionPreferences]) -> Result<()> {
    ensure!(
        subscriptions.len() <= MAX_SUBSCRIPTIONS,
        "at most {MAX_SUBSCRIPTIONS} question subscriptions can be saved"
    );
    let mut seen = std::collections::HashSet::new();
    for subscription in subscriptions {
        ensure!(
            !subscription.id.is_empty()
                && subscription.id.len() <= 32
                && subscription
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'),
            "{:?} is not a valid subscription id (a-z, 0-9, '-', at most 32)",
            subscription.id
        );
        ensure!(
            seen.insert(subscription.id.as_str()),
            "duplicate subscription id {:?}",
            subscription.id
        );
        ensure!(
            subscription.title.len() <= 128 && !subscription.title.chars().any(char::is_control),
            "subscription {:?} has an invalid title",
            subscription.id
        );
        ensure!(
            subscription.template.len() <= MAX_SUBSCRIPTION_TEMPLATE_BYTES
                && !subscription
                    .template
                    .chars()
                    .any(|ch| ch.is_control() && ch != '\n'),
            "subscription {:?} has an invalid template",
            subscription.id
        );
        ensure!(
            matches!(
                subscription.preset.as_str(),
                "none" | "asked_of_me" | "running_summary" | "topic_watch"
            ),
            "subscription {:?} has an unknown preset {:?}",
            subscription.id,
            subscription.preset
        );
        ensure!(
            matches!(
                subscription.output.as_str(),
                "paragraph" | "bullets" | "json_questions"
            ),
            "subscription {:?} has an unknown output format {:?}",
            subscription.id,
            subscription.output
        );
        ensure!(
            matches!(
                subscription.trigger.on_speakers.as_str(),
                "all" | "them" | "me"
            ),
            "subscription {:?} has an unknown speaker filter",
            subscription.id
        );
        ensure!(
            matches!(
                subscription.context.window.as_str(),
                "whole" | "last_minutes" | "last_rows"
            ),
            "subscription {:?} has an unknown context window",
            subscription.id
        );
        ensure!(
            subscription.name_hints.len() <= 16
                && subscription
                    .name_hints
                    .iter()
                    .all(|hint| !hint.trim().is_empty() && hint.len() <= 64),
            "subscription {:?} has invalid name hints",
            subscription.id
        );
    }
    Ok(())
}

/// Typed model ids are configuration, not free text: the same character set the Vertex adapter accepts,
/// so a saved entry can never be one the adapter would refuse to build a catalog from.
fn validate_vertex_models(models: &[String]) -> Result<()> {
    ensure!(
        models.len() <= MAX_VERTEX_MODELS,
        "at most {MAX_VERTEX_MODELS} Vertex models can be pinned"
    );
    let mut seen = std::collections::HashSet::new();
    for model in models {
        ensure!(
            !model.is_empty()
                && model.len() <= 256
                && model.bytes().all(|byte| byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'_' | b'.' | b'@')),
            "{model:?} is not a valid Vertex model id"
        );
        ensure!(
            seen.insert(model.as_str()),
            "duplicate Vertex model {model:?}"
        );
    }
    Ok(())
}

/// A Bedrock mode is only savable once its own non-secret companion field is present, so the pane can
/// never persist a connection that is guaranteed to fail at resolve time.
fn validate_bedrock_provider(provider: &BedrockProviderPreferences) -> Result<()> {
    let profile = provider
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let role_arn = provider
        .role_arn
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    ensure!(
        !provider.credential_mode.requires_profile() || profile.is_some(),
        "the {:?} AWS credential mode requires a profile name",
        provider.credential_mode
    );
    ensure!(
        !provider.credential_mode.requires_role_arn() || role_arn.is_some(),
        "assuming a role requires its ARN"
    );
    ensure!(
        role_arn.is_none_or(|arn| arn.starts_with("arn:") && arn.contains(":role/")),
        "the AWS role ARN is not an IAM role ARN"
    );
    ensure!(
        profile.is_none_or(|profile| profile.len() <= 256 && !profile.contains(['[', ']', '\n'])),
        "the AWS profile name is not a valid profile name"
    );
    // The connection is regional, and Bedrock model availability differs per region.
    ensure!(
        provider.scope.connection_scope_id.is_none()
            || provider
                .scope
                .region
                .as_deref()
                .is_some_and(|region| !region.trim().is_empty()),
        "a configured Bedrock connection requires a region"
    );
    ensure!(
        provider.scope.project.is_none() && provider.scope.quota_project.is_none(),
        "Bedrock has no project or quota-project scope"
    );
    Ok(())
}

pub(crate) fn hosted_preferences_path() -> Result<PathBuf> {
    Ok(corti_queue::data_dir()?.join("hosted.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn test_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "corti-hosted-preferences-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("hosted.toml")
    }

    #[test]
    fn defaults_are_off_and_independently_revisioned() {
        let preferences = HostedPreferences::default();
        assert_eq!(preferences.schema(), HOSTED_PREFERENCES_SCHEMA);
        assert_eq!(preferences.revision(), 0);
        assert!(!preferences.values().master_enabled);
        assert!(!preferences.values().live.enabled);
        assert!(!preferences.values().final_lane.enabled);
        assert!(!preferences.values().questions.enabled);
        assert!(!preferences.values().pinned_auto_enabled);
        assert!(
            !preferences
                .values()
                .providers
                .legacy_codex_experimental_approved
        );

        let unchanged = preferences.revise(|_| {}).unwrap();
        assert_eq!(unchanged.revision(), 0);
        let revised = preferences
            .revise(|values| values.show_history_diagnostics = true)
            .unwrap();
        assert_eq!(revised.revision(), 1);
        assert_eq!(AppConfigBoundary::FILE_NAME, "config.toml");
        assert_eq!(HOSTED_FILE_NAME, "hosted.toml");
    }

    // Tiny constants make the separate-file assertion explicit without loading process-global paths.
    struct AppConfigBoundary;
    impl AppConfigBoundary {
        const FILE_NAME: &'static str = "config.toml";
    }
    const HOSTED_FILE_NAME: &str = "hosted.toml";

    #[test]
    fn enable_requires_persisted_egress_acknowledgement() {
        let preferences = HostedPreferences::default();
        let error = preferences
            .revise(|values| values.master_enabled = true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("disclosure acknowledgement"), "{error}");

        let enabled = preferences
            .revise(|values| {
                values.egress_acknowledgement_version = Some(EGRESS_DISCLOSURE_VERSION);
                values.master_enabled = true;
            })
            .unwrap();
        assert!(enabled.values().master_enabled);
    }

    #[test]
    fn private_round_trip_contains_references_but_no_secret_values() {
        let path = test_path("round-trip");
        let preferences = HostedPreferences::default()
            .revise(|values| {
                values.providers.openai.scope.connection_scope_id =
                    Some(ConnectionScopeId::new("scope-fixture").unwrap());
                values.providers.openai.scope.alias = Some("Fixture connection".into());
            })
            .unwrap();
        preferences.save_at(&path).unwrap();

        assert_eq!(HostedPreferences::load_at(&path).unwrap(), preferences);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("macos_non_synchronizing_generic_password"));
        assert!(text.contains("open_ai_api_key"));
        for forbidden in [
            "synthetic-secret-value",
            "bearer ",
            "sk-",
            "access_token",
            "refresh_token",
        ] {
            assert!(!text.to_ascii_lowercase().contains(forbidden));
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn bedrock_round_trip_persists_mode_profile_region_and_role_but_no_key_material() {
        let path = test_path("bedrock-round-trip");
        let preferences = HostedPreferences::default()
            .revise(|values| {
                let bedrock = &mut values.providers.bedrock;
                bedrock.credential_mode = AwsCredentialMode::AssumeRole;
                bedrock.profile = Some("synthetic-profile".into());
                bedrock.role_arn = Some("arn:aws:iam::123456789012:role/synthetic-role".into());
                bedrock.scope.connection_scope_id =
                    Some(ConnectionScopeId::new("bedrock-scope-fixture").unwrap());
                bedrock.scope.alias = Some("Fixture Bedrock".into());
                bedrock.scope.region = Some("us-east-1".into());
            })
            .unwrap();
        preferences.save_at(&path).unwrap();
        assert_eq!(HostedPreferences::load_at(&path).unwrap(), preferences);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("assume_role"));
        assert!(text.contains("synthetic-profile"));
        assert!(text.contains("us-east-1"));
        assert!(text.contains("role/synthetic-role"));
        let lowered = text.to_ascii_lowercase();
        for forbidden in [
            "akia",
            "asia",
            "aws_secret_access_key =",
            "session_token =",
            "aws_access_key_id =",
        ] {
            assert!(!lowered.contains(forbidden), "{forbidden} leaked");
        }
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn legacy_configured_bedrock_without_a_setup_name_remains_readable() {
        let path = test_path("bedrock-legacy-missing-name");
        let preferences = HostedPreferences::default()
            .revise(|values| {
                let bedrock = &mut values.providers.bedrock;
                bedrock.scope.connection_scope_id =
                    Some(ConnectionScopeId::new("legacy-bedrock-scope").unwrap());
                bedrock.scope.region = Some("us-east-1".into());
                bedrock.scope.alias = None;
            })
            .unwrap();
        preferences.save_at(&path).unwrap();
        let loaded = HostedPreferences::load_at(&path).unwrap();
        assert_eq!(
            loaded
                .values()
                .providers
                .bedrock
                .scope
                .connection_scope_id
                .as_ref()
                .unwrap()
                .as_str(),
            "legacy-bedrock-scope"
        );
        assert!(loaded.values().providers.bedrock.scope.alias.is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn bedrock_modes_require_their_own_non_secret_companion_field() {
        let base = HostedPreferences::default();
        let error = base
            .revise(|values| values.providers.bedrock.credential_mode = AwsCredentialMode::Profile)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires a profile name"), "{error}");

        let error = base
            .revise(|values| {
                values.providers.bedrock.credential_mode = AwsCredentialMode::AssumeRole;
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires its ARN"), "{error}");

        let error = base
            .revise(|values| {
                values.providers.bedrock.credential_mode = AwsCredentialMode::AssumeRole;
                values.providers.bedrock.role_arn = Some("not-an-arn".into());
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("not an IAM role ARN"), "{error}");

        // The default chain needs nothing beyond a region once the connection is configured.
        let ok = base
            .revise(|values| {
                values.providers.bedrock.scope.connection_scope_id =
                    Some(ConnectionScopeId::new("bedrock-scope-fixture").unwrap());
                values.providers.bedrock.scope.region = Some("eu-central-1".into());
            })
            .unwrap();
        assert_eq!(
            ok.values().providers.bedrock.credential_mode,
            AwsCredentialMode::DefaultChain
        );

        let error = base
            .revise(|values| {
                values.providers.bedrock.scope.connection_scope_id =
                    Some(ConnectionScopeId::new("bedrock-scope-fixture").unwrap());
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires a region"), "{error}");
    }

    #[test]
    fn typed_vertex_models_round_trip_and_reject_anything_the_adapter_would_refuse() {
        let path = test_path("vertex-models-round-trip");
        let preferences = HostedPreferences::default()
            .revise(|values| {
                values.providers.vertex_models = vec![
                    "gemini-2.5-flash-lite".into(),
                    "claude-sonnet-4-5@20250929".into(),
                ];
            })
            .unwrap();
        preferences.save_at(&path).unwrap();
        assert_eq!(HostedPreferences::load_at(&path).unwrap(), preferences);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();

        // A malformed or repeated id would disarm the whole Vertex catalog, not just its own entry.
        for models in [
            vec!["gemini 2.5 flash".to_string()],
            vec![String::new()],
            vec!["gemini-2.5-pro".to_string(), "gemini-2.5-pro".to_string()],
        ] {
            assert!(
                HostedPreferences::default()
                    .revise(|values| values.providers.vertex_models = models.clone())
                    .is_err(),
                "{models:?} was accepted"
            );
        }

        let too_many = (0..=MAX_VERTEX_MODELS)
            .map(|index| format!("gemini-fixture-{index}"))
            .collect::<Vec<_>>();
        assert!(
            HostedPreferences::default()
                .revise(|values| values.providers.vertex_models = too_many)
                .is_err()
        );
    }

    #[test]
    fn aws_keychain_slots_are_fixed_and_distinct() {
        let slots = [
            SecretPurpose::AwsAccessKeyId,
            SecretPurpose::AwsSecretAccessKey,
            SecretPurpose::AwsSessionToken,
            SecretPurpose::OpenAiApiKey,
            SecretPurpose::ChatGptSubscriptionCredential,
            SecretPurpose::AnthropicApiKey,
            SecretPurpose::PostprocessCacheMasterKey,
        ];
        let accounts = slots
            .iter()
            .map(|slot| slot.slot_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(accounts.len(), slots.len());
        assert_eq!(
            SecretReference::aws_secret_access_key().purpose(),
            SecretPurpose::AwsSecretAccessKey
        );
        // The slot names must not move, or a running install loses its stored keys.
        assert_eq!(SecretPurpose::OpenAiApiKey.slot_name(), "openai-api-key-v1");
        assert_eq!(
            SecretPurpose::ChatGptSubscriptionCredential.slot_name(),
            "chatgpt-subscription-credential-v1"
        );
        assert_eq!(
            SecretPurpose::PostprocessCacheMasterKey.slot_name(),
            "encrypted-store-master-v1"
        );
    }

    #[test]
    fn missing_document_fails_closed_to_off_defaults() {
        let path = test_path("missing");
        std::fs::remove_file(&path).ok();
        let loaded = HostedPreferences::load_at(&path).unwrap();
        assert_eq!(loaded, HostedPreferences::default());
        assert!(!loaded.values().master_enabled);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn partial_current_document_uses_safe_defaults_and_keeps_its_revision() {
        let path = test_path("partial");
        atomic_write_private(
            &path,
            b"schema = 1\nrevision = 9\n\n[preferences]\nshow_history_diagnostics = true\n",
            "hosted preferences",
        )
        .unwrap();
        let loaded = HostedPreferences::load_at(&path).unwrap();
        assert_eq!(loaded.revision(), 9);
        assert!(loaded.values().show_history_diagnostics);
        assert!(!loaded.values().master_enabled);
        assert_eq!(
            loaded.values().providers.openai.credential.purpose(),
            SecretPurpose::OpenAiApiKey
        );
        assert_eq!(
            loaded.values().providers.anthropic.credential.purpose(),
            SecretPurpose::AnthropicApiKey
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn legacy_codex_approval_is_accepted_for_migration_but_never_written_again() {
        let document = r#"
schema = 1
revision = 4

[preferences.providers]
codex_experimental_approved = true
"#;
        let loaded: HostedPreferences = toml::from_str(document).unwrap();
        let loaded = loaded.migrate_v1_to_v2();
        loaded.validate().unwrap();
        assert!(loaded.values().providers.legacy_codex_experimental_approved);
        let rewritten = toml::to_string_pretty(&loaded).unwrap();
        assert!(!rewritten.contains("codex"), "{rewritten}");
    }

    #[test]
    fn schema_one_migrates_to_two_with_the_pinned_template_as_a_subscription() {
        let path = test_path("migrate-v1");
        let document = r#"
schema = 1
revision = 12

[preferences]
master_enabled = true
egress_acknowledgement_version = 1
pinned_question_template = "What did we decide?"
pinned_auto_enabled = true
pinned_auto_acknowledgement_version = 1

[preferences.live]
enabled = true
provider = "google"
transport = "vertex_api"
model = "claude-sonnet-4-5"
local_cache = "reusable"
provider_cache = "off"
"#;
        atomic_write_private(&path, document.as_bytes(), "hosted preferences").unwrap();
        let loaded = HostedPreferences::load_at(&path).unwrap();
        assert_eq!(loaded.schema(), HOSTED_PREFERENCES_SCHEMA);
        assert_eq!(loaded.revision(), 12, "migration is not a user edit");
        let values = loaded.values();
        assert!(values.master_enabled);
        assert_eq!(
            values.live_first_text_seconds,
            DEFAULT_LIVE_FIRST_TEXT_SECONDS
        );
        assert_eq!(values.live_deadline_seconds, DEFAULT_LIVE_DEADLINE_SECONDS);
        assert_eq!(
            values.question_deadline_seconds,
            DEFAULT_QUESTION_DEADLINE_SECONDS
        );
        assert!(values.lexicon_enabled);
        assert_eq!(values.providers.vertex_thinking, ThinkingPreference::Auto);
        assert_eq!(values.subscriptions.len(), 1);
        let pinned = &values.subscriptions[0];
        assert_eq!(pinned.id, MIGRATED_PINNED_SUBSCRIPTION_ID);
        assert_eq!(pinned.template, "What did we decide?");
        assert!(pinned.enabled);
        assert_eq!(pinned.preset, "none");
        assert_eq!(pinned.output, "paragraph");
        assert_eq!(values.subscriptions_auto_acknowledgement_version, Some(1));
        // The pinned fields stay authoritative until the subscription coordinator replaces them.
        assert_eq!(values.pinned_question_template, "What did we decide?");
        assert!(values.pinned_auto_enabled);

        // Saving rewrites the document as schema 2 and it loads again unchanged.
        loaded.save_at(&path).unwrap();
        let reloaded = HostedPreferences::load_at(&path).unwrap();
        assert_eq!(reloaded, loaded);
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.starts_with("schema = 2"), "{rewritten}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn schema_two_deadline_and_subscription_bounds_are_enforced() {
        let base = HostedPreferences::default();
        assert!(
            base.revise(|values| values.live_first_text_seconds = 0)
                .is_err()
        );
        assert!(
            base.revise(|values| values.live_deadline_seconds = 1)
                .is_err()
        );
        assert!(
            base.revise(|values| values.question_deadline_seconds = 301)
                .is_err()
        );
        let tuned = base
            .revise(|values| {
                values.live_first_text_seconds = 12;
                values.live_deadline_seconds = 30;
                values.question_deadline_seconds = 60;
                values.providers.vertex_thinking = ThinkingPreference::Omit;
            })
            .unwrap();
        assert_eq!(tuned.revision(), 1);
        assert_eq!(tuned.values().live_deadline_seconds, 30);

        let subscription = |id: &str| QuestionSubscriptionPreferences {
            id: id.to_owned(),
            title: "Running summary".to_owned(),
            template: "Summarise what we have discussed so far.".to_owned(),
            enabled: true,
            preset: "running_summary".to_owned(),
            output: "bullets".to_owned(),
            ..QuestionSubscriptionPreferences::default()
        };
        assert!(
            base.revise(|values| values.subscriptions = vec![subscription("summary")])
                .is_ok()
        );
        assert!(
            base.revise(|values| values.subscriptions =
                vec![subscription("summary"), subscription("summary")])
                .is_err(),
            "duplicate ids"
        );
        assert!(
            base.revise(|values| values.subscriptions = vec![subscription("Not Valid!")])
                .is_err()
        );
        assert!(
            base.revise(|values| {
                let mut bad = subscription("summary");
                bad.output = "haiku".to_owned();
                values.subscriptions = vec![bad];
            })
            .is_err()
        );
    }

    #[test]
    fn provider_cache_acknowledgement_round_trips() {
        let mut values = HostedPreferenceValues::default();
        assert!(!values.provider_cache_acknowledged("google"));
        assert!(values.set_provider_cache_acknowledged("google", true));
        assert!(values.provider_cache_acknowledged("google"));
        assert!(!values.provider_cache_acknowledged("openai"));
        assert!(values.set_provider_cache_acknowledged("google", false));
        assert!(!values.provider_cache_acknowledged("google"));
        assert!(
            !values.set_provider_cache_acknowledged("chatgpt", true),
            "experimental transports have no controllable policy"
        );
    }

    #[test]
    fn unknown_schema_is_rejected_without_enabling_anything() {
        let path = test_path("schema");
        atomic_write_private(&path, b"schema = 999\nrevision = 0\n", "hosted preferences").unwrap();
        let error = HostedPreferences::load_at(&path).unwrap_err().to_string();
        assert!(error.contains("unsupported hosted preferences schema"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn cache_master_reference_is_fixed_and_secret_free() {
        let reference = SecretReference::cache_master_key();
        assert_eq!(
            reference.purpose(),
            SecretPurpose::PostprocessCacheMasterKey
        );
        let json = serde_json::to_string(&reference).unwrap();
        assert_eq!(
            json,
            r#"{"backend":"macos_non_synchronizing_generic_password","purpose":"postprocess_cache_master_key"}"#
        );
    }
}
