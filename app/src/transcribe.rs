//! Backend selection: turn a recording into a [`DiarizedTranscript`].
//!
//! Both backends (`aws`, `local`) can be compiled in; which one runs is chosen at **runtime** from config
//! ([`AppConfig::transcribe_backend`], env `CORTI_TRANSCRIBE_BACKEND`) behind the single
//! [`corti_transcribe::Transcriber`] trait, so the pipeline stays backend-agnostic and a future settings
//! screen can switch backends live.

use std::path::{Path, PathBuf};

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
            diarize_far_end: self.cfg.local_diarize_far_end,
        };
        LocalTranscriber::new(cfg).transcribe(audio, meta)
    }
}

/// Run offline AEC (unless `skip_aec`), transcribe with the runtime-selected `backend`, then persist the
/// `<recording>.transcript.json` sidecar next to `raw_audio`. This is the tray-free, queue-free
/// transcription core shared by the pipeline worker ([`crate::pipeline::transcribe_and_file`]) and the
/// `--redo` CLI ([`crate::cli`]). Returns the transcript plus the audio path actually fed to the backend
/// (the cleaned WAV when AEC ran, else the raw input) for logging.
///
/// `skip_aec` is set by the CLI when the input is already a `*-clean.wav` (a 2-channel AEC *output*):
/// running AEC again would cancel a second time. The pipeline always passes `skip_aec = false` — its input
/// is the raw 2-track recording, and a tap-only (mono) recording is handled inside `write_clean_wav`
/// (`Ok(None)`). Only a genuine backend transcription failure is returned as `Err`; AEC failures fall back
/// to the raw recording and a sidecar-write failure is logged but swallowed, so neither stalls the pipeline.
pub fn transcribe_recording(
    backend: &Backend,
    aec_enabled: bool,
    skip_aec: bool,
    id: &str,
    meta: &RecordingMeta,
    raw_audio: &Path,
) -> Result<(DiarizedTranscript, PathBuf)> {
    // Clean speaker bleed on disk before transcription (backend-agnostic). The raw recording is never
    // touched. A tap-only ("webinar") recording has no mic track, so AEC is skipped deliberately (not an
    // error); a genuine AEC failure falls back to the raw recording so the pipeline never stalls.
    let input: PathBuf = if aec_enabled && !skip_aec {
        match corti_capture::write_clean_wav(raw_audio) {
            Ok(Some(clean)) => clean,
            Ok(None) => {
                // Tap-only ("webinar"/listen-only) recording: no mic, nothing to cancel.
                eprintln!("[corti] tap-only recording — no mic track to clean; skipping AEC");
                raw_audio.to_path_buf()
            }
            Err(e) => {
                eprintln!("[corti] AEC failed ({e:#}); using the raw recording");
                raw_audio.to_path_buf()
            }
        }
    } else {
        raw_audio.to_path_buf()
    };

    let transcript = backend.transcribe(id, &input, meta)?;

    // Persist the transcript next to the WAV so re-filing after a crash never needs to re-transcribe.
    // Best-effort: even if this fails we still hold the transcript in memory for this run.
    let sidecar = crate::pipeline::sidecar_path(raw_audio);
    if let Err(e) = crate::pipeline::write_sidecar(&sidecar, &transcript) {
        eprintln!(
            "[corti] could not persist transcript sidecar {}: {e:#}",
            sidecar.display()
        );
    }

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
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| eprintln!("[corti] could not build tokio runtime for AWS config: {e}"))
        .ok()?;
    Some(rt.block_on(configure_loader(cfg).load()))
}
