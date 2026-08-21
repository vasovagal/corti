//! Versioned provenance stored under the Corti-owned `corti` note-frontmatter key.
//!
//! The schema describes the generator, final live/batch path, model identities, quality-relevant effective
//! configuration, and only hosted post-processing that actually affected filed text. It deliberately
//! excludes credentials, cloud buckets/profiles, account/project ids, prompts, steering text, questions,
//! usage, and cost. [`TranscriptProvenance::frontmatter_json`] is the safe cross-process payload consumed by
//! `vagus add-note`; JSON objects are valid YAML flow mappings.

use std::collections::BTreeMap;
use std::fmt;

use corti_postprocess::SupportTier;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;

/// Current shape of the object below the note's `corti` frontmatter key.
pub const SCHEMA_VERSION: u32 = 2;
const HMAC_SHA256_BASE64URL_BYTES: usize = 43;
const MAX_METADATA_BYTES: usize = 512;

/// Which input path produced the final transcript text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationMode {
    /// PCM was transcribed from the bounded capture tee while the call was in progress.
    Live,
    /// A completed recording file was processed after capture (including live-path fallback).
    Batch,
}

/// One model plus the selected on-disk/provider representation when Corti knows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentity {
    /// Stable model/service identity.
    pub id: String,
    /// Selected representation or artifact name, never an absolute local path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

impl ModelIdentity {
    pub fn new(id: impl Into<String>, artifact: Option<impl Into<String>>) -> Self {
        Self {
            id: id.into(),
            artifact: artifact.map(Into::into),
        }
    }
}

/// Every model that can affect transcript text or speaker attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptModels {
    pub asr: ModelIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vad: Option<ModelIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diarization: Option<ModelIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speaker_embedding: Option<ModelIdentity>,
}

/// HMAC-SHA-256 fingerprint safe to persist without exposing a dictionary-testable plaintext hash.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ProvenanceFingerprint(String);

impl ProvenanceFingerprint {
    pub fn new(value: impl Into<String>) -> Result<Self, AppliedPostprocessError> {
        let value = value.into();
        if value.len() != HMAC_SHA256_BASE64URL_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(AppliedPostprocessError::InvalidFingerprint);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProvenanceFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProvenanceFingerprint(<opaque-hmac>)")
    }
}

