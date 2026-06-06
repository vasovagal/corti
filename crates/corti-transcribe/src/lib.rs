//! The transcription contract for corti: a recorded 2-track WAV → a [`DiarizedTranscript`].
//!
//! Backends (`corti-transcribe-aws`, `corti-transcribe-local`) implement [`Transcriber`]; the rest of
//! the pipeline is backend-agnostic. The diarized-Markdown renderer lives in `corti-core`
//! ([`DiarizedTranscript::to_markdown`]), so backends only produce the struct.
//!
//! See `design/02-corti-transcribe.md` for the AWS/local plans and the 2-track diarization prior
//! (ch0 = me, ch1 = them).

use std::path::Path;

use anyhow::Result;
use corti_core::{DiarizedTranscript, RecordingMeta};

/// Shared reconciliation helpers (word grouping, speaker merge, diarization attribution) used by the
/// concrete backends to shape timestamped words into a [`DiarizedTranscript`].
pub mod segment;

/// Turns a recording into a diarized, timestamped transcript.
///
/// Synchronous by design: corti transcribes *after* the call, off the UI thread, so a blocking call keeps
/// this contract free of `async-trait`/`tokio`. Async backends (AWS) run a private runtime internally.
pub trait Transcriber {
    fn transcribe(&self, audio: &Path, meta: &RecordingMeta) -> Result<DiarizedTranscript>;
}
