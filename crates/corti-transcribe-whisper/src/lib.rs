//! Local whisper backend for corti — **shell, not yet implemented**.
//!
//! Plan (see `design/02-corti-transcribe.md`): run `whisper-rs` locally (fully offline). whisper doesn't
//! diarize, so transcribe the mic (ch0) and far-end (ch1) tracks separately and interleave segments by
//! time → `Speaker::Me` / `Speaker::Other`. Cache the model under `~/Library/Caches/corti/models/`.

use std::path::Path;

use anyhow::{Result, bail};
use corti_core::{DiarizedTranscript, RecordingMeta};
use corti_transcribe::Transcriber;

/// Local whisper backend. Holds the model path when implemented.
#[derive(Debug, Default, Clone)]
pub struct WhisperTranscriber {
    /// Path to the ggml/gguf whisper model file.
    pub model_path: std::path::PathBuf,
}

impl Transcriber for WhisperTranscriber {
    fn transcribe(&self, _audio: &Path, _meta: &RecordingMeta) -> Result<DiarizedTranscript> {
        // TODO(design/02-corti-transcribe.md): whisper-rs on ch0 + ch1, interleave by timestamp.
        bail!("corti-transcribe-whisper is not yet implemented")
    }
}
