//! Local on-device backend for corti — fully offline transcription on Apple Silicon.
//!
//! Engine: NVIDIA Parakeet-TDT-0.6B-v3 (ONNX) via the official `sherpa-onnx` Rust crate, CPU provider by
//! default. corti records a 2-track WAV (ch0 = me/mic, ch1 = them/system-tap), so diarization for the
//! me-vs-them split is just the channel: ch0 → [`Speaker::Me`], ch1 → `Speaker::Other("Them")`. The
//! far-end channel can **optionally** be diarized into `Them 1/2/…` (pyannote-segmentation-3.0 + a
//! runtime-selectable English speaker-embedding model, both ONNX) when [`LocalConfig::diarize_far_end`] is
//! set — off by default. Over-clustering on English audio is tracked as issue #18; the embedding model and a
//! [`LocalConfig::diarize_threshold`] knob are tunable to address it.
//!
//! Pipeline per channel: resample to 16 kHz → Silero VAD into speech regions → Parakeet ASR per region
//! (token timestamps) → reassemble words; far-end words are attributed to diarization turns. Words are
//! shaped into segments by the shared [`corti_transcribe::segment`] helpers and merged onto one timeline.
//!
//! See `design/02-corti-transcribe.md` and `design/adr/0003-local-asr-sherpa-onnx.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use corti_core::{DiarizedTranscript, RecordingMeta, Speaker};
use corti_transcribe::Transcriber;
use corti_transcribe::segment::{SEGMENT_GAP, diarize_words, merge_by_time, words_to_segments};

mod asr;
mod audio;
mod engine;
#[cfg(feature = "ggml")]
pub mod ggml;
mod live;
pub mod models;

