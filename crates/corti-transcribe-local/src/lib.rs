//! Local on-device backend for corti — fully offline transcription on Apple Silicon.
//!
//! ASR model: NVIDIA Parakeet-TDT-0.6B-v3 through selectable [`Asr`] runtimes — sherpa/ONNX on CPU
//! (compatibility default) or transcribe.cpp/GGML on Metal. sherpa also supplies Silero VAD and optional
//! far-end diarization. corti records a 2-track WAV (ch0 = me/mic, ch1 = them/system-tap), so diarization for the
//! me-vs-them split is just the channel: ch0 → [`Speaker::Me`], ch1 → `Speaker::Other("Them")`. The
//! far-end channel can **optionally** be diarized into `Them 1/2/…` (pyannote-segmentation-3.0 + a
//! runtime-selectable English speaker-embedding model, both ONNX) when [`LocalConfig::diarize_far_end`] is
//! set — off by default. Over-clustering on English audio is tracked as issue #18; the embedding model and a
//! [`LocalConfig::diarize_threshold`] knob are tunable to address it.
//!
//! Pipeline per channel: resample to 16 kHz → Silero VAD into speech regions → selected Parakeet ASR per
//! region → map timestamped words; far-end words are attributed to diarization turns. Words are
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

/// Where the local models live and how to run them. Built by the app from its config.
#[derive(Debug, Clone)]
pub struct LocalConfig {
    /// Directory holding the model files (Parakeet, pyannote segmentation, embedding, VAD).
    /// `None` ⇒ the default cache (`~/Library/Caches/corti/models/`), resolved by the backend.
    pub model_dir: Option<PathBuf>,
    /// Local inference threads: ONNX intra-op threads and transcribe.cpp session threads. Small by default
    /// (a short batch job → favours battery on the M1 Pro).
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

/// Local offline transcriber (Parakeet-TDT via the selected ASR runtime). Models load lazily on `transcribe`.
#[derive(Debug, Clone)]
pub struct LocalTranscriber {
    cfg: LocalConfig,
}

impl LocalTranscriber {
    pub fn new(cfg: LocalConfig) -> Self {
        Self { cfg }
    }

    /// Validate only the files required by the selected ASR engine, VAD, and optional diarizer without
    /// loading any model. The app's live-capture eligibility check uses this cheap path so GGML sessions do
    /// not incorrectly require the legacy Parakeet ONNX set (and unsupported engine builds fail clearly).
    pub fn validate_models(&self) -> Result<()> {
        let dir = models::resolve_dir(self.cfg.model_dir.clone())?;
        let wants_ggml = asr::wants_ggml(&self.cfg.asr_engine)?;
        models::discover_for(
            &dir,
            !wants_ggml,
            self.cfg.diarize_far_end,
            &self.cfg.embedding_model,
        )?;
        if wants_ggml {
            self.validate_ggml_model(&dir)?;
        }
        Ok(())
    }

    #[cfg(feature = "ggml")]
    fn validate_ggml_model(&self, model_dir: &Path) -> Result<()> {
        ggml::resolve_gguf(self.cfg.ggml_model.clone(), model_dir).map(|_| ())
    }

    #[cfg(not(feature = "ggml"))]
    fn validate_ggml_model(&self, _model_dir: &Path) -> Result<()> {
        anyhow::bail!(
            "ASR engine `ggml` is not compiled into this build — rebuild with the `ggml` feature of \
             corti-transcribe-local (ADR 0011)"
        )
    }

    /// Load the models + recognizer once and return a [`LiveEngine`] for driving chunked/live transcription
    /// (ADR 0009). When `diarize_far_end` is enabled, the same engine also loads one reusable diarizer so a
    /// caller can diarize bounded rolling windows before durably filing them; whole-call audio is never
    /// required. The selected ASR engine and Silero VAD are always required. Spawn one
    /// [`LiveTranscriber`] per channel via [`LiveEngine::channel`].
    pub fn live_engine(&self) -> Result<LiveEngine> {
        #[cfg(feature = "offline-tracing")]
        let span = tracing::span!(
            target: "vasovagal::trace",
            tracing::Level::INFO,
            "corti.transcription.model_load",
            backend = "local",
            engine = trace_engine(&self.cfg.asr_engine),
            model_family = "speech_to_text",
            outcome = tracing::field::Empty,
            error_code = tracing::field::Empty,
        );
        #[cfg(feature = "offline-tracing")]
        let result = span.in_scope(|| self.live_engine_inner());
        #[cfg(not(feature = "offline-tracing"))]
        let result = self.live_engine_inner();
        #[cfg(feature = "offline-tracing")]
        record_result(&span, &result, "model_unavailable");
        result
    }

