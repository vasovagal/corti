//! Build note-frontmatter provenance from the exact runtime config snapshot used for transcription.
//!
//! This is an allowlist of quality-relevant settings. Credentials, AWS profile/bucket, model directories,
//! and absolute override paths are deliberately excluded.

use std::collections::BTreeMap;

use corti_vagus::provenance::{
    GenerationMode, ModelIdentity, TranscriptModels, TranscriptProvenance,
};
use serde_json::{Map, Number, Value};

use crate::config::{AppConfig, BackendChoice};

/// Describe the final transcript generated under `cfg`. Callers must pass the same immutable snapshot the
/// backend/live engine owns, never re-read shared Settings at filing time.
pub(crate) fn from_config(cfg: &AppConfig, mode: GenerationMode) -> TranscriptProvenance {
    match cfg.transcribe_backend {
        BackendChoice::Aws => aws(cfg, mode),
        BackendChoice::Local => local(cfg, mode),
    }
}

fn aws(cfg: &AppConfig, mode: GenerationMode) -> TranscriptProvenance {
    let mut configuration = base_configuration(cfg, mode);
    configuration.insert("language".into(), Value::String(cfg.language.clone()));
    configuration.insert(
        "speaker_attribution".into(),
        Value::String("channel_identification_for_multichannel".into()),
    );
    TranscriptProvenance::new(
        mode,
        "aws",
        TranscriptModels {
            // AWS does not expose a selectable/versioned acoustic model for ordinary Transcribe jobs.
            asr: ModelIdentity::new("aws/transcribe-default", None::<String>),
            vad: None,
            diarization: None,
            speaker_embedding: None,
        },
        configuration,
    )
}

#[cfg(feature = "local")]
fn local(cfg: &AppConfig, mode: GenerationMode) -> TranscriptProvenance {
    use corti_transcribe_local::{LocalConfig, models};

    let defaults = LocalConfig::default();
    let wants_ggml = cfg.local_asr_engine == models::GGML_ASR_ENGINE;
    let asr_artifact = if wants_ggml {
        cfg.local_ggml_model
            .as_deref()
            .and_then(std::path::Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| models::GGML_FILE.to_string())
    } else {
        models::PARAKEET_DIR.to_string()
    };
    let selected_embedding = models::embedding_spec(&cfg.local_embedding_model);

    let mut configuration = base_configuration(cfg, mode);
    configuration.insert(
        "asr_engine".into(),
        Value::String(cfg.local_asr_engine.clone()),
    );
    configuration.insert("threads".into(), Value::from(i64::from(cfg.local_threads)));
    configuration.insert(
        "diarize_far_end".into(),
        Value::Bool(cfg.local_diarize_far_end),
    );
    configuration.insert(
        "speaker_attribution".into(),
        Value::String(if cfg.local_diarize_far_end {
            "channels+pyannote".into()
        } else {
            "channels".into()
        }),
    );
    configuration.insert(
        "diarization_threshold".into(),
        float_value(cfg.local_diarize_threshold),
    );
    configuration.insert("vad_threshold".into(), float_value(defaults.vad_threshold));
    configuration.insert(
        "vad_min_silence_seconds".into(),
        float_value(defaults.vad_min_silence),
    );

    TranscriptProvenance::new(
        mode,
        "local",
        TranscriptModels {
            asr: ModelIdentity::new(models::PARAKEET_MODEL_ID, Some(asr_artifact)),
            vad: Some(ModelIdentity::new(
                models::VAD_MODEL_ID,
                Some(models::VAD_FILE),
            )),
            diarization: cfg.local_diarize_far_end.then(|| {
                ModelIdentity::new(
                    models::SEGMENTATION_MODEL_ID,
                    Some(format!("{}/model.onnx", models::SEGMENTATION_DIR)),
                )
            }),
            speaker_embedding: cfg.local_diarize_far_end.then(|| {
                ModelIdentity::new(selected_embedding.id, Some(selected_embedding.install_rel))
            }),
        },
        configuration,
    )
}

#[cfg(not(feature = "local"))]
fn local(cfg: &AppConfig, mode: GenerationMode) -> TranscriptProvenance {
    // This can only be observed if a build without the local backend somehow files a successful local
    // transcript. Keep it truthful rather than borrowing AWS/current settings.
    let mut provenance = TranscriptProvenance::legacy_unknown(mode);
    provenance.backend = "local-unavailable".into();
    provenance.configuration = base_configuration(cfg, mode);
    provenance
}

