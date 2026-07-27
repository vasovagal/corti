//! Background job kinds and handlers — what the pipeline worker's drain loop actually runs.
//!
//! Kinds are plain strings persisted in queue.db's `jobs` table (see `corti-jobs`). Payloads carry just
//! enough to re-find durable state (a recording id): handlers are idempotent re-dispatches over the
//! `recordings` row, not bearers of state themselves, so replaying one after a crash is always safe.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use corti_core::JobStatus;
use corti_jobs::ClaimedJob;

use crate::pipeline::{self, Ctx};

/// Re-run a recording that failed transcription/filing, with backoff. Payload: `{"id": "<stem>"}`.
pub(crate) const RETRY_TRANSCRIPTION: &str = "retry_transcription";
/// The hourly retention sweep (periodic singleton; also fires at startup).
pub(crate) const SWEEP_EXPIRED: &str = "sweep_expired";

/// Attempts a recording gets via the retry job before it is terminally failed (the live-path failure
/// that scheduled the job was attempt zero).
pub(crate) const RETRY_MAX_ATTEMPTS: u32 = 5;

pub(crate) const SWEEP_PERIOD: Duration = Duration::from_secs(3600);
/// Minimum terminal-row lifetime. The actual horizon is the maximum of this and configured audio
/// retention, because deleting the path-bearing row first would orphan longer-retained audio forever.
const MIN_ROW_RETENTION_DAYS: i64 = 90;
/// Parked `failed` background-job rows are debris after this long.
const FAILED_JOB_ROW_RETENTION_DAYS: i64 = 30;

pub(crate) fn id_payload(id: &str) -> serde_json::Value {
    serde_json::json!({ "id": id })
}

/// Dispatch one claimed job by kind. An unrecognized kind fails with backoff so version skew surfaces
/// in the jobs table instead of crash-looping or silently vanishing.
pub(crate) fn run(ctx: &mut Ctx, job: &ClaimedJob) -> Result<()> {
    match job.kind.as_str() {
        RETRY_TRANSCRIPTION => retry_transcription(ctx, job),
        SWEEP_EXPIRED => sweep_expired(ctx),
        other => anyhow::bail!("unknown job kind {other:?}"),
    }
}