    fn live_engine_inner(&self) -> Result<LiveEngine> {
        let dir = models::resolve_dir(self.cfg.model_dir.clone())?;
        let wants_ggml = asr::wants_ggml(&self.cfg.asr_engine)?;
        let m = models::discover_for(
            &dir,
            !wants_ggml,
            self.cfg.diarize_far_end,
            &self.cfg.embedding_model,
        )?;
        let asr = self.build_asr(wants_ggml, &m, &dir)?;
        let diarizer = self
            .cfg
            .diarize_far_end
            .then(|| {
                engine::build_diarizer(
                    &m,
                    self.cfg.num_threads,
                    self.cfg.diarize_threshold,
                    self.cfg.diarize_num_clusters,
                    self.cfg.diarize_min_duration_on,
                    self.cfg.diarize_min_duration_off,
                )
            })
            .transpose()?;
        Ok(LiveEngine::new(
            asr,
            m,
            self.cfg.vad_threshold,
            self.cfg.vad_min_silence,
            diarizer,
        ))
    }

    /// Build the runtime-selected ASR engine (the only engine-specific seam — see [`Asr`]). `wants_ggml`
    /// comes from [`asr::wants_ggml`] so an unknown token has already errored by the time this runs.
    fn build_asr(&self, wants_ggml: bool, m: &models::Models, model_dir: &Path) -> Result<Asr> {
        if !wants_ggml {
            return Ok(Asr::Sherpa(engine::build_recognizer(
                m,
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
        Ok(Asr::Ggml(ggml::GgmlAsr::load(&gguf, self.cfg.num_threads)?))
    }

    #[cfg(not(feature = "ggml"))]
    fn build_ggml_asr(&self, _model_dir: &Path) -> Result<Asr> {
        anyhow::bail!(
            "ASR engine `ggml` is not compiled into this build — rebuild with the `ggml` feature of \
             corti-transcribe-local (ADR 0011)"
        )
    }
}

#[cfg(feature = "offline-tracing")]
fn trace_engine(configured: &str) -> &'static str {
    match configured {
        "" | "sherpa" => "onnx",
        _ => "other",
    }
}

#[cfg(feature = "offline-tracing")]
fn record_result<T>(span: &tracing::Span, result: &Result<T>, error_code: &'static str) {
    if result.is_ok() {
        span.record("outcome", "ok");
    } else {
        span.record("outcome", "error");
        span.record("error_code", error_code);
    }
}

impl LocalTranscriber {
    fn transcribe_inner(&self, audio: &Path) -> Result<DiarizedTranscript> {
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
        if wants_ggml {
            // Fail before decoding a call-sized WAV when the selected GGUF/build is unavailable.
            self.validate_ggml_model(&dir)?;
        }

        #[cfg(feature = "offline-tracing")]
        let decode_span = tracing::span!(
            target: "vasovagal::trace",
            tracing::Level::INFO,
            "corti.transcription.decode",
            backend = "local",
            engine = trace_engine(&self.cfg.asr_engine),
            model_family = "speech_to_text",
            sample_rate = tracing::field::Empty,
            channel_count = tracing::field::Empty,
            sample_count = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error_code = tracing::field::Empty,
        );
        #[cfg(feature = "offline-tracing")]
        let decoded = decode_span.in_scope(|| audio::read_two_track(audio));
        #[cfg(not(feature = "offline-tracing"))]
        let decoded = audio::read_two_track(audio);
        #[cfg(feature = "offline-tracing")]
        {
            record_result(&decode_span, &decoded, "decode_failed");
            if let Ok(track) = &decoded {
                decode_span.record("sample_rate", u64::try_from(track.sample_rate).unwrap_or(0));
                decode_span.record(
                    "channel_count",
                    u64::from(!track.mic.is_empty()) + u64::from(!track.them.is_empty()),
                );
                decode_span.record(
                    "sample_count",
                    u64::try_from(track.mic.len().saturating_add(track.them.len()))
                        .unwrap_or(u64::MAX),
                );
            }
        }
        let track = decoded?;
        let threads = self.cfg.num_threads;

        // One ASR engine load per job, shared across both channels (and both `LiveTranscriber`s).
        #[cfg(feature = "offline-tracing")]
        let model_span = tracing::span!(
            target: "vasovagal::trace",
            tracing::Level::INFO,
            "corti.transcription.model_load",
            backend = "local",
            engine = trace_engine(&self.cfg.asr_engine),
            model_family = "speech_to_text",
            outcome = tracing::field::Empty,
            error_code = tracing::field::Empty,
        );
        #[cfg(feature = "offline-tracing")]
        let model = model_span.in_scope(|| self.build_asr(wants_ggml, &m, &dir));
        #[cfg(not(feature = "offline-tracing"))]
        let model = self.build_asr(wants_ggml, &m, &dir);
        #[cfg(feature = "offline-tracing")]
        record_result(&model_span, &model, "model_unavailable");
        let rec = Arc::new(model?);
        let mut segments = Vec::new();

        // ch0 (mic) → Me. Channel = speaker; no diarizer needed.
        if !track.mic.is_empty() {
            #[cfg(feature = "offline-tracing")]
            let channel_span = tracing::span!(
                target: "vasovagal::trace",
                tracing::Level::INFO,
                "corti.transcription.channel",
                backend = "local",
                engine = trace_engine(&self.cfg.asr_engine),
                model_family = "speech_to_text",
                sample_rate = u64::try_from(track.sample_rate).unwrap_or(0),
                sample_count = u64::try_from(track.mic.len()).unwrap_or(u64::MAX),
                item_count = tracing::field::Empty,
                outcome = tracing::field::Empty,
                error_code = tracing::field::Empty,
            );
            let run = || -> Result<Vec<corti_transcribe::segment::Word>> {
                let mic = engine::resample_to_16k(&track.mic, track.sample_rate)?;
                let vad = engine::build_vad(&m, self.cfg.vad_threshold, self.cfg.vad_min_silence)?;
                Ok(engine::transcribe_channel(rec.clone(), vad, &mic))
            };
            #[cfg(feature = "offline-tracing")]
            let words = channel_span.in_scope(run);
            #[cfg(not(feature = "offline-tracing"))]
            let words = run();
            #[cfg(feature = "offline-tracing")]
            {
                record_result(&channel_span, &words, "decode_failed");
                if let Ok(words) = &words {
                    channel_span
                        .record("item_count", u64::try_from(words.len()).unwrap_or(u64::MAX));
                }
            }
            segments.extend(words_to_segments(&words?, Speaker::Me, SEGMENT_GAP));
        }

        // ch1 (system tap) → far end.
        if !track.them.is_empty() {
            #[cfg(feature = "offline-tracing")]
            let channel_span = tracing::span!(
                target: "vasovagal::trace",
                tracing::Level::INFO,
                "corti.transcription.channel",
                backend = "local",
                engine = trace_engine(&self.cfg.asr_engine),
                model_family = "speech_to_text",
                sample_rate = u64::try_from(track.sample_rate).unwrap_or(0),
                sample_count = u64::try_from(track.them.len()).unwrap_or(u64::MAX),
                item_count = tracing::field::Empty,
                outcome = tracing::field::Empty,
                error_code = tracing::field::Empty,
            );
            let run = || -> Result<(Vec<f32>, Vec<corti_transcribe::segment::Word>)> {
                let them = engine::resample_to_16k(&track.them, track.sample_rate)?;
                let vad = engine::build_vad(&m, self.cfg.vad_threshold, self.cfg.vad_min_silence)?;
                let words = engine::transcribe_channel(rec.clone(), vad, &them);
                Ok((them, words))
            };
            #[cfg(feature = "offline-tracing")]
            let decoded = channel_span.in_scope(run);
            #[cfg(not(feature = "offline-tracing"))]
            let decoded = run();
            #[cfg(feature = "offline-tracing")]
            {
                record_result(&channel_span, &decoded, "decode_failed");
                if let Ok((_, words)) = &decoded {
                    channel_span
                        .record("item_count", u64::try_from(words.len()).unwrap_or(u64::MAX));
                }
            }
            let (them, words) = decoded?;
            if self.cfg.diarize_far_end {
                // Opt-in: split the far end into per-speaker labels (Them 1/2/…).
                #[cfg(feature = "offline-tracing")]
                let diarize_span = tracing::span!(
                    target: "vasovagal::trace",
                    tracing::Level::INFO,
                    "corti.transcription.diarize",
                    backend = "local",
                    engine = "onnx",
                    model_family = "diarization",
                    sample_rate = 16_000_u64,
                    sample_count = u64::try_from(them.len()).unwrap_or(u64::MAX),
                    item_count = tracing::field::Empty,
                    outcome = tracing::field::Empty,
                    error_code = tracing::field::Empty,
                );
                let run = || -> Result<Vec<corti_transcribe::segment::SpeakerTurn>> {
                    let diar = engine::build_diarizer(
                        &m,
                        threads,
                        self.cfg.diarize_threshold,
                        self.cfg.diarize_num_clusters,
                        self.cfg.diarize_min_duration_on,
                        self.cfg.diarize_min_duration_off,
                    )?;
                    Ok(engine::diarize_channel(&diar, &them))
                };
                #[cfg(feature = "offline-tracing")]
                let turns = diarize_span.in_scope(run);
                #[cfg(not(feature = "offline-tracing"))]
                let turns = run();
                #[cfg(feature = "offline-tracing")]
                {
                    record_result(&diarize_span, &turns, "decode_failed");
                    if let Ok(turns) = &turns {
                        diarize_span
                            .record("item_count", u64::try_from(turns.len()).unwrap_or(u64::MAX));
                    }
                }
                segments.extend(diarize_words(&words, &turns?, SEGMENT_GAP, "Them"));
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

impl Transcriber for LocalTranscriber {
    fn transcribe(&self, audio: &Path, _meta: &RecordingMeta) -> Result<DiarizedTranscript> {
        #[cfg(feature = "offline-tracing")]
        let span = tracing::span!(
            target: "vasovagal::trace",
            tracing::Level::INFO,
            "corti.transcription.backend",
            backend = "local",
            engine = trace_engine(&self.cfg.asr_engine),
            model_family = "speech_to_text",
            item_count = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error_code = tracing::field::Empty,
        );
        #[cfg(feature = "offline-tracing")]
        let result = span.in_scope(|| self.transcribe_inner(audio));
        #[cfg(not(feature = "offline-tracing"))]
        let result = self.transcribe_inner(audio);
        #[cfg(feature = "offline-tracing")]
        {
            record_result(&span, &result, "other");
            if let Ok(transcript) = &result {
                span.record(
                    "item_count",
                    u64::try_from(transcript.segments.len()).unwrap_or(u64::MAX),
                );
            }
        }
        result
    }
}

#[cfg(all(test, feature = "ggml"))]
mod tests {
    use super::{LocalConfig, LocalTranscriber, models};

    #[test]
    fn ggml_file_validation_does_not_require_onnx_parakeet() {
        let dir = std::env::temp_dir().join(format!(
            "corti-ggml-model-validation-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(models::VAD_FILE), []).unwrap();
        std::fs::write(dir.join(models::GGML_FILE), []).unwrap();

        let ggml = LocalTranscriber::new(LocalConfig {
            model_dir: Some(dir.clone()),
            asr_engine: models::GGML_ASR_ENGINE.into(),
            ..LocalConfig::default()
        });
        ggml.validate_models().unwrap();

        let sherpa = LocalTranscriber::new(LocalConfig {
            model_dir: Some(dir.clone()),
            asr_engine: models::SHERPA_ASR_ENGINE.into(),
            ..LocalConfig::default()
        });
        assert!(sherpa.validate_models().is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    #[ignore = "needs the real GGUF/VAD/diarization models; set CORTI_VERIFY_MODEL_DIR"]
    fn ggml_live_engine_coexists_with_sherpa_diarizer() {
        let dir = std::path::PathBuf::from(
            std::env::var("CORTI_VERIFY_MODEL_DIR")
                .expect("set CORTI_VERIFY_MODEL_DIR to the model cache dir"),
        );
        let engine = LocalTranscriber::new(LocalConfig {
            model_dir: Some(dir),
            asr_engine: models::GGML_ASR_ENGINE.into(),
            diarize_far_end: true,
            ..LocalConfig::default()
        })
        .live_engine()
        .expect("load GGML ASR + sherpa VAD/diarizer");
        assert!(engine.diarizes_far_end());
        engine.channel().expect("spawn channel sharing GGML ASR");
    }
}
