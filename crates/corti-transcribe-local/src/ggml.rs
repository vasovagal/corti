//! transcribe.cpp (GGML) ASR stage — the ADR 0011 spike engine.
//!
//! Runs the same NVIDIA Parakeet-TDT-0.6B-v3 the sherpa engine uses, but as a single GGUF file through
//! the `transcribe-cpp` crate (GGML runtime; Metal is enabled automatically on Apple Silicon — the GPU
//! path ADR 0003 could not get from ONNX Runtime's CoreML EP). Only the per-region decode lives here:
//! resampling, Silero VAD and pyannote diarization stay on sherpa-onnx, so a bench run compares exactly
//! the ASR runtime.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use corti_transcribe::segment::Word;
use transcribe_cpp::{Model, RunOptions, Session, TimestampKind, Transcript};

/// Default GGUF filename expected under the model dir. Q8_0 is the tier closest to the shipping sherpa
/// int8 ONNX (upstream WER 1.94% vs the 1.95% F32 reference on LibriSpeech test-clean).
pub const DEFAULT_GGUF_FILE: &str = "parakeet-tdt-0.6b-v3-Q8_0.gguf";
/// Where to fetch [`DEFAULT_GGUF_FILE`] (the transcribe.cpp author's official GGUF port).
pub const DEFAULT_GGUF_URL: &str = "https://huggingface.co/handy-computer/parakeet-tdt-0.6b-v3-gguf/resolve/main/parakeet-tdt-0.6b-v3-Q8_0.gguf";

/// Resolve the GGUF to load: the explicit override, else `<model_dir>/`[`DEFAULT_GGUF_FILE`], failing
/// with the one-line download command when the file is missing (mirrors `models::discover`'s
/// actionable-error contract; spike-era manual fetch, no `fetch-models.sh` entry yet).
pub fn resolve_gguf(override_path: Option<PathBuf>, model_dir: &Path) -> Result<PathBuf> {
    let path = override_path.unwrap_or_else(|| model_dir.join(DEFAULT_GGUF_FILE));
    if !path.exists() {
        anyhow::bail!(
            "ggml ASR model not found at {}\n\nDownload it once with:\n  curl -L --create-dirs -o {} \\\n    {}",
            path.display(),
            path.display(),
            DEFAULT_GGUF_URL,
        );
    }
    Ok(path)
}

/// A loaded transcribe.cpp model with one decode session, shareable across channels.
///
/// `Session::run` takes `&mut self`, so the session sits behind a `Mutex`; that also supplies the `Sync`
/// the `Arc<Asr>` sharing in the live path needs (`Session` is `Send` but not `Sync`). The lock costs
/// nothing extra: transcribe.cpp serializes compute per model internally anyway.
pub struct GgmlAsr {
    session: Mutex<Session>,
}

impl GgmlAsr {
    /// Load the GGUF and open a session. Logged like the sherpa `model loaded` lines (issue #76),
    /// with the compute backend GGML actually picked (`Metal` / `CPU`) — the load-bearing fact for the
    /// ADR 0011 benchmark.
    pub fn load(gguf: &Path) -> Result<Self> {
        let started = std::time::Instant::now();
        let model =
            Model::load(gguf).with_context(|| format!("loading GGUF model {}", gguf.display()))?;
        let session = model
            .session()
            .context("opening a transcribe.cpp session")?;
        tracing::info!(
            target: "corti::transcribe::local",
            model = "parakeet-ggml",
            path = %gguf.display(),
            backend = %model.backend(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "model loaded"
        );
        Ok(Self {
            session: Mutex::new(session),
        })
    }

    /// Decode one 16 kHz mono VAD region; word times are lifted by `offset_sec` to absolute call time.
    /// Mirrors the sherpa contract: a failed region decode yields no words (warned), never an error.
    pub(crate) fn asr_segment(&self, samples_16k: &[f32], offset_sec: f64) -> Vec<Word> {
        let opts = RunOptions {
            timestamps: TimestampKind::Word,
            ..RunOptions::default()
        };
        let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
        match session.run(samples_16k, &opts) {
            Ok(t) => words_from(&t, offset_sec),
            Err(e) => {
                tracing::warn!(
                    target: "corti::transcribe::local",
                    error = %e,
                    "ggml region decode failed — dropping the region"
                );
                Vec::new()
            }
        }
    }
}

/// Map a transcribe.cpp [`Transcript`] (times in ms, region-relative) to corti [`Word`]s (seconds,
/// absolute). Prefers word rows; degrades to one `Word` per segment, then to one `Word` for the bare
/// text — coarser times, but transcribed speech is never silently dropped just because a family/build
/// produced no fine alignment.
fn words_from(t: &Transcript, offset_sec: f64) -> Vec<Word> {
    let word = |t0_ms: i64, t1_ms: i64, text: &str| {
        let text = text.trim();
        (!text.is_empty()).then(|| Word {
            start: offset_sec + t0_ms as f64 / 1000.0,
            end: offset_sec + t1_ms as f64 / 1000.0,
            text: text.to_string(),
        })
    };
    if !t.words.is_empty() {
        return t
            .words
            .iter()
            .filter_map(|w| word(w.t0_ms, w.t1_ms, &w.text))
            .collect();
    }
    if !t.segments.is_empty() {
        return t
            .segments
            .iter()
            .filter_map(|s| word(s.t0_ms, s.t1_ms, &s.text))
            .collect();
    }
    word(0, 0, &t.text).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_map_with_offset() {
        let t = Transcript {
            text: "hello world".into(),
            words: vec![
                transcribe_cpp::Word {
                    t0_ms: 0,
                    t1_ms: 400,
                    text: " hello".into(),
                    ..Default::default()
                },
                transcribe_cpp::Word {
                    t0_ms: 500,
                    t1_ms: 900,
                    text: "world".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let words = words_from(&t, 10.0);
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "hello"); // trimmed
        assert!((words[0].start - 10.0).abs() < 1e-9);
        assert!((words[0].end - 10.4).abs() < 1e-9);
        assert!((words[1].start - 10.5).abs() < 1e-9);
    }

    #[test]
    fn falls_back_to_segments_then_text() {
        // No word rows → one Word per segment.
        let t = Transcript {
            text: "a b".into(),
            segments: vec![transcribe_cpp::Segment {
                t0_ms: 1000,
                t1_ms: 2000,
                text: "a b".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let words = words_from(&t, 5.0);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "a b");
        assert!((words[0].start - 6.0).abs() < 1e-9);

        // No rows at all → the bare text at the region start.
        let t = Transcript {
            text: " just text ".into(),
            ..Default::default()
        };
        let words = words_from(&t, 3.0);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "just text");
        assert!((words[0].start - 3.0).abs() < 1e-9);

        // Nothing anywhere → no words.
        assert!(words_from(&Transcript::default(), 0.0).is_empty());
    }
}
