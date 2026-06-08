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

/// Build the [`Models`] paths under `dir` and verify the required files exist, with an actionable error
/// naming the missing files and the fetch script if not. The segmentation + embedding models are required
/// only when `need_diarization` is set (far-end speaker splitting); the default path needs just Parakeet +
/// VAD.
pub fn discover(dir: &Path, need_diarization: bool) -> Result<Models> {
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

    let missing: Vec<&Path> = models
        .required(need_diarization)
        .into_iter()
        .filter(|p| !p.exists())
        .collect();
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
    /// The model files that must exist: always Parakeet + VAD; segmentation + embedding only when far-end
    /// diarization is enabled.
    fn required(&self, need_diarization: bool) -> Vec<&Path> {
        let mut req = vec![
            self.parakeet_encoder.as_path(),
            self.parakeet_decoder.as_path(),
            self.parakeet_joiner.as_path(),
            self.parakeet_tokens.as_path(),
            self.vad.as_path(),
        ];
        if need_diarization {
            req.push(self.segmentation.as_path());
            req.push(self.embedding.as_path());
        }
        req
    }
}

/// How a downloaded model artifact is installed under the model dir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// A `.tar.bz2` archive extracted into the model dir (yields a directory).
    Tarball,
    /// A bare file placed directly in the model dir.
    File,
}

/// One downloadable model artifact: where to fetch it, its pinned digest/size, and where it lands. The
/// single source of truth shared by the in-app model status + downloader — the in-app equivalent of
/// `fetch-models.sh`, with checksums it lacks (issue #24; supersedes the out-of-band script's bug, #17).
#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// Stable id for the UI + download command.
    pub id: &'static str,
    /// Human-readable label.
    pub label: &'static str,
    /// Download URL (a pinned sherpa-onnx release asset).
    pub url: &'static str,
    /// SHA-256 of the **downloaded artifact** (the `.tar.bz2` or the bare file), pinned so a truncated or
    /// tampered download is caught before it's installed.
    pub sha256: &'static str,
    /// Exact size of the downloaded artifact in bytes (a download-progress denominator + a sanity check
    /// against the response's Content-Length).
    pub download_bytes: u64,
    /// How to install the artifact (extract vs. place).
    pub kind: ArtifactKind,
    /// Needed only when far-end diarization is enabled (segmentation + embedding); the default path needs
    /// just Parakeet + VAD.
    pub diarize_only: bool,
    /// Path, relative to the model dir, that this artifact installs — a directory for tarballs, a file for
    /// bare files. Removed before a re-download so a partial install can't linger.
    pub install_rel: &'static str,
    /// Path, relative to the model dir, that must exist for the model to count as present (a key file).
    pub present_rel: &'static str,
}

/// The pinned catalog of model artifacts the local backend needs. Digests and sizes were computed from the
/// pinned sherpa-onnx release assets; keep them in lock-step with the URLs.
pub fn model_catalog() -> Vec<ModelSpec> {
    // URLs share the prefix `https://github.com/k2-fsa/sherpa-onnx/releases/download`, spelled in full
    // below so each field is a plain `&'static str`.
    vec![
        ModelSpec {
            id: "parakeet",
            label: "Parakeet-TDT 0.6B v3 (ASR)",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
            sha256: "5793d0fd397c5778d2cf2126994d58e9d56b1be7c04d13c7a15bb1b4eafb16bf",
            download_bytes: 487170055,
            kind: ArtifactKind::Tarball,
            diarize_only: false,
            install_rel: PARAKEET_DIR,
            present_rel: "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx",
        },
        ModelSpec {
            id: "vad",
            label: "Silero VAD",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
            sha256: "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6",
            download_bytes: 643854,
            kind: ArtifactKind::File,
            diarize_only: false,
            install_rel: VAD_FILE,
            present_rel: VAD_FILE,
        },
        ModelSpec {
            id: "segmentation",
            label: "pyannote speaker segmentation 3.0",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2",
            sha256: "24615ee884c897d9d2ba09bb4d30da6bb1b15e685065962db5b02e76e4996488",
            download_bytes: 6958444,
            kind: ArtifactKind::Tarball,
            diarize_only: true,
            install_rel: SEGMENTATION_DIR,
            present_rel: "sherpa-onnx-pyannote-segmentation-3-0/model.onnx",
        },
        ModelSpec {
            id: "embedding",
            label: "3D-Speaker embedding (16 kHz)",
            url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_eres2net_base_sv_zh-cn_3dspeaker_16k.onnx",
            sha256: "1a331345f04805badbb495c775a6ddffcdd1a732567d5ec8b3d5749e3c7a5e4b",
            download_bytes: 39593761,
            kind: ArtifactKind::File,
            diarize_only: true,
            install_rel: EMBEDDING_FILE,
            present_rel: EMBEDDING_FILE,
        },
    ]
}
