//! The sherpa-onnx (ONNX Runtime) pipeline: build the Parakeet recognizer, Silero VAD and pyannote
//! diarizer, then turn a mono 16 kHz channel into timestamped [`Word`]s (VAD-chunked ASR) or far-end
//! speaker turns (diarization).
//!
//! Sample-rate handling (verified against the crate): the **ASR** feature extractor resamples internally,
//! but **VAD and diarization do not** — so we resample each channel to 16 kHz once (via sherpa's
//! `LinearResampler`, a quality windowed-sinc) and feed 16 kHz everywhere. VAD-chunking also sidesteps
//! Parakeet's ~30 s offline clip limit and the empty-output bug on silence (#2258), since only speech
//! regions reach the recognizer.

use std::path::Path;

use anyhow::{Context, Result};
use corti_transcribe::segment::{SpeakerTurn, Word};
use sherpa_onnx::{
    FastClusteringConfig, LinearResampler, OfflineRecognizer, OfflineRecognizerConfig,
    OfflineSpeakerDiarization, OfflineSpeakerDiarizationConfig,
    OfflineSpeakerSegmentationModelConfig, OfflineSpeakerSegmentationPyannoteModelConfig,
    OfflineTransducerModelConfig, SileroVadModelConfig, SpeakerEmbeddingExtractorConfig,
    VadModelConfig, VoiceActivityDetector,
};

use crate::models::Models;

/// All ONNX models run at 16 kHz mono.
const TARGET_RATE: i32 = 16_000;
/// Silero VAD window (samples at 16 kHz). 512 is the value Silero v4/v5 expect.
const VAD_WINDOW: usize = 512;
/// Cap a single speech segment so ASR stays under Parakeet's offline clip limit (~30 s).
const MAX_SPEECH_SECONDS: f32 = 20.0;

/// Resample a mono channel to 16 kHz (no-op if already there). Uses sherpa's `LinearResampler`.
pub fn resample_to_16k(samples: &[f32], from_hz: i32) -> Result<Vec<f32>> {
    if from_hz == TARGET_RATE || samples.is_empty() {
        return Ok(samples.to_vec());
    }
    let resampler = LinearResampler::create(from_hz, TARGET_RATE)
        .with_context(|| format!("creating resampler {from_hz} → {TARGET_RATE} Hz"))?;
    Ok(resampler.resample(samples, true))
}

/// Build the Parakeet-TDT offline recognizer (CPU by default; `coreml` opt-in via `provider`).
pub fn build_recognizer(m: &Models, provider: &str, num_threads: i32) -> Result<OfflineRecognizer> {
    let mut config = OfflineRecognizerConfig::default();
    config.model_config.transducer = OfflineTransducerModelConfig {
        encoder: Some(path_str(&m.parakeet_encoder)),
        decoder: Some(path_str(&m.parakeet_decoder)),
        joiner: Some(path_str(&m.parakeet_joiner)),
    };
    config.model_config.tokens = Some(path_str(&m.parakeet_tokens));
    config.model_config.model_type = Some("nemo_transducer".to_string());
    config.model_config.provider = Some(provider.to_string());
    config.model_config.num_threads = num_threads;
    OfflineRecognizer::create(&config)
        .context("failed to create the Parakeet recognizer (check model files / provider)")
}

/// Build a fresh Silero VAD (stateful — one per channel).
pub fn build_vad(m: &Models, provider: &str) -> Result<VoiceActivityDetector> {
    let silero = SileroVadModelConfig {
        model: Some(path_str(&m.vad)),
        threshold: 0.5,
        min_silence_duration: 0.25,
        min_speech_duration: 0.25,
        window_size: VAD_WINDOW as i32,
        max_speech_duration: MAX_SPEECH_SECONDS,
    };
    let config = VadModelConfig {
        silero_vad: silero,
        ten_vad: Default::default(),
        sample_rate: TARGET_RATE,
        num_threads: 1,
        provider: Some(provider.to_string()),
        debug: false,
    };
    // Buffer up to MAX_SPEECH_SECONDS of audio internally.
    VoiceActivityDetector::create(&config, MAX_SPEECH_SECONDS)
        .context("failed to create the Silero VAD (check model file)")
}