fn base_configuration(cfg: &AppConfig, mode: GenerationMode) -> BTreeMap<String, Value> {
    let effective = cfg.aec_config();
    let mut aec = Map::new();
    aec.insert("enabled".into(), Value::Bool(cfg.aec_enabled));
    aec.insert(
        "mode".into(),
        Value::String(
            if !cfg.aec_enabled {
                "disabled"
            } else if mode == GenerationMode::Live {
                "streaming"
            } else {
                "offline"
            }
            .into(),
        ),
    );
    aec.insert(
        "filter_len".into(),
        Value::from(effective.filter_len as u64),
    );
    aec.insert("mu".into(), float_value(effective.mu));
    aec.insert("eps".into(), float_value(effective.eps));
    aec.insert(
        "power_smoothing".into(),
        float_value(effective.power_smoothing),
    );
    aec.insert(
        "double_talk_ratio".into(),
        float_value(effective.double_talk_ratio),
    );
    aec.insert(
        "suppress_residual".into(),
        float_value(effective.suppress_residual),
    );
    aec.insert("max_lag_ms".into(), float_value(effective.max_lag_ms));

    let mut configuration = BTreeMap::new();
    configuration.insert("aec".into(), Value::Object(aec));
    configuration.insert(
        "input".into(),
        Value::String(
            if mode == GenerationMode::Live {
                "live_pcm_stream"
            } else {
                "completed_recording"
            }
            .into(),
        ),
    );
    if mode == GenerationMode::Live {
        configuration.insert(
            "live_buffer_minutes".into(),
            Value::from(u64::from(cfg.live_buffer_minutes)),
        );
    }
    configuration
}

/// JSON numbers cannot represent NaN/±inf. Preserve a malformed-but-effective config value as an explicit
/// string rather than making provenance serialization fail after transcription has already succeeded.
fn float_value(value: f32) -> Value {
    Number::from_f64(f64::from(value))
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_batch_provenance_is_secret_free_and_names_the_opaque_service_model() {
        let cfg = AppConfig {
            transcribe_backend: BackendChoice::Aws,
            aws_bucket: Some("private-bucket".into()),
            aws_profile: Some("secret-profile".into()),
            aws_region: Some("us-secret-1".into()),
            language: "en-GB".into(),
            ..AppConfig::default()
        };
        let provenance = from_config(&cfg, GenerationMode::Batch);
        let json = provenance.frontmatter_json().unwrap();
        assert_eq!(provenance.mode, GenerationMode::Batch);
        assert_eq!(provenance.backend, "aws");
        assert_eq!(provenance.models.asr.id, "aws/transcribe-default");
        assert_eq!(provenance.configuration["language"], "en-GB");
        assert_eq!(provenance.configuration["input"], "completed_recording");
        assert!(!json.contains("private-bucket"));
        assert!(!json.contains("secret-profile"));
        assert!(!json.contains("us-secret-1"));
    }

    #[cfg(feature = "local")]
    #[test]
    fn live_local_provenance_names_models_representation_and_effective_settings() {
        let cfg = AppConfig {
            transcribe_backend: BackendChoice::Local,
            local_asr_engine: "ggml".into(),
            local_ggml_model: Some("/private/models/custom-parakeet.gguf".into()),
            local_threads: 6,
            local_diarize_far_end: true,
            local_embedding_model: "wespeaker-resnet34".into(),
            local_diarize_threshold: 0.65,
            live_buffer_minutes: 3,
            ..AppConfig::default()
        };
        let provenance = from_config(&cfg, GenerationMode::Live);
        let json = provenance.frontmatter_json().unwrap();
        assert_eq!(provenance.mode, GenerationMode::Live);
        assert_eq!(provenance.backend, "local");
        assert_eq!(provenance.models.asr.id, "nvidia/parakeet-tdt-0.6b-v3");
        assert_eq!(
            provenance.models.asr.artifact.as_deref(),
            Some("custom-parakeet.gguf")
        );
        assert_eq!(
            provenance.models.speaker_embedding.as_ref().unwrap().id,
            "wespeaker-resnet34"
        );
        assert_eq!(provenance.configuration["asr_engine"], "ggml");
        assert_eq!(provenance.configuration["threads"], 6);
        assert_eq!(provenance.configuration["live_buffer_minutes"], 3);
        assert_eq!(provenance.configuration["input"], "live_pcm_stream");
        assert_eq!(provenance.configuration["aec"]["mode"], "streaming");
        assert!(
            !json.contains("/private/models"),
            "absolute path leaked: {json}"
        );
    }

    #[test]
    fn nonfinite_config_is_described_without_breaking_serialization() {
        let cfg = AppConfig {
            aec_mu: Some(f32::NAN),
            ..AppConfig::default()
        };
        let provenance = from_config(&cfg, GenerationMode::Batch);
        assert_eq!(provenance.configuration["aec"]["mu"], "NaN");
        provenance.frontmatter_json().unwrap();
    }
}