impl Serialize for ProvenanceFingerprint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProvenanceFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliedPostprocessState {
    #[default]
    None,
    Live,
    LiveMixed,
    Final,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliedCacheSource {
    Local,
    Provider,
    Network,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalPostprocessOutcome {
    Applied,
    Disabled,
    Timeout,
    Failed,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedWordBankProvenance {
    pub revision: u64,
    pub fingerprint: ProvenanceFingerprint,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveRevisionSummary {
    pub count: u32,
    pub fingerprint: ProvenanceFingerprint,
}

/// Typed metadata needed to construct applied hosted provenance. Every free-form value is bounded and every
/// content-derived value is an opaque keyed fingerprint rather than text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedPostprocessDetails {
    pub provider: String,
    pub transport: String,
    pub support_tier: SupportTier,
    pub model: String,
    pub adapter_version: u32,
    pub prompt_version: u32,
    pub output_schema_version: u32,
    pub word_bank: AppliedWordBankProvenance,
    pub steering_fingerprint: ProvenanceFingerprint,
    pub cache_source: AppliedCacheSource,
    pub live_revision_summary: Option<LiveRevisionSummary>,
    pub final_outcome: Option<FinalPostprocessOutcome>,
}

/// Hosted provenance for the text that was actually applied. `None` state has no provider/model fields by
/// construction; a completely unrecorded `none` value is omitted from note frontmatter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedPostprocessProvenance {
    state: AppliedPostprocessState,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    support_tier: Option<SupportTier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    adapter_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_schema_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    word_bank: Option<AppliedWordBankProvenance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    steering_fingerprint: Option<ProvenanceFingerprint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_source: Option<AppliedCacheSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    live_revision_summary: Option<LiveRevisionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_outcome: Option<FinalPostprocessOutcome>,
}

impl Default for AppliedPostprocessProvenance {
    fn default() -> Self {
        Self::none()
    }
}

impl AppliedPostprocessProvenance {
    pub const fn none() -> Self {
        Self {
            state: AppliedPostprocessState::None,
            provider: None,
            transport: None,
            support_tier: None,
            model: None,
            adapter_version: None,
            prompt_version: None,
            output_schema_version: None,
            word_bank: None,
            steering_fingerprint: None,
            cache_source: None,
            live_revision_summary: None,
            final_outcome: None,
        }
    }

    pub fn not_applied(outcome: FinalPostprocessOutcome) -> Result<Self, AppliedPostprocessError> {
        if outcome == FinalPostprocessOutcome::Applied {
            return Err(AppliedPostprocessError::AppliedOutcomeWithoutText);
        }
        Ok(Self {
            final_outcome: Some(outcome),
            ..Self::none()
        })
    }

    pub fn applied(
        state: AppliedPostprocessState,
        details: AppliedPostprocessDetails,
    ) -> Result<Self, AppliedPostprocessError> {
        if state == AppliedPostprocessState::None {
            return Err(AppliedPostprocessError::MissingAppliedState);
        }
        if details.support_tier == SupportTier::Blocked {
            return Err(AppliedPostprocessError::BlockedSupportTier);
        }
        validate_metadata(&details.provider)?;
        validate_metadata(&details.transport)?;
        validate_metadata(&details.model)?;
        let provenance = Self {
            state,
            provider: Some(details.provider),
            transport: Some(details.transport),
            support_tier: Some(details.support_tier),
            model: Some(details.model),
            adapter_version: Some(details.adapter_version),
            prompt_version: Some(details.prompt_version),
            output_schema_version: Some(details.output_schema_version),
            word_bank: Some(details.word_bank),
            steering_fingerprint: Some(details.steering_fingerprint),
            cache_source: Some(details.cache_source),
            live_revision_summary: details.live_revision_summary,
            final_outcome: details.final_outcome,
        };
        provenance.validate()?;
        Ok(provenance)
    }

    pub const fn state(&self) -> AppliedPostprocessState {
        self.state
    }

    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub const fn final_outcome(&self) -> Option<FinalPostprocessOutcome> {
        self.final_outcome
    }

    pub fn is_unrecorded_none(&self) -> bool {
        self == &Self::none()
    }

    pub fn validate(&self) -> Result<(), AppliedPostprocessError> {
        let provider_fields_present = [
            self.provider.is_some(),
            self.transport.is_some(),
            self.support_tier.is_some(),
            self.model.is_some(),
            self.adapter_version.is_some(),
            self.prompt_version.is_some(),
            self.output_schema_version.is_some(),
            self.word_bank.is_some(),
            self.steering_fingerprint.is_some(),
            self.cache_source.is_some(),
        ];
        match self.state {
            AppliedPostprocessState::None => {
                if provider_fields_present.into_iter().any(|present| present)
                    || self.live_revision_summary.is_some()
                {
                    return Err(AppliedPostprocessError::MetadataWithoutAppliedText);
                }
                if self.final_outcome == Some(FinalPostprocessOutcome::Applied) {
                    return Err(AppliedPostprocessError::AppliedOutcomeWithoutText);
                }
            }
            _ => {
                if provider_fields_present.into_iter().any(|present| !present) {
                    return Err(AppliedPostprocessError::IncompleteAppliedMetadata);
                }
                if self.support_tier == Some(SupportTier::Blocked) {
                    return Err(AppliedPostprocessError::BlockedSupportTier);
                }
                validate_metadata(self.provider.as_deref().expect("checked above"))?;
                validate_metadata(self.transport.as_deref().expect("checked above"))?;
                validate_metadata(self.model.as_deref().expect("checked above"))?;
            }
        }
        Ok(())
    }
}

fn validate_metadata(value: &str) -> Result<(), AppliedPostprocessError> {
    if value.is_empty() || value.len() > MAX_METADATA_BYTES || value.chars().any(char::is_control) {
        return Err(AppliedPostprocessError::InvalidMetadata);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppliedPostprocessError {
    InvalidFingerprint,
    InvalidMetadata,
    MissingAppliedState,
    IncompleteAppliedMetadata,
    MetadataWithoutAppliedText,
    AppliedOutcomeWithoutText,
    BlockedSupportTier,
}

impl fmt::Display for AppliedPostprocessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidFingerprint => {
                "postprocess fingerprint is not an opaque HMAC-SHA-256 value"
            }
            Self::InvalidMetadata => {
                "postprocess metadata is empty, oversized, or contains controls"
            }
            Self::MissingAppliedState => "applied postprocess metadata requires an applied state",
            Self::IncompleteAppliedMetadata => "applied postprocess metadata is incomplete",
            Self::MetadataWithoutAppliedText => {
                "provider metadata must be omitted when no hosted text was applied"
            }
            Self::AppliedOutcomeWithoutText => {
                "an applied final outcome requires applied hosted text"
            }
            Self::BlockedSupportTier => "a blocked provider cannot have applied hosted text",
        })
    }
}