/// A one-shot job ran out of attempts: surface the failure on the artifact it was working for.
pub(crate) fn on_exhausted(ctx: &Ctx, job: &ClaimedJob, error: &str) {
    if job.kind != RETRY_TRANSCRIPTION {
        return;
    }
    let Some(id) = job.payload["id"].as_str() else {
        return;
    };
    let Ok(Some(row)) = ctx.queue.get(id) else {
        return;
    };
    if row.status.is_terminal() {
        return;
    }
    pipeline::fail(
        ctx,
        id,
        &row.meta(),
        format!("gave up after {} attempts: {error}", job.attempts),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryAction {
    Stale,
    FileCheckpoint,
    Transcribe,
    MissingAudio,
}

fn retry_action(status: JobStatus, checkpoint_exists: bool, audio_exists: bool) -> RetryAction {
    match status {
        JobStatus::Done | JobStatus::Failed | JobStatus::Recording => RetryAction::Stale,
        JobStatus::PendingNote if checkpoint_exists => RetryAction::FileCheckpoint,
        JobStatus::PendingTranscription | JobStatus::Transcribing | JobStatus::PendingNote
            if audio_exists =>
        {
            // `PendingNote` without a checkpoint is a legacy row: transcribe once to create one.
            RetryAction::Transcribe
        }
        JobStatus::PendingTranscription | JobStatus::Transcribing | JobStatus::PendingNote => {
            RetryAction::MissingAudio
        }
    }
}

/// Push a recording the rest of the way through the pipeline, from whatever state a failure or crash
/// left it in. `PendingNote` means the transcript is durable and dispatches straight to filing. Legacy
/// `PendingNote` rows without a checkpoint fall back to audio once for backward compatibility.
fn retry_transcription(ctx: &mut Ctx, job: &ClaimedJob) -> Result<()> {
    let id = job.payload["id"]
        .as_str()
        .context("payload missing recording id")?;
    let Some(row) = ctx.queue.get(id)? else {
        return Ok(()); // row deleted meanwhile — nothing to retry
    };
    let meta = row.meta();
    let checkpoint_exists = crate::checkpoint::path_for(&row.audio_path).is_file();
    match retry_action(row.status, checkpoint_exists, row.audio_path.exists()) {
        RetryAction::Stale => Ok(()),
        RetryAction::FileCheckpoint => pipeline::file_checkpoint(ctx, id, &meta, &row.audio_path),
        RetryAction::Transcribe => pipeline::transcribe_and_file(ctx, id, &meta, &row.audio_path),
        RetryAction::MissingAudio => {
            pipeline::fail(
                ctx,
                id,
                &meta,
                "audio file is gone — cannot transcribe".to_string(),
            );
            Ok(())
        }
    }
}

/// The retention sweep: delete expired **audio** (rows are kept — the Recording Queue's history,
/// including "Filed in brain", depends on them), then GC terminal recording rows and parked job rows
/// past their much longer horizons. `retention_days` is read live from the shared config so a Settings
/// change applies to the very next sweep.
///
/// Format-agnostic (lossless-WAV world): it deletes whatever path the queue row stores, the AEC-cleaned
/// sibling, and any leftover transcript checkpoint. The checkpoint normally disappears at completion;
/// sweeping it here contains crash leftovers.
fn sweep_expired(ctx: &mut Ctx) -> Result<()> {
    let retention_days = i64::from(ctx.config.lock().unwrap().retention_days);
    let audio_cutoff = chrono::Local::now() - chrono::Duration::days(retention_days);

    let mut files = 0usize;
    for row in ctx.queue.expired(audio_cutoff)? {
        // The recording itself (whatever path the row stores) and its AEC-cleaned sibling, when distinct.
        let audio = &row.audio_path;
        for path in [
            audio.clone(),
            corti_capture::clean_wav_path(audio),
            crate::checkpoint::path_for(audio),
        ] {
            if std::fs::remove_file(&path).is_ok() {
                files += 1;
            }
        }
    }

    let row_days = retention_days.max(MIN_ROW_RETENTION_DAYS);
    let row_cutoff = chrono::Local::now() - chrono::Duration::days(row_days);
    let rows = ctx.queue.delete_terminal_older_than(row_cutoff)?;
    let job_rows = ctx.queue.jobs().delete_terminal_older_than(
        Utc::now() - chrono::Duration::days(FAILED_JOB_ROW_RETENTION_DAYS),
    )?;

    if files + rows + job_rows > 0 {
        eprintln!(
            "[corti] sweep: deleted {files} expired audio file(s), {rows} ancient row(s), \
             {job_rows} stale job row(s)"
        );
        crate::tray::emit_queue_changed(&ctx.app, "sweep");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_dispatches_pending_note_to_checkpoint_only() {
        assert_eq!(
            retry_action(JobStatus::PendingNote, true, true),
            RetryAction::FileCheckpoint
        );
        // The durable transcript is sufficient even if the raw audio is temporarily unavailable.
        assert_eq!(
            retry_action(JobStatus::PendingNote, true, false),
            RetryAction::FileCheckpoint
        );
    }

    #[test]
    fn legacy_pending_note_falls_back_to_audio_once() {
        assert_eq!(
            retry_action(JobStatus::PendingNote, false, true),
            RetryAction::Transcribe
        );
        assert_eq!(
            retry_action(JobStatus::PendingNote, false, false),
            RetryAction::MissingAudio
        );
    }

    #[test]
    fn transcription_states_still_require_audio() {
        for status in [JobStatus::PendingTranscription, JobStatus::Transcribing] {
            assert_eq!(
                retry_action(status, true, true),
                RetryAction::Transcribe,
                "a stray checkpoint must not redefine {status:?}"
            );
            assert_eq!(
                retry_action(status, false, false),
                RetryAction::MissingAudio
            );
        }
        assert_eq!(
            retry_action(JobStatus::Done, true, true),
            RetryAction::Stale
        );
    }

    #[test]
    fn row_retention_never_precedes_audio_retention() {
        for retention_days in [1_i64, 7, 90, 91, 365] {
            let row_days = retention_days.max(MIN_ROW_RETENTION_DAYS);
            assert!(row_days >= retention_days);
        }
        assert_eq!(365_i64.max(MIN_ROW_RETENTION_DAYS), 365);
    }
}
