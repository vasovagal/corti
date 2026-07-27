//! Backend selection: turn a recording into a [`DiarizedTranscript`].
//!
//! Both backends (`aws`, `local`) can be compiled in; which one runs is chosen at **runtime** from config
//! ([`AppConfig::transcribe_backend`], env `CORTI_TRANSCRIBE_BACKEND`) behind the single
//! [`corti_transcribe::Transcriber`] trait, so the pipeline stays backend-agnostic and a future settings
//! screen can switch backends live.

use std::path::{Path, PathBuf};

use anyhow::Result;
use corti_core::{DiarizedTranscript, RecordingMeta};
use tracing::{error, info, warn};

use crate::checkpoint::AwsStaging;
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
            BackendChoice::Aws => BackendKind::Aws(build_sdk_config(&cfg).map(Box::new)),
            #[cfg(feature = "local")]
            BackendChoice::Local => BackendKind::Local,
            #[allow(unreachable_patterns)]
            _ => BackendKind::Unavailable(
                "requested transcription backend is not compiled into this build \
                 (enable the `aws` or `local` feature)",
            ),
        };
        if let BackendKind::Unavailable(reason) = &kind {
            error!(target: "corti::transcribe", "{reason}");
        }
        Self { cfg, kind }
    }

    /// Transcribe a recording into a diarized transcript using the runtime-selected backend. The durable
    /// pipeline supplies the stable name persisted on the recording row so retries reattach; explicit CLI
    /// runs pass `None` so `--redo --aws` always creates a fresh AWS attempt.
    pub fn transcribe(
        &self,
        aws_job_name: Option<&str>,
        audio: &Path,
        meta: &RecordingMeta,
    ) -> Result<DiarizedTranscript> {
        // The job name is only used by the AWS arm; keep it referenced for builds without that feature.
        let _ = aws_job_name;
        match &self.kind {
            #[cfg(feature = "aws")]
            BackendKind::Aws(sdk) => self.transcribe_aws(sdk.as_deref(), aws_job_name, audio, meta),
            #[cfg(feature = "local")]
            BackendKind::Local => self.transcribe_local(audio, meta),
            BackendKind::Unavailable(reason) => anyhow::bail!("{reason}"),
        }
    }

    #[cfg(feature = "aws")]
    fn transcribe_aws(
        &self,
        sdk: Option<&aws_config::SdkConfig>,
        job_name: Option<&str>,
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
            job_name: job_name.map(str::to_string),
            // A durable stable job is cleaned only after the app checkpoint is persisted. One-shot CLI
            // attempts keep the backend's success-only cleanup behavior.
            delete_after: job_name.is_none(),
            language: self.cfg.language.clone(),
            ..AwsOptions::new(bucket)
        };
        AwsTranscriber::new(sdk, opts).transcribe(audio, meta)
    }

    /// Describe any cloud staging owned by the just-completed durable attempt. The checkpoint, rather than
    /// mutable current settings, becomes the cleanup authority.
    pub fn aws_staging_for_checkpoint(&self, job_name: &str) -> Result<Option<AwsStaging>> {
        let _ = job_name;
        match &self.kind {
            #[cfg(feature = "aws")]
            BackendKind::Aws(sdk) => {
                use anyhow::Context;
                use corti_transcribe_aws::AwsOptions;

                let bucket = self
                    .cfg
                    .aws_bucket
                    .clone()
                    .context("no S3 bucket configured for checkpoint cleanup")?;
                let sdk = sdk
                    .as_deref()
                    .context("AWS SDK config unavailable for checkpoint cleanup")?;
                let opts = AwsOptions::new(bucket.clone());
                Ok(Some(AwsStaging {
                    bucket,
                    key_prefix: opts.key_prefix,
                    job_name: job_name.to_string(),
                    region: sdk.region().map(|region| region.as_ref().to_string()),
                }))
            }
            #[cfg(feature = "local")]
            BackendKind::Local => Ok(None),
            BackendKind::Unavailable(_) => Ok(None),
        }
    }

    /// Remove the exact staged objects recorded by the durable checkpoint. This intentionally does not use
    /// the currently selected backend or bucket: users may switch to local transcription (or change buckets)
    /// while a filing retry is waiting. Cleanup completion is persisted by the caller before filing resumes.
    pub fn cleanup_after_checkpoint(&self, staging: &AwsStaging) -> Result<()> {
        let _ = staging;
        #[cfg(feature = "aws")]
        {
            use anyhow::Context;
            use corti_transcribe_aws::{AwsOptions, AwsTranscriber};

            let sdk = build_sdk_config_for_region(&self.cfg, staging.region.as_deref())
                .context("AWS SDK config unavailable for checkpoint cleanup")?;
            let opts = AwsOptions {
                key_prefix: staging.key_prefix.clone(),
                delete_after: false,
                ..AwsOptions::new(staging.bucket.clone())
            };
            AwsTranscriber::new(&sdk, opts).cleanup_staged(&staging.job_name)
        }
        #[cfg(not(feature = "aws"))]
        anyhow::bail!(
            "checkpoint requires AWS staged-object cleanup, but this build has no AWS backend"
        )
    }

    #[cfg(feature = "local")]
    fn transcribe_local(&self, audio: &Path, meta: &RecordingMeta) -> Result<DiarizedTranscript> {
        use corti_transcribe::Transcriber;
        use corti_transcribe_local::{LocalConfig, LocalTranscriber};

        let cfg = LocalConfig {
            model_dir: self.cfg.local_model_dir.clone(),
            provider: self.cfg.local_provider.clone(),
            num_threads: self.cfg.local_threads,
            diarize_far_end: self.cfg.local_diarize_far_end,
            embedding_model: self.cfg.local_embedding_model.clone(),
            diarize_threshold: self.cfg.local_diarize_threshold,
            // VAD/diarizer fine-tuning knobs are exposed for the benchmark harness and default to the
            // shipping constants; the app keeps them at default until a tuned default is adopted.
            ..LocalConfig::default()
        };
        LocalTranscriber::new(cfg).transcribe(audio, meta)
    }
}