pub use asr::Asr;
pub use corti_transcribe::segment::Word;
pub use live::{LiveEngine, LiveTranscriber};
#[cfg(feature = "stream")]
pub use live::{LiveSink, LiveWordStream, live_word_stream};

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
    /// (pyannote-segmentation-3.0 + the selected embedding model). **Off by default** — the default
    /// attributes the whole far end to a single `Them` (like the AWS backend). When off, the segmentation +
    /// embedding models are not required.
    pub diarize_far_end: bool,
    /// Which English speaker-embedding model to use for far-end diarization (a [`models::EMBEDDING_IDS`] id;
    /// unknown/empty falls back to [`models::DEFAULT_EMBEDDING_ID`]).
    pub embedding_model: String,
    /// Diarization clustering threshold (0.0–1.0) that estimates the far-end speaker count — higher merges
    /// more, lower splits more. Default `0.5` (sherpa-onnx's default); tune to curb over-clustering (#18).
    pub diarize_threshold: f32,
    /// Silero VAD speech-probability threshold (0.0–1.0). Default `0.5` (Silero's default). Lower detects
    /// more speech (fewer dropped words, more false positives on noise); higher is stricter.
    pub vad_threshold: f32,
    /// Silero VAD minimum trailing silence (seconds) before a speech region is closed. Default `1.0`
    /// (benchmark-tuned, up from Silero's 0.25). Larger keeps within-utterance pauses together so each ASR
    /// chunk carries more context (up to the 20 s `MAX_SPEECH_SECONDS` cap); smaller splits more
    /// aggressively at pauses, fragmenting words across chunk seams. The Planet Money sweep
    /// (`design/06-benchmark-harness.md`) showed a monotone WER drop 0.25→1.25 (≈ −38 % relative), plateauing
    /// ~1.0–1.5; 1.0 is the conservative pick capturing essentially all of it.
    pub vad_min_silence: f32,
    /// Far-end diarization speaker count: `-1` (default) auto-estimates via [`Self::diarize_threshold`];
    /// a value `> 0` pins a known speaker count (avoids over/under-clustering, #18) when it is known a priori.
    pub diarize_num_clusters: i32,
    /// Far-end diarization minimum speech-on duration (seconds) for a segment. Default `0.3`.
    pub diarize_min_duration_on: f32,
    /// Far-end diarization minimum silence-off duration (seconds) between segments. Default `0.5`.
    pub diarize_min_duration_off: f32,
    /// ASR decoding method override: `None` (default) keeps sherpa-onnx's default (greedy); `Some("modified_beam_search")`
    /// switches to beam search (use with [`Self::asr_max_active_paths`]). `None` ⇒ byte-identical to today.
    pub asr_decoding: Option<String>,
    /// Beam width for `modified_beam_search` (sherpa `max_active_paths`); `None` keeps sherpa's default.
    pub asr_max_active_paths: Option<i32>,
    /// Transducer blank penalty; higher discourages blank (fewer deletions, risks insertions). `None` keeps the default.
    pub asr_blank_penalty: Option<f32>,
    /// Which engine decodes VAD speech regions: `"sherpa"` (default — today's shipping ONNX path) or
    /// `"ggml"` (ADR 0011 spike — the same Parakeet-TDT-0.6B-v3 as a GGUF via transcribe.cpp, Metal on
    /// Apple Silicon; requires a build with the `ggml` feature, else a clear runtime error). VAD and
    /// far-end diarization stay on sherpa-onnx either way, so this knob isolates the ASR runtime.
    pub asr_engine: String,
    /// Explicit GGUF path for the `ggml` engine; `None` ⇒ `<model dir>/parakeet-tdt-0.6b-v3-Q8_0.gguf`
    /// (see `ggml::DEFAULT_GGUF_FILE`). Ignored by the `sherpa` engine.
    pub ggml_model: Option<PathBuf>,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            model_dir: None,
            provider: "cpu".to_string(),
            num_threads: 4,
            diarize_far_end: false,
            embedding_model: models::DEFAULT_EMBEDDING_ID.to_string(),
            diarize_threshold: 0.5,
            vad_threshold: 0.5,
            vad_min_silence: 1.0, // benchmark-tuned (was 0.25); see field doc + design/06-benchmark-harness.md

            diarize_num_clusters: -1,
            diarize_min_duration_on: 0.3,
            diarize_min_duration_off: 0.5,
            asr_decoding: None,
            asr_max_active_paths: None,
            asr_blank_penalty: None,
            asr_engine: "sherpa".to_string(),
            ggml_model: None,
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

    /// Load the models + recognizer once and return a [`LiveEngine`] for driving chunked/live transcription
    /// (ADR 0009). The far-end diarization models are **not** required here — the live path attributes the
    /// far end to a single `Them`, like the batch default — so only the ASR engine + Silero VAD must be
    /// present. Spawn one [`LiveTranscriber`] per channel via [`LiveEngine::channel`].
    pub fn live_engine(&self) -> Result<LiveEngine> {
        let dir = models::resolve_dir(self.cfg.model_dir.clone())?;
        let wants_ggml = asr::wants_ggml(&self.cfg.asr_engine)?;
        let m = models::discover_for(&dir, !wants_ggml, false, &self.cfg.embedding_model)?;
        let provider = resolve_provider(self.cfg.provider.as_str()).to_string();
        let asr = self.build_asr(wants_ggml, &m, &provider, &dir)?;
        Ok(LiveEngine::new(
            asr,
            m,
            provider,
            self.cfg.vad_threshold,
            self.cfg.vad_min_silence,
        ))
    }

    /// Build the runtime-selected ASR engine (the only engine-specific seam — see [`Asr`]). `wants_ggml`
    /// comes from [`asr::wants_ggml`] so an unknown token has already errored by the time this runs.
    fn build_asr(
        &self,
        wants_ggml: bool,
        m: &models::Models,
        provider: &str,
        model_dir: &Path,
    ) -> Result<Asr> {
        if !wants_ggml {
            return Ok(Asr::Sherpa(engine::build_recognizer(
                m,
                provider,
                self.cfg.num_threads,
                self.cfg.asr_decoding.as_deref(),
                self.cfg.asr_max_active_paths,
                self.cfg.asr_blank_penalty,
            )?));
        }
        self.build_ggml_asr(model_dir)
    }

    #[cfg(feature = "ggml")]
    fn build_ggml_asr(&self, model_dir: &Path) -> Result<Asr> {
        let gguf = ggml::resolve_gguf(self.cfg.ggml_model.clone(), model_dir)?;
        Ok(Asr::Ggml(ggml::GgmlAsr::load(&gguf)?))
    }

    #[cfg(not(feature = "ggml"))]
    fn build_ggml_asr(&self, _model_dir: &Path) -> Result<Asr> {
        anyhow::bail!(
            "ASR engine `ggml` is not compiled into this build — rebuild with the `ggml` feature of \
             corti-transcribe-local (ADR 0011)"
        )
    }
}

