//! AWS Transcribe (batch) backend for corti — **shell, not yet implemented**.
//!
//! Plan (see `design/02-corti-transcribe.md`): upload the audio to S3, `StartTranscriptionJob` with
//! `ShowSpeakerLabels` for diarization + word timestamps, poll, then join `results.items[]` to
//! `results.speaker_labels.segments[]` into a [`DiarizedTranscript`]. Map the speaker aligned with ch0 to
//! `Speaker::Me`.

use std::path::Path;

use anyhow::{Result, bail};
use corti_core::{DiarizedTranscript, RecordingMeta};
use corti_transcribe::Transcriber;

/// AWS Transcribe batch backend. Configure with creds/region/bucket when implemented.
#[derive(Debug, Default, Clone)]
pub struct AwsTranscriber {
    /// S3 bucket to stage audio in.
    pub bucket: String,
    /// AWS region (e.g. "us-east-1").
    pub region: Option<String>,
}

impl Transcriber for AwsTranscriber {
    fn transcribe(&self, _audio: &Path, _meta: &RecordingMeta) -> Result<DiarizedTranscript> {
        // TODO(design/02-corti-transcribe.md): aws-sdk-s3 upload → aws-sdk-transcribe job → poll → parse.
        bail!("corti-transcribe-aws is not yet implemented")
    }
}