/// Identifies one transcription invocation for diagnostics and AWS idempotency.
#[derive(Clone, Copy)]
pub struct TranscriptionAttempt<'a> {
    id: &'a str,
    aws_job_name: Option<&'a str>,
}

impl<'a> TranscriptionAttempt<'a> {
    /// Durable pipeline attempt: AWS retries reattach to the stable name persisted on the recording row.
    /// Ordinarily this is `id`; legacy `PendingNote` compatibility recovery uses a new name so it cannot
    /// reattach to an old completed job whose output predates checkpoint-safe cleanup.
    pub fn durable_named(id: &'a str, aws_job_name: &'a str) -> Self {
        Self {
            id,
            aws_job_name: Some(aws_job_name),
        }
    }

    /// Explicit CLI attempt: keep the diagnostic id but mint a fresh AWS name.
    pub fn fresh(id: &'a str) -> Self {
        Self {
            id,
            aws_job_name: None,
        }
    }
}

/// Run offline AEC (unless `skip_aec`), then transcribe with the runtime-selected `backend`. This is the
/// tray-free, queue-free transcription core shared by the pipeline worker
/// ([`crate::pipeline::transcribe_and_file`]) and the `--redo`/`--input` CLI ([`crate::cli`]). Returns the
/// transcript plus the audio path actually fed to the backend (the cleaned WAV when AEC ran, else the raw
/// input) for logging. Persisting any transcript sidecar is a caller's concern, not this primitive's.
///
/// `skip_aec` is set by the CLI when the input is already a `*-clean.wav` (a 2-channel AEC *output*):
/// running AEC again would cancel a second time. The pipeline always passes `skip_aec = false` — its input
/// is the raw 2-track recording, and a tap-only (mono) recording is handled inside `write_clean_wav`
/// (`Ok(None)`). Only a genuine backend transcription failure is returned as `Err`; an AEC failure falls
/// back to the raw recording so it never stalls the caller.
pub fn transcribe_recording(
    backend: &Backend,
    aec_enabled: bool,
    skip_aec: bool,
    aec_cfg: &corti_aec::AecConfig,
    attempt: TranscriptionAttempt<'_>,
    meta: &RecordingMeta,
    raw_audio: &Path,
) -> Result<(DiarizedTranscript, PathBuf)> {
    // Clean speaker bleed on disk before transcription (backend-agnostic). The raw recording is never
    // touched. A tap-only ("webinar") recording has no mic track, so AEC is skipped deliberately (not an
    // error); a genuine AEC failure falls back to the raw recording so the pipeline never stalls.
    let input: PathBuf = if aec_enabled && !skip_aec {
        match corti_capture::write_clean_wav(raw_audio, aec_cfg) {
            Ok(Some(clean)) => {
                info!(
                    target: "corti::transcribe",
                    job_id = %attempt.id,
                    aec = true,
                    input = %raw_audio.display(),
                    output = %clean.display(),
                    "AEC ran — cleaned recording"
                );
                clean
            }
            Ok(None) => {
                // Tap-only ("webinar"/listen-only) recording: no mic, nothing to cancel.
                info!(
                    target: "corti::transcribe",
                    job_id = %attempt.id,
                    aec = false,
                    input = %raw_audio.display(),
                    "tap-only recording — no mic track to clean; skipping AEC"
                );
                raw_audio.to_path_buf()
            }
            Err(e) => {
                warn!(
                    target: "corti::transcribe",
                    job_id = %attempt.id,
                    aec = false,
                    input = %raw_audio.display(),
                    error = %format!("{e:#}"),
                    "AEC failed; using the raw recording"
                );
                raw_audio.to_path_buf()
            }
        }
    } else {
        raw_audio.to_path_buf()
    };

    let transcript = backend.transcribe(attempt.aws_job_name, &input, meta)?;
    Ok((transcript, input))
}

