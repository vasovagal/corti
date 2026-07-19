//! The interchangeable per-region ASR stage (ADR 0011).
//!
//! Everything around the ASR decode — 16 kHz resampling, Silero VAD chunking, pyannote far-end
//! diarization, word→segment shaping — is engine-agnostic and stays on sherpa-onnx. Only the decode of a
//! single VAD speech region is behind this enum, so switching engines isolates exactly the ASR runtime
//! (ONNX Runtime CPU vs GGML/Metal) for an apples-to-apples benchmark.

use corti_transcribe::segment::Word;
use sherpa_onnx::OfflineRecognizer;

use crate::engine;

/// One loaded ASR engine, shared across channels via `Arc` (both variants are `Send + Sync`).
pub enum Asr {
    /// Parakeet-TDT-0.6B-v3 int8 via sherpa-onnx / ONNX Runtime — the shipping engine (ADR 0003).
    Sherpa(OfflineRecognizer),
    /// The same Parakeet-TDT-0.6B-v3 as a GGUF via transcribe.cpp (GGML; Metal on Apple Silicon) — the
    /// ADR 0011 spike engine, compiled in only with the `ggml` feature.
    #[cfg(feature = "ggml")]
    Ggml(crate::ggml::GgmlAsr),
}

impl Asr {
    /// Decode one 16 kHz mono VAD speech region and lift its word timestamps to absolute time.
    /// Engine trouble on a region yields no words (logged), never an error — one bad region must not
    /// sink the whole job.
    pub(crate) fn asr_segment(&self, samples_16k: &[f32], offset_sec: f64) -> Vec<Word> {
        match self {
            Asr::Sherpa(rec) => engine::asr_segment(rec, samples_16k, offset_sec),
            #[cfg(feature = "ggml")]
            Asr::Ggml(g) => g.asr_segment(samples_16k, offset_sec),
        }
    }
}

/// Parse the [`crate::LocalConfig::asr_engine`] token. Empty means "the default" (sherpa) so an unset
/// config value never errors; an unknown token is a hard error rather than a silent fallback — in a
/// benchmark a silent fallback would mislabel every result.
pub(crate) fn wants_ggml(engine: &str) -> anyhow::Result<bool> {
    match engine {
        "" | "sherpa" => Ok(false),
        "ggml" => Ok(true),
        other => anyhow::bail!("unknown ASR engine `{other}` (expected `sherpa` or `ggml`)"),
    }
}

#[cfg(test)]
mod tests {
    use super::wants_ggml;

    #[test]
    fn engine_tokens_parse() {
        assert!(!wants_ggml("").unwrap());
        assert!(!wants_ggml("sherpa").unwrap());
        assert!(wants_ggml("ggml").unwrap());
        assert!(wants_ggml("whisper").is_err());
    }
}