impl Transcriber for LocalTranscriber {
    fn transcribe(&self, audio: &Path, _meta: &RecordingMeta) -> Result<DiarizedTranscript> {
        let job_started = std::time::Instant::now();
        tracing::info!(
            target: "corti::transcribe::local",
            path = %audio.display(),
            "local transcription started"
        );
        let dir = models::resolve_dir(self.cfg.model_dir.clone())?;
        let wants_ggml = asr::wants_ggml(&self.cfg.asr_engine)?;
        let m = models::discover_for(
            &dir,
            !wants_ggml,
            self.cfg.diarize_far_end,
            &self.cfg.embedding_model,
        )?;
        let track = audio::read_two_track(audio)?;
        let provider = resolve_provider(self.cfg.provider.as_str());
        let threads = self.cfg.num_threads;

        // One ASR engine load per job, shared across both channels (and both `LiveTranscriber`s).
        let rec = Arc::new(self.build_asr(wants_ggml, &m, provider, &dir)?);
        let mut segments = Vec::new();

        // ch0 (mic) → Me. Channel = speaker; no diarizer needed.
        if !track.mic.is_empty() {
            let mic = engine::resample_to_16k(&track.mic, track.sample_rate)?;
            let vad = engine::build_vad(
                &m,
                provider,
                self.cfg.vad_threshold,
                self.cfg.vad_min_silence,
            )?;
            let words = engine::transcribe_channel(rec.clone(), vad, &mic);
            segments.extend(words_to_segments(&words, Speaker::Me, SEGMENT_GAP));
        }

        // ch1 (system tap) → far end.
        if !track.them.is_empty() {
            let them = engine::resample_to_16k(&track.them, track.sample_rate)?;
            let vad = engine::build_vad(
                &m,
                provider,
                self.cfg.vad_threshold,
                self.cfg.vad_min_silence,
            )?;
            let words = engine::transcribe_channel(rec.clone(), vad, &them);
            if self.cfg.diarize_far_end {
                // Opt-in: split the far end into per-speaker labels (Them 1/2/…).
                let diar = engine::build_diarizer(
                    &m,
                    provider,
                    threads,
                    self.cfg.diarize_threshold,
                    self.cfg.diarize_num_clusters,
                    self.cfg.diarize_min_duration_on,
                    self.cfg.diarize_min_duration_off,
                )?;
                let turns = engine::diarize_channel(&diar, &them);
                segments.extend(diarize_words(&words, &turns, SEGMENT_GAP, "Them"));
            } else {
                // Default: attribute the whole far end to a single speaker (like the AWS backend).
                let them_speaker = Speaker::Other("Them".to_string());
                segments.extend(words_to_segments(&words, them_speaker, SEGMENT_GAP));
            }
        }

        let transcript = DiarizedTranscript::new(merge_by_time(segments));
        tracing::info!(
            target: "corti::transcribe::local",
            elapsed_ms = job_started.elapsed().as_millis() as u64,
            segments = transcript.segments.len(),
            "local transcription finished"
        );
        Ok(transcript)
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
        tracing::warn!(
            target: "corti::transcribe::local",
            requested,
            "provider unavailable in this build — running on CPU (build with the `coreml-lib` feature + a CoreML-enabled sherpa-onnx lib to enable it)"
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