/// Build the pyannote-segmentation + speaker-embedding diarizer (for splitting the far-end channel). The
/// embedding model is whichever English model `m.embedding` resolved to (see `models::EMBEDDING_IDS`).
/// `threshold` is the clustering cutoff that estimates the speaker count — higher merges more (fewer
/// speakers), lower splits more; tunable via `CORTI_LOCAL_DIARIZE_THRESHOLD` to address over-clustering (#18).
pub fn build_diarizer(
    m: &Models,
    provider: &str,
    num_threads: i32,
    threshold: f32,
) -> Result<OfflineSpeakerDiarization> {
    let config = OfflineSpeakerDiarizationConfig {
        segmentation: OfflineSpeakerSegmentationModelConfig {
            pyannote: OfflineSpeakerSegmentationPyannoteModelConfig {
                model: Some(path_str(&m.segmentation)),
            },
            num_threads,
            debug: false,
            provider: Some(provider.to_string()),
        },
        embedding: SpeakerEmbeddingExtractorConfig {
            model: Some(path_str(&m.embedding)),
            num_threads,
            debug: false,
            provider: Some(provider.to_string()),
        },
        // Unknown number of far-end speakers → estimate via the clustering threshold (num_clusters < 0).
        clustering: FastClusteringConfig {
            num_clusters: -1,
            threshold,
        },
        min_duration_on: 0.3,
        min_duration_off: 0.5,
    };
    OfflineSpeakerDiarization::create(&config)
        .context("failed to create speaker diarization (check segmentation/embedding models)")
}

/// Transcribe one 16 kHz mono channel: Silero VAD chunks it into speech regions, each region is decoded by
/// the recognizer, and token timestamps are offset to absolute time and reassembled into [`Word`]s.
pub fn transcribe_channel(
    rec: &OfflineRecognizer,
    vad: &VoiceActivityDetector,
    samples_16k: &[f32],
) -> Vec<Word> {
    let mut words = Vec::new();
    let drain = |vad: &VoiceActivityDetector, words: &mut Vec<Word>| {
        while let Some(seg) = vad.front() {
            let offset = seg.start() as f64 / TARGET_RATE as f64;
            words.extend(asr_segment(rec, seg.samples(), offset));
            vad.pop();
        }
    };

    for chunk in samples_16k.chunks(VAD_WINDOW) {
        vad.accept_waveform(chunk);
        drain(vad, &mut words);
    }
    vad.flush();
    drain(vad, &mut words);
    words
}

/// Run far-end diarization on a 16 kHz mono channel → time-ordered speaker turns labelled `Them N`.
pub fn diarize_channel(diar: &OfflineSpeakerDiarization, samples_16k: &[f32]) -> Vec<SpeakerTurn> {
    let Some(result) = diar.process(samples_16k) else {
        return Vec::new();
    };
    result
        .sort_by_start_time()
        .into_iter()
        .map(|s| SpeakerTurn {
            start: s.start as f64,
            end: s.end as f64,
            label: format!("Them {}", s.speaker + 1),
        })
        .collect()
}

/// Decode one VAD speech region and lift its token timestamps to absolute time.
fn asr_segment(rec: &OfflineRecognizer, samples_16k: &[f32], offset_sec: f64) -> Vec<Word> {
    let stream = rec.create_stream();
    stream.accept_waveform(TARGET_RATE, samples_16k);
    rec.decode(&stream);
    match stream.get_result() {
        Some(r) => tokens_to_words(
            &r.tokens,
            r.timestamps.as_deref(),
            r.durations.as_deref(),
            offset_sec,
        ),
        None => Vec::new(),
    }
}

/// Reassemble Parakeet sentencepiece tokens into whole [`Word`]s. A new word starts at a token prefixed
/// with the sentencepiece word-boundary marker `▁` (U+2581); the marker becomes a space. Token timestamps
/// (relative to the segment) are shifted by `offset_sec`; a word's end is the last token's start + its
/// duration. Pure (no sherpa types) so it is unit-tested directly.
pub fn tokens_to_words(
    tokens: &[String],
    timestamps: Option<&[f32]>,
    durations: Option<&[f32]>,
    offset_sec: f64,
) -> Vec<Word> {
    const WORD_BOUNDARY: char = '\u{2581}';
    let ts = timestamps.unwrap_or(&[]);
    let dur = durations.unwrap_or(&[]);

    let mut words: Vec<Word> = Vec::new();
    let mut cur: Option<Word> = None;
    for (i, tok) in tokens.iter().enumerate() {
        let start = ts.get(i).copied().unwrap_or(0.0) as f64 + offset_sec;
        let end = start + dur.get(i).copied().unwrap_or(0.0) as f64;
        let piece = tok.replace(WORD_BOUNDARY, " ");

        if tok.starts_with(WORD_BOUNDARY) {
            if let Some(w) = cur.take() {
                push_word(&mut words, w);
            }
            cur = Some(Word {
                start,
                end,
                text: piece.trim_start().to_string(),
            });
        } else if let Some(w) = cur.as_mut() {
            w.text.push_str(&piece);
            w.end = end;
        } else {
            // First token has no boundary marker — start a word anyway.
            cur = Some(Word {
                start,
                end,
                text: piece,
            });
        }
    }
    if let Some(w) = cur.take() {
        push_word(&mut words, w);
    }
    words
}

