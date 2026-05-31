//! Transcribe a recorded 2-track WAV with AWS Transcribe and print the diarized Markdown.
//!
//! This example plays the role of the app: it runs the **standard AWS credential chain** (env vars,
//! `~/.aws`, SSO, IAM role) and passes the resulting `SdkConfig` into `AwsTranscriber` — the crate itself
//! never reads credentials.
//!
//! ```sh
//! CORTI_AWS_BUCKET=my-corti-bucket \
//!   cargo run -p corti-transcribe-aws --example transcribe_file -- /path/to/recording.wav
//! ```
//!
//! Requirements:
//! - AWS credentials resolvable by the default chain, and a region (`AWS_REGION` or your profile).
//! - An S3 bucket (`CORTI_AWS_BUCKET`) in that region. We pass no `DataAccessRoleArn`, so Transcribe
//!   uses the **calling principal's** permissions to reach S3 — grant it, on the `corti/` prefix:
//!   `s3:PutObject` / `s3:GetObject` / `s3:DeleteObject` (objects) + `s3:ListBucket` (the bucket), plus
//!   `transcribe:StartTranscriptionJob` / `transcribe:GetTranscriptionJob`. Full policy JSON is in
//!   `design/02-corti-transcribe.md`.
//! - Audio leaves your device for AWS (the job deletes its staged copies when done).
//!
//! Pipe the output into `vagus add-note` (or use `corti-vagus`) to close the full
//! capture → transcribe → note loop.

use std::path::PathBuf;

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use corti_core::{OwningApp, RecordingMeta};
use corti_transcribe::Transcriber;
use corti_transcribe_aws::{AwsOptions, AwsTranscriber};

fn main() -> Result<()> {
    let audio = PathBuf::from(
        std::env::args()
            .nth(1)
            .context("usage: transcribe_file <recording.wav>")?,
    );
    let bucket = std::env::var("CORTI_AWS_BUCKET")
        .context("set CORTI_AWS_BUCKET to an S3 bucket to stage audio in")?;

    // The app's job: resolve credentials/region via the standard chain.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let sdk_config =
        rt.block_on(async { aws_config::defaults(BehaviorVersion::latest()).load().await });

    let transcriber = AwsTranscriber::new(&sdk_config, AwsOptions::new(bucket));

    // The detector would supply real metadata; the AWS backend only needs the audio path.
    let meta = RecordingMeta {
        started_at: chrono::Local::now(),
        ended_at: Some(chrono::Local::now()),
        owning_app: OwningApp::unknown(),
        audio_path: audio.clone(),
    };

    println!("transcribing {} via AWS Transcribe…", audio.display());
    let transcript = transcriber.transcribe(&audio, &meta)?;
    println!("\n{}", transcript.to_markdown());
    Ok(())
}
