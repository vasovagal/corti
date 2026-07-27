//! Background job kinds and handlers — what the pipeline worker's drain loop actually runs.
//!
//! Kinds are plain strings persisted in queue.db's `jobs` table (see `corti-jobs`). Payloads normally carry
//! a recording id; a live fallback also carries its preferred note path until the recording row confirms
//! ownership. Handlers remain idempotent re-dispatches over durable state.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use corti_core::JobStatus;
use corti_jobs::ClaimedJob;
use corti_queue::Queue;
use tracing::warn;

use crate::checkpoint::FilingCheckpoint;
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

pub(crate) fn retry_payload(id: &str, preferred_note: Option<&Path>) -> serde_json::Value {
    match preferred_note {
        Some(path) => serde_json::json!({
            "id": id,
            "preferred_note": path.to_string_lossy(),
        }),
        None => serde_json::json!({ "id": id }),
    }
}

pub(crate) fn id_payload(id: &str) -> serde_json::Value {
    retry_payload(id, None)
}

fn preferred_note(payload: &serde_json::Value) -> Option<PathBuf> {
    payload["preferred_note"].as_str().map(PathBuf::from)
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
    let note_path = preferred_note(&job.payload);
    pipeline::fail_with_note(
        ctx,
        id,
        &row.meta(),
        format!("gave up after {} attempts: {error}", job.attempts),
        note_path.as_deref(),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryAction {
    Stale,
    FileCheckpoint,
    Transcribe,
    TranscribeLegacyPendingNote,
    MissingAudio,
}

fn retry_action(status: JobStatus, checkpoint_valid: bool, audio_exists: bool) -> RetryAction {
    match status {
        JobStatus::Done | JobStatus::Failed | JobStatus::Recording => RetryAction::Stale,
        // The checkpoint is the authoritative durable phase marker even when the immediately-following
        // SQLite transition failed or the process died before it. Never repeat ASR in that window.
        JobStatus::PendingTranscription | JobStatus::Transcribing | JobStatus::PendingNote
            if checkpoint_valid =>
        {
            RetryAction::FileCheckpoint
        }
        JobStatus::PendingNote if audio_exists => {
            // Pre-checkpoint databases can contain PendingNote with only retained audio. Give that one
            // compatibility transcription a different stable AWS name: the old completed job may have had
            // its output deleted by the previous cleanup policy.
            RetryAction::TranscribeLegacyPendingNote
        }
        JobStatus::PendingTranscription | JobStatus::Transcribing if audio_exists => {
            RetryAction::Transcribe
        }
        JobStatus::PendingTranscription | JobStatus::Transcribing | JobStatus::PendingNote => {
            RetryAction::MissingAudio
        }
    }
}

fn legacy_attempt_name(id: &str) -> String {
    format!("{id}-checkpoint-v1")
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
    let checkpoint_path = crate::checkpoint::path_for(&row.audio_path);
    let checkpoint_valid = FilingCheckpoint::load(&row.audio_path).is_ok();
    if checkpoint_path.is_file() && !checkpoint_valid {
        warn!(
            target: "corti::pipeline",
            job_id = %id,
            checkpoint = %checkpoint_path.display(),
            "filing checkpoint is unreadable; retained audio will be transcribed again"
        );
    }
    let action = retry_action(row.status, checkpoint_valid, row.audio_path.exists());
    let preferred_note = preferred_note(&job.payload);
    if action != RetryAction::Stale
        && let Some(note_path) = preferred_note.as_ref()
    {
        // The payload is the authority when the original recording-row write failed. Repair the row before
        // every exit path, including missing audio/exhaustion, so terminal failure can close this note.
        ctx.queue
            .update(
                id,
                corti_queue::JobUpdate {
                    note_path: Some(note_path.clone()),
                    ..Default::default()
                },
            )
            .context("persisting preferred live note from retry payload")?;
    }
    match action {
        RetryAction::Stale => Ok(()),
        RetryAction::FileCheckpoint => {
            // Repair the row when the checkpoint rename won the race with a failed/crashed PendingNote
            // update. If this write fails, the retry remains recoverable from the checkpoint.
            if row.status != JobStatus::PendingNote {
                ctx.queue
                    .update(
                        id,
                        corti_queue::JobUpdate {
                            status: Some(JobStatus::PendingNote),
                            ..Default::default()
                        },
                    )
                    .context("promoting recovered checkpoint to PendingNote")?;
            }
            pipeline::file_checkpoint(ctx, id, &meta, &row.audio_path)
        }
        RetryAction::TranscribeLegacyPendingNote => {
            ctx.queue
                .update(
                    id,
                    corti_queue::JobUpdate {
                        transcribe_job: Some(legacy_attempt_name(id)),
                        ..Default::default()
                    },
                )
                .context("persisting legacy recovery attempt name")?;
            pipeline::transcribe_and_file(ctx, id, &meta, &row.audio_path)
        }
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
    // One instant defines both horizons. In particular, the row-GC set can never be a few milliseconds
    // larger than the artifact-deletion set at equal (90–365 day) retention.
    let now = Local::now();
    let (files, rows) = sweep_recordings(&ctx.queue, retention_days, now)?;
    let job_rows = ctx.queue.jobs().delete_terminal_older_than(
        now.with_timezone(&Utc) - chrono::Duration::days(FAILED_JOB_ROW_RETENTION_DAYS),
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

/// Delete recording artifacts and then, on the longer history horizon, their terminal rows. A row is
/// retained whenever any path cannot be removed so a later sweep still knows what to retry. Non-terminal
/// rows retain their audio, but stale atomic-write temps are always safe to remove once retention expires:
/// only the canonical checkpoint name is ever a recovery input.
fn sweep_recordings(
    queue: &Queue,
    retention_days: i64,
    now: DateTime<Local>,
) -> Result<(usize, usize)> {
    let audio_cutoff = now - chrono::Duration::days(retention_days);
    let row_cutoff = now - chrono::Duration::days(retention_days.max(MIN_ROW_RETENTION_DAYS));
    let mut files = 0usize;
    let mut rows = 0usize;

    for row in queue.all()? {
        if row.updated_at >= audio_cutoff {
            continue;
        }

        if !row.status.is_terminal() {
            files += remove_stale_checkpoint_temps(&row.id, &row.audio_path).0;
            continue;
        }

        let (removed, all_absent) = remove_recording_artifacts(&row.id, &row.audio_path);
        files += removed;
        if row.updated_at < row_cutoff && all_absent && queue.delete_terminal(&row.id)? {
            rows += 1;
        }
    }
    Ok((files, rows))
}

fn remove_recording_artifacts(id: &str, audio: &Path) -> (usize, bool) {
    let mut paths = vec![
        audio.to_path_buf(),
        corti_capture::clean_wav_path(audio),
        crate::checkpoint::path_for(audio),
    ];
    let mut all_absent = true;
    match crate::checkpoint::temporary_paths(audio) {
        Ok(temps) => paths.extend(temps),
        Err(err) => {
            all_absent = false;
            warn!(
                target: "corti::retention",
                job_id = %id,
                error = %err,
                "could not inspect temporary transcript checkpoints; retaining recording row"
            );
        }
    }
    paths.sort();
    paths.dedup();
    let (removed, paths_absent) = remove_paths(id, paths);
    (removed, all_absent && paths_absent)
}

fn remove_stale_checkpoint_temps(id: &str, audio: &Path) -> (usize, bool) {
    match crate::checkpoint::temporary_paths(audio) {
        Ok(paths) => remove_paths(id, paths),
        Err(err) => {
            warn!(
                target: "corti::retention",
                job_id = %id,
                error = %err,
                "could not inspect stale temporary transcript checkpoints"
            );
            (0, false)
        }
    }
}

fn remove_paths(id: &str, paths: Vec<PathBuf>) -> (usize, bool) {
    let mut removed = 0usize;
    let mut all_absent = true;
    for path in paths {
        match std::fs::remove_file(&path) {
            Ok(()) => removed += 1,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                all_absent = false;
                warn!(
                    target: "corti::retention",
                    job_id = %id,
                    path = %path.display(),
                    error = %err,
                    "could not remove expired recording artifact; retaining recording row"
                );
            }
        }
    }
    (removed, all_absent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{OwningApp, RecordingMeta};

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("corti-jobs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta(audio_path: PathBuf) -> RecordingMeta {
        RecordingMeta {
            started_at: Local::now(),
            ended_at: Some(Local::now()),
            owning_app: OwningApp::from_bundle_id("us.zoom.xos"),
            audio_path,
        }
    }

    #[test]
    fn any_valid_checkpoint_dispatches_to_filing_only() {
        for status in [
            JobStatus::PendingTranscription,
            JobStatus::Transcribing,
            JobStatus::PendingNote,
        ] {
            assert_eq!(
                retry_action(status, true, true),
                RetryAction::FileCheckpoint
            );
            // The durable transcript is sufficient even if the raw audio is unavailable.
            assert_eq!(
                retry_action(status, true, false),
                RetryAction::FileCheckpoint
            );
        }
    }

    #[test]
    fn legacy_pending_note_uses_a_new_stable_attempt() {
        assert_eq!(
            retry_action(JobStatus::PendingNote, false, true),
            RetryAction::TranscribeLegacyPendingNote
        );
        assert_eq!(legacy_attempt_name("recording"), "recording-checkpoint-v1");
        assert_eq!(
            retry_action(JobStatus::PendingNote, false, false),
            RetryAction::MissingAudio
        );
    }

    #[test]
    fn transcription_without_a_checkpoint_still_requires_audio() {
        for status in [JobStatus::PendingTranscription, JobStatus::Transcribing] {
            assert_eq!(retry_action(status, false, true), RetryAction::Transcribe);
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

    #[test]
    fn failed_artifact_deletion_retains_path_bearing_row() {
        let dir = test_dir("unlink-failure");
        let audio = dir.join("blocked.wav");
        // `remove_file` cannot unlink a directory: a deterministic stand-in for permission/I/O failure.
        std::fs::create_dir(&audio).unwrap();
        let queue = Queue::open_at(dir.join("queue.db")).unwrap();
        let id = queue.enqueue(&meta(audio.clone())).unwrap();
        queue.set_status(&id, JobStatus::Done).unwrap();
        let future = Local::now() + chrono::Duration::days(366);

        let (_, rows) = sweep_recordings(&queue, 365, future).unwrap();
        assert_eq!(rows, 0);
        assert!(
            queue.get(&id).unwrap().is_some(),
            "path must remain retryable"
        );

        std::fs::remove_dir(&audio).unwrap();
        let (_, rows) = sweep_recordings(&queue, 365, future).unwrap();
        assert_eq!(rows, 1);
        assert!(queue.get(&id).unwrap().is_none());
        drop(queue);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn retry_payload_durably_carries_preferred_live_note() {
        let path = Path::new("/vault/live note.md");
        let payload = retry_payload("recording", Some(path));
        assert_eq!(payload["id"], "recording");
        assert_eq!(preferred_note(&payload).as_deref(), Some(path));
        assert_eq!(preferred_note(&id_payload("recording")), None);
    }

    #[test]
    fn stale_checkpoint_temp_is_swept_from_nonterminal_row() {
        let dir = test_dir("stale-checkpoint-temp");
        let audio = dir.join("recording.wav");
        std::fs::write(&audio, b"raw").unwrap();
        let queue = Queue::open_at(dir.join("queue.db")).unwrap();
        let id = queue.enqueue(&meta(audio.clone())).unwrap();
        let checkpoint = crate::checkpoint::path_for(&audio);
        let stale = PathBuf::from(format!("{}.tmp-12345", checkpoint.display()));
        std::fs::write(&stale, b"plaintext transcript").unwrap();

        let (files, rows) =
            sweep_recordings(&queue, 1, Local::now() + chrono::Duration::days(2)).unwrap();
        assert_eq!((files, rows), (1, 0));
        assert!(!stale.exists());
        assert!(audio.exists(), "nonterminal recovery audio must remain");
        assert!(queue.get(&id).unwrap().is_some());
        drop(queue);
        std::fs::remove_dir_all(dir).ok();
    }
}
