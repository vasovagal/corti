//! Resolve and validate the ONNX model files the local backend needs.
//!
//! Layout under the model dir (default `~/Library/Caches/corti/models/`, guardrail #5 — outside any vault):
//! ```text
//! sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/{encoder,decoder,joiner}.int8.onnx, tokens.txt
//! sherpa-onnx-pyannote-segmentation-3-0/model.onnx
//! 3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx
//! silero_vad.onnx
//! ```
//! Fetch them once with `crates/corti-transcribe-local/fetch-models.sh` (pinned to sherpa-onnx releases).
//! In-app first-run download is a tracked follow-up (`Feature`: model-management UX).

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// Parakeet-TDT-0.6B-v3 (multilingual, int8) — directory name as extracted from the release archive.
pub const PARAKEET_DIR: &str = "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8";
/// pyannote speaker-segmentation 3.0 — directory name as extracted from the release archive.
pub const SEGMENTATION_DIR: &str = "sherpa-onnx-pyannote-segmentation-3-0";
/// 3D-Speaker speaker-embedding model (general, 16 kHz). NOTE: trained on zh-cn; speaker embeddings
/// transfer across languages but an English/VoxCeleb model may separate far-end speakers better — tracked
/// as a quality follow-up (`Feature`: evaluate English speaker-embedding model).
pub const EMBEDDING_FILE: &str = "3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx";
/// Silero VAD model.
pub const VAD_FILE: &str = "silero_vad.onnx";

/// Resolved, validated paths to every model file the backend loads.
#[derive(Debug, Clone)]
pub struct Models {
    pub parakeet_encoder: PathBuf,
    pub parakeet_decoder: PathBuf,
    pub parakeet_joiner: PathBuf,
    pub parakeet_tokens: PathBuf,
    pub segmentation: PathBuf,
    pub embedding: PathBuf,
    pub vad: PathBuf,
}

/// The model directory: explicit override, else `~/Library/Caches/corti/models/`.
pub fn resolve_dir(override_dir: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(d) = override_dir {
        return Ok(d);
    }
    let cache =
        dirs::cache_dir().ok_or_else(|| anyhow::anyhow!("cannot resolve a cache directory"))?;
    Ok(cache.join("corti").join("models"))
}

/// Build the [`Models`] paths under `dir` and verify every file exists, with an actionable error naming
/// the missing files and the fetch script if not.
pub fn discover(dir: &Path) -> Result<Models> {
    let parakeet = dir.join(PARAKEET_DIR);
    let models = Models {
        parakeet_encoder: parakeet.join("encoder.int8.onnx"),
        parakeet_decoder: parakeet.join("decoder.int8.onnx"),
        parakeet_joiner: parakeet.join("joiner.int8.onnx"),
        parakeet_tokens: parakeet.join("tokens.txt"),
        segmentation: dir.join(SEGMENTATION_DIR).join("model.onnx"),
        embedding: dir.join(EMBEDDING_FILE),
        vad: dir.join(VAD_FILE),
    };

    let missing: Vec<&Path> = models.all().into_iter().filter(|p| !p.exists()).collect();
    if !missing.is_empty() {
        let list = missing
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "local transcription models are missing under {}:\n{}\n\nFetch them once with:\n  \
             crates/corti-transcribe-local/fetch-models.sh {}",
            dir.display(),
            list,
            dir.display(),
        );
    }
    Ok(models)
}

impl Models {
    fn all(&self) -> Vec<&Path> {
        vec![
            &self.parakeet_encoder,
            &self.parakeet_decoder,
            &self.parakeet_joiner,
            &self.parakeet_tokens,
            &self.segmentation,
            &self.embedding,
            &self.vad,
        ]
    }
}