impl std::error::Error for AppliedPostprocessError {}

/// Durable generation identity filed with one transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptProvenance {
    pub schema: u32,
    /// Corti package version compiled into the generating binary.
    pub version: String,
    pub mode: GenerationMode,
    /// Stable backend id (`local`, `aws`, or `unknown` for a pre-schema checkpoint).
    pub backend: String,
    pub models: TranscriptModels,
    /// Sorted, quality-relevant effective settings. Flexible so schema-1 remains readable without coupling
    /// Vagus or old filing checkpoints to every app config field.
    pub configuration: BTreeMap<String, Value>,
    /// Optional schema-2 object. Omitted entirely when hosted text did not affect the note and no settled
    /// final outcome was explicitly recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    postprocess: Option<AppliedPostprocessProvenance>,
}

impl TranscriptProvenance {
    pub fn new(
        mode: GenerationMode,
        backend: impl Into<String>,
        models: TranscriptModels,
        configuration: BTreeMap<String, Value>,
    ) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            version: env!("CARGO_PKG_VERSION").to_string(),
            mode,
            backend: backend.into(),
            models,
            configuration,
            postprocess: None,
        }
    }

    /// Truthful fallback for a transcript checkpoint written before provenance existed. Never infer from
    /// current Settings: they may have changed since ASR ran.
    pub fn legacy_unknown(mode: GenerationMode) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            version: "unknown".to_string(),
            mode,
            backend: "unknown".to_string(),
            models: TranscriptModels {
                asr: ModelIdentity::new("unknown", None::<String>),
                vad: None,
                diarization: None,
                speaker_embedding: None,
            },
            configuration: BTreeMap::new(),
            postprocess: None,
        }
    }

    pub fn postprocess(&self) -> Option<&AppliedPostprocessProvenance> {
        self.postprocess.as_ref()
    }

    pub fn set_postprocess(
        &mut self,
        postprocess: AppliedPostprocessProvenance,
    ) -> Result<(), AppliedPostprocessError> {
        postprocess.validate()?;
        self.schema = SCHEMA_VERSION;
        self.postprocess = (!postprocess.is_unrecorded_none()).then_some(postprocess);
        Ok(())
    }

    fn validate(&self) -> Result<(), AppliedPostprocessError> {
        if let Some(postprocess) = &self.postprocess {
            postprocess.validate()?;
        }
        Ok(())
    }

    /// JSON object passed to Vagus. The wrapper creates exactly one producer-owned top-level key.
    pub fn frontmatter_json(&self) -> serde_json::Result<String> {
        #[derive(Serialize)]
        struct Frontmatter<'a> {
            corti: &'a TranscriptProvenance,
        }
        self.validate().map_err(provenance_json_error)?;
        serde_json::to_string(&Frontmatter { corti: self })
    }

    /// One YAML-safe line for Corti's standalone renderer and same-note fallback rewrite. The value remains
    /// compact JSON, which is valid YAML flow syntax and cannot inject another frontmatter field.
    pub fn frontmatter_line(&self) -> serde_json::Result<String> {
        self.validate().map_err(provenance_json_error)?;
        Ok(format!("corti: {}\n", serde_json::to_string(self)?))
    }
}

