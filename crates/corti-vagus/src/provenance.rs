//! Versioned provenance stored under the Corti-owned `corti` note-frontmatter key.
//!
//! The schema describes the generator, final live/batch path, model identities, and quality-relevant
//! effective configuration. It deliberately excludes credentials, cloud buckets/profiles, and absolute
//! model paths. [`TranscriptProvenance::frontmatter_json`] is the safe cross-process payload consumed by
//! `vagus add-note`; JSON objects are valid YAML flow mappings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current shape of the object below the note's `corti` frontmatter key.
pub const SCHEMA_VERSION: u32 = 1;

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
    /// Sorted, quality-relevant effective settings. Flexible so schema-1 can add optional knobs without
    /// coupling Vagus or old filing checkpoints to every app config field.
    pub configuration: BTreeMap<String, Value>,
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
        }
    }

    /// JSON object passed to Vagus. The wrapper creates exactly one producer-owned top-level key.
    pub fn frontmatter_json(&self) -> serde_json::Result<String> {
        #[derive(Serialize)]
        struct Frontmatter<'a> {
            corti: &'a TranscriptProvenance,
        }
        serde_json::to_string(&Frontmatter { corti: self })
    }

    /// One YAML-safe line for Corti's standalone renderer and same-note fallback rewrite. The value remains
    /// compact JSON, which is valid YAML flow syntax and cannot inject another frontmatter field.
    pub fn frontmatter_line(&self) -> serde_json::Result<String> {
        Ok(format!("corti: {}\n", serde_json::to_string(self)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_payload_is_namespaced_versioned_and_yaml_safe() {
        let mut configuration = BTreeMap::new();
        configuration.insert(
            "language".into(),
            Value::String("en-US\nstatus: hacked".into()),
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
    fn legacy_checkpoint_provenance_never_borrows_current_settings() {
        let legacy = TranscriptProvenance::legacy_unknown(GenerationMode::Batch);
        assert_eq!(legacy.version, "unknown");
        assert_eq!(legacy.backend, "unknown");
        assert_eq!(legacy.models.asr.id, "unknown");
        assert!(legacy.configuration.is_empty());
    }
}
