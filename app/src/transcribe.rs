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
use corti_transcribe::segment::{CleanupConfig, SpanEvidence, cleanup, cleanup_with_evidence};

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
        if let BackendKind::Unavailable(reason) = &kind {
            error!(target: "corti::transcribe", "{reason}");
        }
        Self { cfg, kind }
    }

    /// Whether this backend's transcript timestamps may be compared against a `-aec-stats.json` sidecar.
    ///
    /// Only the local backend. AWS stays text-only by decision (#149 phase 3b): its jobs are the path where
    /// corti is least certain the audio it timestamped is the audio corti's own canceller produced, and the
    /// upside is nil — the shipping default is local, and a wrong answer here silently deletes transcript
    /// rows. A build with neither backend compiled in answers `false` and never gets that far anyway.
    pub fn audio_evidence_supported(&self) -> bool {
        match &self.kind {
            #[cfg(feature = "aws")]
            BackendKind::Aws(_) => false,
            #[cfg(feature = "local")]
            BackendKind::Local => true,
            BackendKind::Unavailable(_) => false,
        }
    }

    /// The deterministic segment-cleanup rules this backend's config snapshot selects. Taken from the
    /// same immutable `AppConfig` the backend was built with, so a Settings edit mid-job cannot change the
    /// rules half-way through a transcript.
    pub fn cleanup_config(&self) -> CleanupConfig {
        self.cfg.cleanup_config()
    }

    /// Provenance with the recording's durable AEC execution record rather than current Settings.
    /// `audio_evidence` is the runtime answer from [`transcribe_recording`]: whether the segment cleanup
    /// actually had per-block AEC statistics for this recording.
    pub fn provenance_with_aec(
        &self,
        mode: corti_vagus::provenance::GenerationMode,
        aec: crate::provenance::AecExecution<'_>,
        audio_evidence: bool,
    ) -> corti_vagus::provenance::TranscriptProvenance {
        crate::provenance::from_config_with_aec(&self.cfg, mode, aec, audio_evidence)
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

/// Fold the AEC's per-block record over `[start, end]` into the four numbers the segment cleanup asks for
/// (#149 phase 3b). `corti-transcribe` deliberately does not depend on `corti-aec`, so the adaptation
/// happens here, in the one crate that already links both.
///
/// `blocks` must be sorted by `t_start_secs`, which every producer of them is: the ring drains in order and
/// the sidecar is written in drain order.
pub fn span_evidence(
    blocks: &[corti_aec::BlockStats],
    start: f64,
    end: f64,
) -> Option<SpanEvidence> {
    let s = corti_aec::span_stats(blocks, start, end)?;
    Some(SpanEvidence {
        mic_db: s.mean_mic_db,
        echo_estimate_db: s.mean_echo_estimate_db,
        double_talk_fraction: s.double_talk_fraction,
        blocks: s.blocks,
    })
}

/// Read the `-aec-stats.json` sidecar written beside a recording, if there is one.
///
/// Absent is the ordinary case, not an error: the AWS backend has no canceller, foreign audio never had
/// one, and a capture whose AEC was disabled wrote no sidecar. A *corrupt* or wrong-schema sidecar is
/// logged and treated as absent — the cleanup falls back to its text rules rather than failing a
/// transcription over a diagnostic file.
fn load_aec_block_stats(path: &Path) -> Option<Vec<corti_aec::BlockStats>> {
    let bytes = std::fs::read(path).ok()?;
    match serde_json::from_slice::<corti_capture::AecStatsFile>(&bytes) {
        Ok(record) if record.schema_version == corti_capture::AEC_STATS_SCHEMA_VERSION => {
            info!(
                target: "corti::transcribe",
                path = %path.display(),
                blocks = record.blocks.len(),
                dropped = record.stats_dropped,
                delay_samples = record.delay_samples as u64,
                "AEC block statistics available as cleanup evidence"
            );
            Some(record.blocks)
        }
        Ok(record) => {
            warn!(
                target: "corti::transcribe",
                path = %path.display(),
                schema_version = record.schema_version,
                "ignoring an AEC statistics sidecar written by a different schema"
            );
            None
        }
        Err(e) => {
            warn!(
                target: "corti::transcribe",
                path = %path.display(),
                error = %e,
                "ignoring an unreadable AEC statistics sidecar"
            );
            None
        }
    }
}

/// Optionally run the file-to-file AEC pass, then transcribe with the runtime-selected `backend`. This is
/// the tray-free, queue-free transcription core shared by the pipeline worker
/// ([`crate::pipeline::transcribe_and_file`]) and the `--redo`/`--input` CLI ([`crate::cli`]). Returns the
/// transcript plus the audio path actually fed to the backend (the cleaned WAV when AEC ran, else the raw
/// input) for logging, and whether the cleanup had per-block AEC statistics to work from. Persisting any
/// transcript sidecar is a caller's concern, not this primitive's.
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
) -> Result<(DiarizedTranscript, PathBuf, OfflineAecOutcome, bool)> {
    // Clean speaker bleed on disk before transcription (backend-agnostic). The input file is never
    // touched. A tap-only ("webinar") recording has no mic track, so AEC is skipped deliberately (not an
    // error); a genuine AEC failure falls back to the raw recording so the pipeline never stalls.
    let (input, aec_outcome): (PathBuf, OfflineAecOutcome) = if let Some(aec) = aec {
        // Ask for the per-block record while we are running the filter anyway: it is the only chance this
        // path gets, and it is what lets the echo pass tell a residual-bleed row from a coincidence
        // (#149 phase 3b). The cleaned WAV is byte-identical either way.
        match corti_capture::write_clean_wav_with_options(
            raw_audio,
            &aec.config,
            aec.lookahead_seconds,
            corti_capture::AecStatsSidecar::Write,
        ) {
            Ok(Some(clean)) => {
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

    let mut transcript = backend.transcribe(attempt.aws_job_name, &input, meta)?;

    // Deterministic segment cleanup (#149) on the merged timeline the backend returned — the one place
    // where the two channels can see each other. This covers local, AWS and `corti --input`, and runs
    // before the transcript reaches the checkpoint, the note, or the LLM tier.
    //
    // The `-aec-stats.json` sidecar always sits beside the **raw** recording (`write_clean_wav*` names it
    // from its input), whether it was written by the pass above or by the capture writer, so one lookup
    // covers both. Its block times are on the cleaned timeline, which is the timeline the backend just
    // timestamped — one emitted sample per input sample, lookahead already subtracted.
    let blocks = backend
        .audio_evidence_supported()
        .then(|| load_aec_block_stats(&corti_capture::aec_stats_path(raw_audio)))
        .flatten();
    let audio_evidence = blocks.is_some();
    let cleanup_cfg = backend.cleanup_config();
    if !cleanup_cfg.is_noop() {
        let segments_in = transcript.segments.len();
        let segments_taken = std::mem::take(&mut transcript.segments);
        let (segments, stats) = match blocks.as_deref() {
            Some(blocks) => {
                let evidence = |start: f64, end: f64| span_evidence(blocks, start, end);
                cleanup_with_evidence(segments_taken, &cleanup_cfg, &[], Some(&evidence))
            }
            None => cleanup(segments_taken, &cleanup_cfg, &[]),
        };
        transcript.segments = segments;
        info!(
            target: "corti::transcribe",
            job_id = %attempt.id,
            segments_in,
            segments_out = transcript.segments.len(),
            audio_evidence,
            echo_dropped_me = stats.echo_dropped_me,
            echo_dropped_them = stats.echo_dropped_them,
            echo_dropped_audio = stats.echo_dropped_audio,
            merged = stats.merged,
            backchannels_dropped = stats.backchannels_dropped,
            "segment cleanup applied"
        );
    }

    Ok((transcript, input, aec_outcome, audio_evidence))
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

    fn block(
        t_start_secs: f64,
        mic_energy: f32,
        echo_estimate_energy: f32,
    ) -> corti_aec::BlockStats {
        corti_aec::BlockStats {
            t_start_secs,
            mic_energy,
            far_energy: 1.0,
            echo_estimate_energy,
            error_energy: mic_energy,
            double_talk: false,
            suppressed: true,
        }
    }

    fn stats_file(blocks: Vec<corti_aec::BlockStats>) -> corti_capture::AecStatsFile {
        corti_capture::AecStatsFile {
            schema_version: corti_capture::AEC_STATS_SCHEMA_VERSION,
            source: "recording.wav".into(),
            sample_rate: 48_000,
            frames: 48_000,
            lookahead_seconds: 7.0,
            config: corti_aec::AecConfig::default(),
            delay_samples: 128,
            delay_ms: 2.67,
            block_hop_samples: 8_192,
            block_hop_secs: 8_192.0 / 48_000.0,
            stats_dropped: 0,
            blocks,
        }
    }

    /// The sidecar is read when it is there, ignored when it is not, and never fails a transcription:
    /// missing, corrupt and future-schema files all mean "text rules only".
    #[test]
    fn aec_statistics_sidecar_loads_when_present_and_is_skipped_otherwise() {
        let dir = std::env::temp_dir().join(format!("corti-aec-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let raw = dir.join("recording.wav");
        let path = corti_capture::aec_stats_path(&raw);
        assert_eq!(path, dir.join("recording-aec-stats.json"));

        let _ = std::fs::remove_file(&path);
        assert!(load_aec_block_stats(&path).is_none(), "absent ⇒ text-only");

        let record = stats_file(vec![block(0.0, 4.0, 1.0), block(0.2, 1.0, 1.0)]);
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        let blocks = load_aec_block_stats(&path).expect("a well-formed sidecar loads");
        assert_eq!(blocks.len(), 2);

        // The dB math the cleanup sees: 4:1 in energy is ~6 dB above the estimate, 1:1 is 0 dB.
        let loud = span_evidence(&blocks, 0.0, 0.1).unwrap();
        assert_eq!(loud.blocks, 1);
        assert!((loud.mic_db - loud.echo_estimate_db - 6.02).abs() < 0.05);
        assert_eq!(loud.double_talk_fraction, 0.0);
        let quiet = span_evidence(&blocks, 0.2, 0.3).unwrap();
        assert!((quiet.mic_db - quiet.echo_estimate_db).abs() < 1e-3);
        // Before the first block there is nothing to say.
        assert!(span_evidence(&blocks, -5.0, -1.0).is_none());

        let mut future = record.clone();
        future.schema_version = corti_capture::AEC_STATS_SCHEMA_VERSION + 1;
        std::fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();
        assert!(
            load_aec_block_stats(&path).is_none(),
            "a schema this build does not know is not evidence"
        );

        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_aec_block_stats(&path).is_none(), "corrupt ⇒ text-only");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
