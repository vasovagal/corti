//! Local on-device backend for corti — fully offline transcription on Apple Silicon.
//!
//! Engine: NVIDIA Parakeet-TDT-0.6B-v3 (ONNX) via the official `sherpa-onnx` Rust crate, CPU provider by
//! default. corti records a 2-track WAV (ch0 = me/mic, ch1 = them/system-tap), so diarization for the
//! me-vs-them split is just the channel: ch0 → [`Speaker::Me`], ch1 → `Speaker::Other("Them")`. The
//! far-end channel can **optionally** be diarized into `Them 1/2/…` (pyannote-segmentation-3.0 + 3D-Speaker
//! embedding, both ONNX) when [`LocalConfig::diarize_far_end`] is set — off by default (it over-clusters on
//! English audio today; see issue #18).
//!
//! Pipeline per channel: resample to 16 kHz → Silero VAD into speech regions → Parakeet ASR per region
//! (token timestamps) → reassemble words; far-end words are attributed to diarization turns. Words are
//! shaped into segments by the shared [`corti_transcribe::segment`] helpers and merged onto one timeline.
//!
//! See `design/02-corti-transcribe.md` and `design/adr/0003-local-asr-sherpa-onnx.md`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use corti_core::{DiarizedTranscript, RecordingMeta, Speaker};
use corti_transcribe::Transcriber;
use corti_transcribe::segment::{SEGMENT_GAP, diarize_words, merge_by_time, words_to_segments};

mod audio;
mod engine;
mod models;

/// Where the ONNX models live and how to run them. Built by the app from its config.
#[derive(Debug, Clone)]
pub struct LocalConfig {
    /// Directory holding the model files (Parakeet, pyannote segmentation, embedding, VAD).
    /// `None` ⇒ the default cache (`~/Library/Caches/corti/models/`), resolved by the backend.
    pub model_dir: Option<PathBuf>,
    /// ONNX Runtime execution provider. `"cpu"` (default) is the only one that ships: the prebuilt
    /// sherpa-onnx static lib has no CoreML execution provider, and CoreML measured 4.6–11× *slower* on the
    /// int8 transducer anyway (see [`resolve_provider`] and design/adr/0003). A non-`cpu` value is honored
    /// only in a build with the `coreml-lib` feature + a CoreML-enabled lib; otherwise it maps to `"cpu"`.
    pub provider: String,
    /// ONNX intra-op threads. Small by default (a short batch job → favours battery on the M1 Pro).
    pub num_threads: i32,
    /// Split the far-end channel (ch1) into per-speaker labels (`Them 1/2/…`) via ONNX diarization
    /// (pyannote-segmentation-3.0 + 3D-Speaker). **Off by default** — the default attributes the whole
    /// far end to a single `Them` (like the AWS backend). When off, the segmentation + embedding models are
    /// not required. Far-end diarization currently over-clusters on English audio (issue #18).
    pub diarize_far_end: bool,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            model_dir: None,
            provider: "cpu".to_string(),
            num_threads: 4,
            diarize_far_end: false,
        }
    }
}

/// Local offline transcriber (Parakeet-TDT via sherpa-onnx). Models load lazily on `transcribe`.
#[derive(Debug, Clone)]
pub struct LocalTranscriber {
    cfg: LocalConfig,
}

impl LocalTranscriber {
    pub fn new(cfg: LocalConfig) -> Self {
        Self { cfg }
    }
}

impl Transcriber for LocalTranscriber {
    fn transcribe(&self, audio: &Path, _meta: &RecordingMeta) -> Result<DiarizedTranscript> {
        let dir = models::resolve_dir(self.cfg.model_dir.clone())?;
        let m = models::discover(&dir, self.cfg.diarize_far_end)?;
        let track = audio::read_two_track(audio)?;
        let provider = resolve_provider(self.cfg.provider.as_str());
        let threads = self.cfg.num_threads;

        // One recognizer load per job, shared across both channels.
        let rec = engine::build_recognizer(&m, provider, threads)?;
        let mut segments = Vec::new();

        // ch0 (mic) → Me. Channel = speaker; no diarizer needed.
        if !track.mic.is_empty() {
            let mic = engine::resample_to_16k(&track.mic, track.sample_rate)?;
            let vad = engine::build_vad(&m, provider)?;
            let words = engine::transcribe_channel(&rec, &vad, &mic);
            segments.extend(words_to_segments(&words, Speaker::Me, SEGMENT_GAP));
        }

        // ch1 (system tap) → far end.
        if !track.them.is_empty() {
            let them = engine::resample_to_16k(&track.them, track.sample_rate)?;
            let vad = engine::build_vad(&m, provider)?;
            let words = engine::transcribe_channel(&rec, &vad, &them);
            if self.cfg.diarize_far_end {
                // Opt-in: split the far end into per-speaker labels (Them 1/2/…).
                let diar = engine::build_diarizer(&m, provider, threads)?;
                let turns = engine::diarize_channel(&diar, &them);
                segments.extend(diarize_words(&words, &turns, SEGMENT_GAP, "Them"));
            } else {
                // Default: attribute the whole far end to a single speaker (like the AWS backend).
                let them_speaker = Speaker::Other("Them".to_string());
                segments.extend(words_to_segments(&words, them_speaker, SEGMENT_GAP));
            }
        }

        Ok(DiarizedTranscript::new(merge_by_time(segments)))
    }
}

/// Resolve the requested ONNX execution provider against what this build can actually honor.
///
/// A `coreml` request only works when the crate is compiled with the `coreml-lib` feature, which links a
/// CoreML-enabled sherpa-onnx (see `build.rs`). The default crates.io prebuilt has **no** CoreML execution
/// provider, so sherpa-onnx would silently fall back to CPU while printing a misleading
/// `"CoreML is for Apple only since onnxruntime>=1.15. Fallback to cpu!"` line for *every* ONNX session
/// (3 ASR + 2 VAD = 5 lines for a typical job) — which reads like platform misdetection on an Apple-Silicon
/// Mac. When CoreML can't be honored, map the request to `cpu` ourselves and say so once, clearly.
fn resolve_provider(requested: &str) -> &str {
    if requested != "cpu" && !cfg!(feature = "coreml-lib") {
        eprintln!(
            "[corti] provider {requested:?} unavailable in this build — running on CPU \
             (build with the `coreml-lib` feature + a CoreML-enabled sherpa-onnx lib to enable it)"
        );
        return "cpu";
    }
    requested
}

#[cfg(test)]
mod tests {
    use super::resolve_provider;

    #[test]
    fn cpu_passes_through() {
        assert_eq!(resolve_provider("cpu"), "cpu");
    }

    #[test]
    #[cfg(not(feature = "coreml-lib"))]
    fn coreml_falls_back_to_cpu_without_the_feature() {
        // Default build links a sherpa-onnx with no CoreML EP, so we map it to cpu rather than let
        // sherpa-onnx emit its misleading per-session fallback log.
        assert_eq!(resolve_provider("coreml"), "cpu");
    }

    #[test]
    #[cfg(feature = "coreml-lib")]
    fn coreml_passes_through_with_the_feature() {
        assert_eq!(resolve_provider("coreml"), "coreml");
    }
}