fn provenance_json_error(error: AppliedPostprocessError) -> serde_json::Error {
    serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(byte: char) -> ProvenanceFingerprint {
        ProvenanceFingerprint::new(byte.to_string().repeat(HMAC_SHA256_BASE64URL_BYTES)).unwrap()
    }

    #[test]
    fn frontmatter_payload_is_namespaced_versioned_and_yaml_safe() {
        let mut configuration = BTreeMap::new();
        configuration.insert(
            "language".into(),
            Value::String("en-US\nstatus: synthetic".into()),
        );
        let provenance = TranscriptProvenance::new(
            GenerationMode::Batch,
            "aws",
            TranscriptModels {
                asr: ModelIdentity::new("aws/transcribe", None::<String>),
                vad: None,
                diarization: None,
                speaker_embedding: None,
            },
            configuration,
        );

        let payload = provenance.frontmatter_json().unwrap();
        let parsed: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["corti"]["schema"], SCHEMA_VERSION);
        assert_eq!(parsed["corti"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(parsed["corti"]["mode"], "batch");
        assert!(
            !payload.contains("\nstatus:"),
            "JSON must escape embedded newlines"
        );
        assert_eq!(provenance.frontmatter_line().unwrap().lines().count(), 1);
    }

    #[test]
    fn provenance_omits_postprocess_when_no_hosted_text_or_outcome_exists() {
        let provenance = TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
        let value = serde_json::to_value(&provenance).unwrap();
        assert_eq!(value["schema"], SCHEMA_VERSION);
        assert!(value.get("postprocess").is_none());
        let serialized = serde_json::to_string(&provenance).unwrap();
        for forbidden in [
            "provider",
            "transport",
            "support_tier",
            "prompt_version",
            "steering_fingerprint",
            "question",
            "cost_micros",
            "credential",
            "account",
        ] {
            assert!(!serialized.contains(forbidden), "unexpected {forbidden}");
        }
    }

    #[test]
    fn no_applied_text_may_record_outcome_but_omits_provider_and_model() {
        let mut provenance = TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
        provenance
            .set_postprocess(
                AppliedPostprocessProvenance::not_applied(FinalPostprocessOutcome::Timeout)
                    .unwrap(),
            )
            .unwrap();
        let value = serde_json::to_value(&provenance).unwrap();
        let postprocess = &value["postprocess"];
        assert_eq!(postprocess["state"], "none");
        assert_eq!(postprocess["final_outcome"], "timeout");
        assert!(postprocess.get("provider").is_none());
        assert!(postprocess.get("model").is_none());
        assert!(postprocess.get("word_bank").is_none());
    }

    #[test]
    fn applied_text_serializes_only_bounded_content_free_metadata() {
        let applied = AppliedPostprocessProvenance::applied(
            AppliedPostprocessState::Final,
            AppliedPostprocessDetails {
                provider: "openai".into(),
                transport: "openai_api".into(),
                support_tier: SupportTier::Documented,
                model: "fixture-model".into(),
                adapter_version: 1,
                prompt_version: 1,
                output_schema_version: 1,
                word_bank: AppliedWordBankProvenance {
                    revision: 17,
                    fingerprint: fingerprint('A'),
                    count: 3,
                },
                steering_fingerprint: fingerprint('B'),
                cache_source: AppliedCacheSource::Network,
                live_revision_summary: None,
                final_outcome: Some(FinalPostprocessOutcome::Applied),
            },
        )
        .unwrap();
        let value = serde_json::to_value(&applied).unwrap();
        assert_eq!(value["state"], "final");
        assert_eq!(value["provider"], "openai");
        assert_eq!(value["model"], "fixture-model");
        assert!(value.get("prompt").is_none());
        assert!(value.get("steering").is_none());
        assert!(value.get("question").is_none());
        assert!(value.get("cost").is_none());
    }

    #[test]
    fn schema_one_provenance_still_deserializes_with_no_postprocess() {
        let value = r#"{
            "schema":1,
            "version":"0.12.0",
            "mode":"batch",
            "backend":"unknown",
            "models":{"asr":{"id":"unknown"}},
            "configuration":{}
        }"#;
        let loaded: TranscriptProvenance = serde_json::from_str(value).unwrap();
        assert_eq!(loaded.schema, 1);
        assert!(loaded.postprocess().is_none());
    }

    #[test]
    fn legacy_checkpoint_provenance_never_borrows_current_settings() {
        let legacy = TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
        assert_eq!(legacy.version, "unknown");
        assert_eq!(legacy.backend, "unknown");
        assert_eq!(legacy.models.asr.id, "unknown");
        assert!(legacy.configuration.is_empty());
        assert!(legacy.postprocess().is_none());
    }
}
