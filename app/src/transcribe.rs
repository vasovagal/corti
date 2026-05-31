//! Backend selection: turn a recording into a [`DiarizedTranscript`], feature-flavored (`aws` / `whisper`).
//!
//! The rest of the pipeline is backend-agnostic (it only sees [`Backend::transcribe`]); which concrete
//! [`corti_transcribe::Transcriber`] runs is a compile-time choice. `default = ["aws"]`.

use std::path::Path;

use anyhow::Result;
use corti_core::{DiarizedTranscript, RecordingMeta};

use crate::config::AppConfig;

/// The transcription backend, built once at worker startup.
pub struct Backend {
    cfg: AppConfig,
    /// AWS SDK config (credential chain resolved once). `None` if it failed to build.
    #[cfg(feature = "aws")]
    sdk: Option<aws_config::SdkConfig>,
}

impl Backend {
    pub fn init(cfg: AppConfig) -> Self {
        Self {
            #[cfg(feature = "aws")]
            sdk: build_sdk_config(),
            cfg,
        }
    }
}

#[cfg(feature = "aws")]
impl Backend {
    /// Transcribe via AWS Transcribe. `job_id` is the durable recording id: passing it as the stable
    /// `AwsOptions.job_name` is what lets a resumed `Transcribing` job re-attach to the same AWS job (and
    /// re-fetch the same output) instead of paying to submit a fresh one (design/05-app-tauri.md).
    pub fn transcribe(
        &self,
        job_id: &str,
        audio: &Path,
        meta: &RecordingMeta,
    ) -> Result<DiarizedTranscript> {
        use anyhow::Context;
        use corti_transcribe::Transcriber;
        use corti_transcribe_aws::{AwsOptions, AwsTranscriber};

        let bucket = self
            .cfg
            .aws_bucket
            .clone()
            .context("no S3 bucket configured — export CORTI_AWS_BUCKET")?;
        let sdk = self
            .sdk
            .as_ref()
            .context("AWS SDK config unavailable (credential chain failed at startup)")?;

        let opts = AwsOptions {
            job_name: Some(job_id.to_string()),
            language: self.cfg.language.clone(),
            ..AwsOptions::new(bucket)
        };
        AwsTranscriber::new(sdk, opts).transcribe(audio, meta)
    }
}

#[cfg(all(feature = "whisper", not(feature = "aws")))]
impl Backend {
    /// Transcribe via local whisper (the crate is a stub today — see design/02-corti-transcribe.md).
    pub fn transcribe(
        &self,
        _job_id: &str,
        audio: &Path,
        meta: &RecordingMeta,
    ) -> Result<DiarizedTranscript> {
        use anyhow::Context;
        use corti_transcribe::Transcriber;
        use corti_transcribe_whisper::WhisperTranscriber;

        let model_path = self
            .cfg
            .whisper_model
            .clone()
            .context("no whisper model configured — export CORTI_WHISPER_MODEL")?;
        WhisperTranscriber { model_path }.transcribe(audio, meta)
    }
}

#[cfg(not(any(feature = "aws", feature = "whisper")))]
impl Backend {
    pub fn transcribe(
        &self,
        _job_id: &str,
        _audio: &Path,
        _meta: &RecordingMeta,
    ) -> Result<DiarizedTranscript> {
        anyhow::bail!(
            "no transcription backend compiled in (enable the `aws` or `whisper` feature)"
        )
    }
}

/// Resolve the AWS credential chain once, on a throwaway current-thread runtime. The backend itself spins
/// its own runtime for the actual S3/Transcribe calls; this only builds the shared `SdkConfig`.
#[cfg(feature = "aws")]
fn build_sdk_config() -> Option<aws_config::SdkConfig> {
    use aws_config::BehaviorVersion;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| eprintln!("[corti] could not build tokio runtime for AWS config: {e}"))
        .ok()?;
    Some(rt.block_on(async { aws_config::defaults(BehaviorVersion::latest()).load().await }))
}