/// Whether an env var is set and non-empty (after trim) — the conventional "this is configured" test, and
/// the signal that an AWS-native env var should take precedence over our saved config.
#[cfg(feature = "aws")]
pub(crate) fn env_present(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| !v.trim().is_empty())
}

/// Build a `ConfigLoader` that applies the app's saved profile/region as **fallbacks behind** the AWS
/// environment, matching standard CLI/SDK precedence. Sync + IO-free: the credential-resolving `.load()` is
/// driven by the caller on whatever runtime it has.
#[cfg(feature = "aws")]
pub(crate) fn configure_loader(cfg: &AppConfig) -> aws_config::ConfigLoader {
    use aws_config::{BehaviorVersion, Region};

    let mut loader = aws_config::defaults(BehaviorVersion::latest());

    // Region: our value only when the environment doesn't set one.
    if !(env_present("AWS_REGION") || env_present("AWS_DEFAULT_REGION"))
        && let Some(region) = cfg.aws_region.as_deref().filter(|s| !s.is_empty())
    {
        loader = loader.region(Region::new(region.to_string()));
    }

    // Profile: our value only when neither env static creds nor AWS_PROFILE are present (either would win in
    // the credential chain regardless).
    if !env_present("AWS_ACCESS_KEY_ID")
        && !env_present("AWS_PROFILE")
        && let Some(profile) = cfg.aws_profile.as_deref().filter(|s| !s.is_empty())
    {
        loader = loader.profile_name(profile.to_string());
    }

    loader
}

/// Resolve the AWS credential chain once at startup, on a throwaway current-thread runtime. The backend
/// itself spins its own runtime for the actual S3/Transcribe calls; this only builds the shared `SdkConfig`.
///
/// Do NOT call this from inside an existing tokio runtime (e.g. the async `verify_aws` command) — the inner
/// `block_on` panics there. Async callers await `configure_loader(cfg).load()` directly instead.
#[cfg(feature = "aws")]
fn build_sdk_config(cfg: &AppConfig) -> Option<aws_config::SdkConfig> {
    build_sdk_config_for_region(cfg, None)
}

/// Build an SDK config for cleanup, pinning the checkpoint's original region after applying the normal
/// credential/profile chain. This keeps a later region-setting change from addressing the old bucket with
/// the wrong regional client.
#[cfg(feature = "aws")]
fn build_sdk_config_for_region(
    cfg: &AppConfig,
    region: Option<&str>,
) -> Option<aws_config::SdkConfig> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| error!(target: "corti::transcribe", error = %e, "could not build tokio runtime for AWS config"))
        .ok()?;
    let mut loader = configure_loader(cfg);
    if let Some(region) = region.filter(|region| !region.is_empty()) {
        loader = loader.region(aws_config::Region::new(region.to_string()));
    }
    Some(rt.block_on(loader.load()))
}
