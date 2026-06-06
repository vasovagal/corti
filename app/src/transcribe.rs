//! Backend selection: turn a recording into a [`DiarizedTranscript`].
//!
//! Both backends (`aws`, `local`) can be compiled in; which one runs is chosen at **runtime** from config
//! ([`AppConfig::transcribe_backend`], env `CORTI_TRANSCRIBE_BACKEND`) behind the single
//! [`corti_transcribe::Transcriber`] trait, so the pipeline stays backend-agnostic and a future settings
//! screen can switch backends live.

use std::path::Path;

use anyhow::Result;
use corti_core::{DiarizedTranscript, RecordingMeta};

use crate::config::{AppConfig, BackendChoice};

/// The transcription backend, built once at worker startup.
pub struct Backend {
    #[cfg_attr(not(any(feature = "aws", feature = "local")), allow(dead_code))]
    cfg: AppConfig,
    kind: BackendKind,
}

/// The concrete backend resolved at runtime among the compiled-in features.
enum BackendKind {
    /// AWS Transcribe. Holds the SDK config (credential chain resolved once); `None` if it failed.
    /// Boxed: `SdkConfig` is large and would bloat the whole enum otherwise.
    #[cfg(feature = "aws")]
    Aws(Option<Box<aws_config::SdkConfig>>),
    /// Local on-device Parakeet/ONNX backend.
    #[cfg(feature = "local")]
    Local,
    /// The requested backend isn't compiled into this build; carries the reason for a clear error.
    Unavailable(&'static str),
}

impl Backend {
    pub fn init(cfg: AppConfig) -> Self {
        let kind = match cfg.transcribe_backend {
            #[cfg(feature = "aws")]
            BackendChoice::Aws => BackendKind::Aws(build_sdk_config().map(Box::new)),
            #[cfg(feature = "local")]
            BackendChoice::Local => BackendKind::Local,
            #[allow(unreachable_patterns)]
            _ => BackendKind::Unavailable(
                "requested transcription backend is not compiled into this build \
                 (enable the `aws` or `local` feature)",
            ),
        };
        if let BackendKind::Unavailable(reason) = &kind {
            eprintln!("[corti] {reason}");
        }
        Self { cfg, kind }
    }

    /// Transcribe a recording into a diarized transcript using the runtime-selected backend. `job_id` is
    /// the durable recording id; AWS passes it as the stable `AwsOptions.job_name` so a resumed
    /// `Transcribing` job re-attaches to the same AWS job instead of re-submitting (design/05-app-tauri.md).
    pub fn transcribe(
        &self,
        job_id: &str,
        audio: &Path,
        meta: &RecordingMeta,
    ) -> Result<DiarizedTranscript> {
        // `job_id` is only used by the AWS arm; keep it referenced for builds without that feature.
        let _ = job_id;
        match &self.kind {
            #[cfg(feature = "aws")]
            BackendKind::Aws(sdk) => self.transcribe_aws(sdk.as_deref(), job_id, audio, meta),
            #[cfg(feature = "local")]
            BackendKind::Local => self.transcribe_local(audio, meta),
            BackendKind::Unavailable(reason) => anyhow::bail!("{reason}"),
        }
    }

    #[cfg(feature = "aws")]
    fn transcribe_aws(
        &self,
        sdk: Option<&aws_config::SdkConfig>,
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
        let sdk = sdk.context("AWS SDK config unavailable (credential chain failed at startup)")?;
        let opts = AwsOptions {
            job_name: Some(job_id.to_string()),
            language: self.cfg.language.clone(),
            ..AwsOptions::new(bucket)
        };
        AwsTranscriber::new(sdk, opts).transcribe(audio, meta)
    }

    #[cfg(feature = "local")]
    fn transcribe_local(&self, audio: &Path, meta: &RecordingMeta) -> Result<DiarizedTranscript> {
        use corti_transcribe::Transcriber;
        use corti_transcribe_local::{LocalConfig, LocalTranscriber};

        let cfg = LocalConfig {
            model_dir: self.cfg.local_model_dir.clone(),
            provider: self.cfg.local_provider.clone(),
            num_threads: self.cfg.local_threads,
        };
        LocalTranscriber::new(cfg).transcribe(audio, meta)
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