fn push_word(words: &mut Vec<Word>, mut w: Word) {
    w.text = w.text.trim().to_string();
    if !w.text.is_empty() {
        words.push(w);
    }
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    /// Proves sherpa-onnx loads **every** selectable English embedding model through the same diarizer —
    /// i.e. the framework (NeMo / WeSpeaker / 3D-Speaker) is auto-detected from the `.onnx`, with no
    /// per-model wiring. Ignored: needs the real segmentation + embedding files on disk. Run it with a model
    /// dir that holds `sherpa-onnx-pyannote-segmentation-3-0/model.onnx` + the embedding `.onnx` files:
    ///   CORTI_VERIFY_MODEL_DIR=/path/to/models \
    ///     cargo test -p corti-transcribe-local build_diarizer_loads_every_embedding -- --ignored --nocapture
    #[test]
    #[ignore = "needs the real ONNX model files; set CORTI_VERIFY_MODEL_DIR"]
    fn build_diarizer_loads_every_embedding() {
        use crate::models::{self, Models};
        use std::path::PathBuf;

        let dir =
            PathBuf::from(std::env::var("CORTI_VERIFY_MODEL_DIR").expect(
                "set CORTI_VERIFY_MODEL_DIR to a dir with the segmentation + embedding files",
            ));
        let segmentation = dir.join(models::SEGMENTATION_DIR).join("model.onnx");
        assert!(
            segmentation.exists(),
            "missing segmentation model at {}",
            segmentation.display()
        );

        for id in models::EMBEDDING_IDS {
            let spec = models::embedding_spec(id);
            let embedding = dir.join(spec.install_rel);
            assert!(
                embedding.exists(),
                "missing {id} embedding at {}",
                embedding.display()
            );
            // build_diarizer only reads `segmentation` + `embedding`; the rest can be empty.
            let m = Models {
                parakeet_encoder: PathBuf::new(),
                parakeet_decoder: PathBuf::new(),
                parakeet_joiner: PathBuf::new(),
                parakeet_tokens: PathBuf::new(),
                segmentation: segmentation.clone(),
                embedding,
                vad: PathBuf::new(),
            };
            let diar = build_diarizer(&m, "cpu", 1, 0.5);
            assert!(
                diar.is_ok(),
                "embedding {id} ({}) failed to load: {:?}",
                spec.install_rel,
                diar.err()
            );
            eprintln!("✓ {id} ({}) loaded OK", spec.install_rel);
        }
    }

    #[test]
    fn reassembles_sentencepiece_subwords_into_words() {
        // "▁Hello ▁world" split into subwords: ▁Hel, lo, ▁wor, ld
        let tokens = toks(&["\u{2581}Hel", "lo", "\u{2581}wor", "ld"]);
        let timestamps = [0.0f32, 0.1, 0.5, 0.6];
        let durations = [0.1f32, 0.1, 0.1, 0.2];
        let words = tokens_to_words(&tokens, Some(&timestamps), Some(&durations), 0.0);
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "Hello");
        assert_eq!(words[0].start, 0.0);
        assert!((words[0].end - 0.2).abs() < 1e-6); // 0.1 + 0.1
        assert_eq!(words[1].text, "world");
        assert!((words[1].start - 0.5).abs() < 1e-6);
        assert!((words[1].end - 0.8).abs() < 1e-6); // 0.6 + 0.2
    }

    #[test]
    fn applies_offset_and_handles_leading_nonboundary_token() {
        let tokens = toks(&["Hi", "\u{2581}there"]);
        let timestamps = [0.0f32, 0.3];
        let durations = [0.2f32, 0.2];
        let words = tokens_to_words(&tokens, Some(&timestamps), Some(&durations), 10.0);
        assert_eq!(words.len(), 2);
        assert_eq!(words[0].text, "Hi");
        assert!((words[0].start - 10.0).abs() < 1e-6);
        assert_eq!(words[1].text, "there");
        assert!((words[1].start - 10.3).abs() < 1e-6);
    }

    #[test]
    fn missing_timestamps_do_not_panic() {
        let tokens = toks(&["\u{2581}ok"]);
        let words = tokens_to_words(&tokens, None, None, 0.0);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "ok");
    }
}
