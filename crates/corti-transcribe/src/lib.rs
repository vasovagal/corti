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
use corti_core::{DiarizedTranscript, QualityRecord, RecordingMeta};

/// Content-free decode outcomes and checked mapping back to capture quality.
pub mod decode;

/// Shared reconciliation helpers (word grouping, speaker merge, diarization attribution) used by the
/// concrete backends to shape timestamped words into a [`DiarizedTranscript`], plus the deterministic
/// [`segment::cleanup`] pass every path runs over the merged timeline.
pub mod segment;

/// A transcript and the settled quality of this invocation. `decode_complete` describes only this
/// actual decode, independently of source loss, legacy capture coverage or historical live failures.
#[derive(Debug, Clone)]
pub struct TranscriptionOutcome {
    pub transcript: DiarizedTranscript,
    pub quality: QualityRecord,
    pub decode_complete: bool,
}

/// Turns a recording into a diarized, timestamped transcript.
///
/// Synchronous by design: corti transcribes *after* the call, off the UI thread, so a blocking call keeps
/// this contract free of `async-trait`/`tokio`. Async backends (AWS) run a private runtime internally.
pub trait Transcriber {
    fn transcribe(&self, audio: &Path, meta: &RecordingMeta) -> Result<DiarizedTranscript>;

    /// Quality-aware additive contract. Backends with region-level outcomes override this method;
    /// existing whole-file backends preserve their contract and the supplied capture history.
    fn transcribe_with_quality(
        &self,
        audio: &Path,
        meta: &RecordingMeta,
        quality: &QualityRecord,
    ) -> Result<TranscriptionOutcome> {
        Ok(TranscriptionOutcome {
            transcript: self.transcribe(audio, meta)?,
            quality: quality.clone(),
            decode_complete: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{OwningApp, QualityClass, QualityResolution};
    use std::cell::Cell;

    struct WholeFileBackend {
        calls: Cell<usize>,
        fail: bool,
    }

    impl Transcriber for WholeFileBackend {
        fn transcribe(&self, _audio: &Path, _meta: &RecordingMeta) -> Result<DiarizedTranscript> {
            self.calls.set(self.calls.get() + 1);
            anyhow::ensure!(!self.fail, "whole-file decode failed");
            Ok(DiarizedTranscript::default())
        }
    }

    fn meta() -> RecordingMeta {
        RecordingMeta {
            started_at: "2026-01-01T00:00:00Z".parse().unwrap(),
            ended_at: None,
            owning_app: OwningApp {
                bundle_id: None,
                name: "Synthetic".into(),
            },
            audio_path: "unused.wav".into(),
        }
    }

    #[test]
    fn additive_default_calls_existing_backend_once_without_upgrading_capture_quality() {
        let backend = WholeFileBackend {
            calls: Cell::new(0),
            fail: false,
        };
        let mut quality = QualityRecord::default();
        quality
            .record_omitted_spans(
                QualityClass::ProcessingLoss,
                QualityResolution::Outstanding,
                1,
            )
            .unwrap();
        let metadata = meta();
        let result = backend
            .transcribe_with_quality(&metadata.audio_path, &metadata, &quality)
            .unwrap();
        assert_eq!(backend.calls.get(), 1);
        assert!(
            result.decode_complete,
            "valid empty recognition completes this decode"
        );
        assert!(result.transcript.segments.is_empty());
        assert_eq!(
            result.quality, quality,
            "capture and historical failures remain unchanged"
        );
    }

    #[test]
    fn additive_default_preserves_whole_file_errors_without_a_second_attempt() {
        let backend = WholeFileBackend {
            calls: Cell::new(0),
            fail: true,
        };
        let metadata = meta();
        assert!(
            backend
                .transcribe_with_quality(&metadata.audio_path, &metadata, &QualityRecord::default())
                .is_err()
        );
        assert_eq!(backend.calls.get(), 1);
    }
}
