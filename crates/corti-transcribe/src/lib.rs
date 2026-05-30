//! The transcription contract for corti: a recorded 2-track WAV → a [`DiarizedTranscript`].
//!
//! Backends (`corti-transcribe-aws`, `corti-transcribe-whisper`) implement [`Transcriber`]; the rest of
//! the pipeline is backend-agnostic. The diarized-Markdown renderer lives in `corti-core`
//! ([`DiarizedTranscript::to_markdown`]), so backends only produce the struct.
//!
//! See `design/02-corti-transcribe.md` for the AWS/whisper plans and the 2-track diarization prior
//! (ch0 = me, ch1 = them).

use std::path::Path;

use anyhow::Result;
use corti_core::{DiarizedTranscript, RecordingMeta};

/// Turns a recording into a diarized, timestamped transcript.
///
/// Synchronous by design: corti transcribes *after* the call, off the UI thread, so a blocking call keeps
/// this contract free of `async-trait`/`tokio`. Async backends (AWS) run a private runtime internally.
pub trait Transcriber {
    fn transcribe(&self, audio: &Path, meta: &RecordingMeta) -> Result<DiarizedTranscript>;
}
