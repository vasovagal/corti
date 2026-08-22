//! Backend selection: turn a recording into a [`DiarizedTranscript`].
//!
//! Both backends (`aws`, `local`) can be compiled in; which one runs is chosen at **runtime** from config
//! ([`AppConfig::transcribe_backend`], env `CORTI_TRANSCRIBE_BACKEND`) behind the single
//! [`corti_transcribe::Transcriber`] trait, so the pipeline stays backend-agnostic and a future settings
//! screen can switch backends live.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use corti_core::{DiarizedTranscript, RecordingMeta};
use tracing::{error, info, warn};

use crate::checkpoint::AwsStaging;
use crate::config::AppConfig;
#[cfg(any(feature = "aws", feature = "local"))]
use crate::config::BackendChoice;

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
    /// Local on-device Parakeet backend (selected sherpa/CPU or transcribe.cpp/Metal runtime).
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
        #[allow(irrefutable_let_patterns)]
        // tracing-only/no-backend builds contain only this variant
        if let BackendKind::Unavailable(reason) = &kind {
            error!(target: "corti::transcribe", "{reason}");
        }
        Self { cfg, kind }
    }

    /// Immutable-catalogue backend label; never forwards bucket/profile/config values.
    pub fn trace_backend(&self) -> &'static str {
        match &self.kind {
            #[cfg(feature = "aws")]
            BackendKind::Aws(_) => "aws",
            #[cfg(feature = "local")]
            BackendKind::Local => "local",
            BackendKind::Unavailable(_) => "other",
        }
    }

    /// Immutable-catalogue engine label. GGML is intentionally `other` until a future schema version names
    /// it; the app never mislabels it as Whisper.
    pub fn trace_engine(&self) -> &'static str {
        match &self.kind {
            #[cfg(feature = "aws")]
            BackendKind::Aws(_) => "system",
            #[cfg(feature = "local")]
            BackendKind::Local if self.cfg.local_asr_engine == "sherpa" => "onnx",
            #[cfg(feature = "local")]
            BackendKind::Local => "other",
            BackendKind::Unavailable(_) => "other",
        }
    }

    /// Provenance with the recording's durable AEC execution record rather than current Settings.
    pub fn provenance_with_aec(
        &self,
        mode: corti_vagus::provenance::GenerationMode,
        aec: crate::provenance::AecExecution<'_>,
    ) -> corti_vagus::provenance::TranscriptProvenance {
        crate::provenance::from_config_with_aec(&self.cfg, mode, aec)
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
        // These are consumed only by concrete backend arms; keep tracing-only builds warning-free.
        let _ = (aws_job_name, audio, meta);
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
            // attempts have no reattachment owner, so the backend attempts cleanup on every outcome.
            delete_after: job_name.is_none(),
            language: self.cfg.language.clone(),
            ..AwsOptions::new(bucket)
        };
        AwsTranscriber::new(sdk, opts).transcribe(audio, meta)
    }

    /// Describe the exact cloud staging a durable attempt will own. The pipeline persists this before the
    /// backend can upload, then carries the same identity into its post-ASR checkpoint.
    pub fn aws_staging_for_attempt(&self, job_name: &str) -> Result<Option<AwsStaging>> {
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
            num_threads: self.cfg.local_threads,
            diarize_far_end: self.cfg.local_diarize_far_end,
            embedding_model: self.cfg.local_embedding_model.clone(),
            diarize_threshold: self.cfg.local_diarize_threshold,
            asr_engine: self.cfg.local_asr_engine.clone(),
            ggml_model: self.cfg.local_ggml_model.clone(),
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

/// One immutable whole-file fallback request. The lookahead is captured with the recording/config rather
/// than re-read when a durable retry eventually runs.
#[derive(Debug, Clone, PartialEq)]
pub struct OfflineAec {
    pub config: corti_aec::AecConfig,
    pub lookahead_seconds: f32,
}

impl OfflineAec {
    pub fn current(config: corti_aec::AecConfig) -> Self {
        Self {
            config,
            lookahead_seconds: corti_aec::configured_lookahead_seconds(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfflineAecOutcome {
    NotRequested,
    Applied,
    NotApplicable,
    Failed,
}

/// Parse and validate a queue row's versioned capture-processing record. `None` is a legacy pre-marker row.
pub fn decode_capture_processing(
    raw: Option<&str>,
) -> Result<Option<corti_capture::CaptureProcessing>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let processing: corti_capture::CaptureProcessing =
        serde_json::from_str(raw).context("parsing capture-processing record")?;
    processing.validate()?;
    Ok(Some(processing))
}

/// Decide whether a retained recording needs a safe whole-file pass. Marker-less rows preserve the old
/// behavior for upgrades; a factory failure is wholly raw and uses its original settings; fully applied,
/// intentionally disabled, tap-only, and mixed/degraded files must not be processed twice.
pub fn recording_aec_plan(
    processing: Option<&corti_capture::CaptureProcessing>,
    legacy_enabled: bool,
    legacy_config: &corti_aec::AecConfig,
    already_clean: bool,
) -> Option<OfflineAec> {
    if already_clean {
        return None;
    }
    match processing.map(|p| &p.aec) {
        Some(corti_capture::CaptureAecState::RawFallback {
            config,
            lookahead_seconds,
        }) => Some(OfflineAec {
            config: config.clone(),
            lookahead_seconds: *lookahead_seconds,
        }),
        Some(
            corti_capture::CaptureAecState::Disabled
            | corti_capture::CaptureAecState::NotApplicable
            | corti_capture::CaptureAecState::Applied { .. }
            | corti_capture::CaptureAecState::Degraded { .. },
        ) => None,
        None if legacy_enabled => Some(OfflineAec::current(legacy_config.clone())),
        None => None,
    }
}

/// Optionally run the file-to-file AEC pass, then transcribe with the runtime-selected `backend`. This is
/// the tray-free, queue-free transcription core shared by the pipeline worker
/// ([`crate::pipeline::transcribe_and_file`]) and the `--redo`/`--input` CLI ([`crate::cli`]). Returns the
/// transcript plus the audio path actually fed to the backend (the cleaned WAV when AEC ran, else the raw
/// input) for logging. Persisting any transcript sidecar is a caller's concern, not this primitive's.
///
/// `aec` is `Some` for foreign audio, marker-less legacy rows, or a positively identified wholly-raw
/// writer fallback. Applied/degraded/disabled captures pass `None` to avoid changing their retained audio.
/// A tap-only input is handled as `NotApplicable`; an AEC error becomes `Failed` and falls back to raw,
/// while only a genuine backend transcription failure is returned as `Err`.
pub fn transcribe_recording(
    backend: &Backend,
    aec: Option<&OfflineAec>,
    attempt: TranscriptionAttempt<'_>,
    meta: &RecordingMeta,
    raw_audio: &Path,
    parent: &crate::offline_trace::Span,
) -> Result<(DiarizedTranscript, PathBuf, OfflineAecOutcome)> {
    let backend_label = backend.trace_backend();
    let engine_label = backend.trace_engine();
    let transcription = crate::offline_trace::transcription(parent, backend_label, engine_label);
    let result = transcription.in_scope(|| {
        // Clean speaker bleed on disk before transcription (backend-agnostic). The input file is never
        // touched. A tap-only recording has no mic track, so AEC is deliberately skipped; a genuine AEC
        // failure falls back to raw so the functional pipeline never stalls.
        let (input, aec_outcome): (PathBuf, OfflineAecOutcome) = if let Some(aec) = aec {
            let aec_span = crate::offline_trace::transcription_aec(
                &transcription,
                backend_label,
                engine_label,
            );
            let cleaned = aec_span.in_scope(|| {
                corti_capture::write_clean_wav_with_lookahead(
                    raw_audio,
                    &aec.config,
                    aec.lookahead_seconds,
                )
            });
            match cleaned {
                Ok(Some(clean)) => {
                    aec_span.ok();
                    info!(
                        target: "corti::transcribe",
                        job_id = %attempt.id,
                        aec = true,
                        input = %raw_audio.display(),
                        output = %clean.display(),
                        "AEC ran — cleaned recording"
                    );
                    (clean, OfflineAecOutcome::Applied)
                }
                Ok(None) => {
                    aec_span.skipped();
                    // Tap-only ("webinar"/listen-only) recording: no mic, nothing to cancel.
                    info!(
                        target: "corti::transcribe",
                        job_id = %attempt.id,
                        aec = false,
                        input = %raw_audio.display(),
                        "tap-only recording — no mic track to clean; skipping AEC"
                    );
                    (raw_audio.to_path_buf(), OfflineAecOutcome::NotApplicable)
                }
                Err(e) => {
                    aec_span.fallback();
                    warn!(
                        target: "corti::transcribe",
                        job_id = %attempt.id,
                        aec = false,
                        input = %raw_audio.display(),
                        error = %format!("{e:#}"),
                        "AEC failed; using the raw recording"
                    );
                    (raw_audio.to_path_buf(), OfflineAecOutcome::Failed)
                }
            }
        } else {
            (raw_audio.to_path_buf(), OfflineAecOutcome::NotRequested)
        };

        let transcript = backend.transcribe(attempt.aws_job_name, &input, meta)?;
        Ok((transcript, input, aec_outcome))
    });
    match &result {
        Ok((transcript, _, _)) => {
            transcription.record_item_count(transcript.segments.len());
            transcription.ok();
        }
        Err(_) => transcription.error(crate::offline_trace::ErrorCode::Other),
    }
    result
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

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(config: corti_aec::AecConfig) -> corti_capture::CaptureProcessing {
        corti_capture::CaptureProcessing {
            schema_version: corti_capture::CAPTURE_PROCESSING_SCHEMA_VERSION,
            aec: corti_capture::CaptureAecState::Applied {
                config,
                lookahead_seconds: 7.0,
            },
        }
    }

    #[test]
    fn legacy_rows_keep_the_offline_upgrade_path_but_marked_clean_rows_do_not() {
        let cfg = corti_aec::AecConfig::default();
        let legacy = recording_aec_plan(None, true, &cfg, false).unwrap();
        assert_eq!(legacy.config, cfg);
        assert!(recording_aec_plan(None, true, &cfg, true).is_none());
        assert!(recording_aec_plan(Some(&applied(cfg.clone())), true, &cfg, false).is_none());
        for aec in [
            corti_capture::CaptureAecState::Disabled,
            corti_capture::CaptureAecState::NotApplicable,
            corti_capture::CaptureAecState::Degraded {
                config: cfg.clone(),
                lookahead_seconds: 5.0,
            },
        ] {
            let processing = corti_capture::CaptureProcessing {
                schema_version: corti_capture::CAPTURE_PROCESSING_SCHEMA_VERSION,
                aec,
            };
            assert!(recording_aec_plan(Some(&processing), true, &cfg, false).is_none());
        }
    }

    #[test]
    fn raw_factory_fallback_reuses_capture_time_settings() {
        let current = corti_aec::AecConfig::default();
        let mut captured = current.clone();
        captured.filter_len = 4_096;
        let processing = corti_capture::CaptureProcessing {
            schema_version: corti_capture::CAPTURE_PROCESSING_SCHEMA_VERSION,
            aec: corti_capture::CaptureAecState::RawFallback {
                config: captured.clone(),
                lookahead_seconds: 9.0,
            },
        };
        let plan = recording_aec_plan(Some(&processing), false, &current, false).unwrap();
        assert_eq!(plan.config, captured);
        assert_eq!(plan.lookahead_seconds, 9.0);
    }

    #[test]
    fn capture_processing_json_round_trips_and_rejects_unknown_versions() {
        let processing = applied(corti_aec::AecConfig::default());
        let json = serde_json::to_string(&processing).unwrap();
        assert_eq!(
            decode_capture_processing(Some(&json)).unwrap(),
            Some(processing.clone())
        );
        let newer = json.replace("\"schema_version\":1", "\"schema_version\":999");
        assert!(decode_capture_processing(Some(&newer)).is_err());
    }
}
