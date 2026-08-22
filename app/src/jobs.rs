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

use crate::checkpoint::{FilingCheckpoint, OwnedNote};
use crate::pipeline::{self, Ctx};

/// Re-run a recording that failed transcription/filing, with backoff. Payload: `{"id": "<stem>"}`.
pub(crate) const RETRY_TRANSCRIPTION: &str = "retry_transcription";
/// Privacy cleanup that survives transcription/filing exhaustion.
pub(crate) const CLEANUP_AWS_STAGING: &str = "cleanup_aws_staging";
/// The hourly retention sweep (periodic singleton; also fires at startup).
pub(crate) const SWEEP_EXPIRED: &str = "sweep_expired";

/// Attempts a recording gets via the retry job before it is terminally failed (the live-path failure
/// that scheduled the job was attempt zero).
pub(crate) const RETRY_MAX_ATTEMPTS: u32 = 5;

pub(crate) const SWEEP_PERIOD: Duration = Duration::from_secs(3600);

/// Privacy-safe provenance for one durable background attempt. This value is persisted so automatic
/// rescheduling does not erase whether the work originated from a user retry or startup recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptKind {
    AutomaticRetry,
    ManualRetry,
    Recovery,
}

impl AttemptKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::AutomaticRetry => "automatic_retry",
            Self::ManualRetry => "manual_retry",
            Self::Recovery => "recovery",
        }
    }

    fn from_payload(payload: &serde_json::Value) -> Option<Self> {
        match payload["attempt_kind"].as_str() {
            Some("automatic_retry") => Some(Self::AutomaticRetry),
            Some("manual_retry") => Some(Self::ManualRetry),
            Some("recovery") => Some(Self::Recovery),
            _ => None,
        }
    }
}

/// Minimum terminal-row lifetime. The actual horizon is the maximum of this and configured audio
/// retention, because deleting the path-bearing row first would orphan longer-retained audio forever.
const MIN_ROW_RETENTION_DAYS: i64 = 90;
/// Parked `failed` background-job rows are debris after this long.
const FAILED_JOB_ROW_RETENTION_DAYS: i64 = 30;

pub(crate) fn retry_payload(
    id: &str,
    preferred_note: Option<&OwnedNote>,
    attempt_kind: AttemptKind,
) -> serde_json::Value {
    match preferred_note {
        Some(note) => serde_json::json!({
            "id": id,
            "preferred_note": note.path.to_string_lossy(),
            "note_canonical": note.canonical,
            "attempt_kind": attempt_kind.as_str(),
        }),
        None => serde_json::json!({
            "id": id,
            "attempt_kind": attempt_kind.as_str(),
        }),
    }
}

pub(crate) fn id_payload(id: &str, attempt_kind: AttemptKind) -> serde_json::Value {
    retry_payload(id, None, attempt_kind)
}

/// Project a persisted attempt provenance into the immutable tracing catalogue. Legacy retry payloads
/// predate this field and retain their historical automatic-retry interpretation.
pub(crate) fn retry_attempt_kind(payload: &serde_json::Value) -> AttemptKind {
    AttemptKind::from_payload(payload).unwrap_or(AttemptKind::AutomaticRetry)
}

pub(crate) fn trace_attempt_kind(kind: &str, payload: &serde_json::Value) -> &'static str {
    AttemptKind::from_payload(payload).map_or_else(
        || {
            if kind == RETRY_TRANSCRIPTION {
                "automatic_retry"
            } else {
                "initial"
            }
        },
        AttemptKind::as_str,
    )
}

/// Enqueue at most one active transcription retry per recording, independent of provenance or an owned
/// note path. The jobs table's generic exact-JSON dedupe cannot provide this app-level identity by itself.
pub(crate) fn enqueue_retry(
    queue: &Queue,
    id: &str,
    preferred_note: Option<&OwnedNote>,
    attempt_kind: AttemptKind,
    max_attempts: u32,
    run_at: DateTime<Utc>,
) -> Result<Option<i64>> {
    let jobs = queue.jobs();
    if jobs
        .active_for(RETRY_TRANSCRIPTION)?
        .iter()
        .any(|(payload, _)| payload["id"].as_str() == Some(id))
    {
        return Ok(None);
    }
    jobs.enqueue(
        RETRY_TRANSCRIPTION,
        &retry_payload(id, preferred_note, attempt_kind),
        max_attempts,
        run_at,
    )
}

fn preferred_note(payload: &serde_json::Value) -> Option<OwnedNote> {
    payload["preferred_note"].as_str().map(|path| OwnedNote {
        path: PathBuf::from(path),
        canonical: payload["note_canonical"].as_bool().unwrap_or(false),
    })
}

fn authoritative_note(
    checkpoint: Option<OwnedNote>,
    payload: Option<OwnedNote>,
    row: Option<PathBuf>,
) -> Option<OwnedNote> {
    match (checkpoint, payload) {
        (Some(checkpoint), _) if checkpoint.canonical => Some(checkpoint),
        (_, Some(payload)) if payload.canonical => Some(payload),
        (Some(checkpoint), _) => Some(checkpoint),
        (None, Some(payload)) => Some(payload),
        (None, None) => row.map(OwnedNote::partial),
    }
}

fn cleanup_payload(id: &str, audio: &Path) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "audio_path": audio.to_string_lossy(),
    })
}

/// Dispatch one claimed job by kind. An unrecognized kind fails with backoff so version skew surfaces
/// in the jobs table instead of crash-looping or silently vanishing.
pub(crate) fn run(ctx: &mut Ctx, job: &ClaimedJob) -> Result<()> {
    let attempt_kind = trace_attempt_kind(&job.kind, &job.payload);
    let attempt_count = job.attempts.min(1_000);
    let root = crate::offline_trace::background_job(attempt_kind, attempt_count);
    let phase = match job.kind.as_str() {
        RETRY_TRANSCRIPTION => {
            crate::offline_trace::background_retry(&root, attempt_kind, attempt_count)
        }
        CLEANUP_AWS_STAGING => {
            crate::offline_trace::background_cleanup(&root, attempt_kind, attempt_count)
        }
        SWEEP_EXPIRED => {
            crate::offline_trace::background_retention(&root, attempt_kind, attempt_count)
        }
        _ => crate::offline_trace::background_retry(&root, "other", attempt_count),
    };
    let result = root.in_scope(|| {
        phase.in_scope(|| match job.kind.as_str() {
            RETRY_TRANSCRIPTION => retry_transcription(ctx, job),
            CLEANUP_AWS_STAGING => cleanup_aws_staging(ctx, job),
            SWEEP_EXPIRED => sweep_expired(ctx),
            other => anyhow::bail!("unknown job kind {other:?}"),
        })
    });
    if result.is_ok() {
        phase.ok();
        root.ok();
    } else {
        phase.error(crate::offline_trace::ErrorCode::Other);
        root.error(crate::offline_trace::ErrorCode::Other);
    }
    result
}

/// A transcription job ran out of attempts. The recording must become terminal before the caller parks
/// the only active job; otherwise a failed SQLite write would strand nonterminal work forever.
pub(crate) fn on_exhausted(ctx: &Ctx, job: &ClaimedJob, error: &str) -> Result<()> {
    if job.kind != RETRY_TRANSCRIPTION {
        return Ok(());
    }
    let Some(id) = job.payload["id"].as_str() else {
        return Ok(());
    };
    let Some(row) = ctx.queue.get(id)? else {
        return Ok(());
    };
    if row.status.is_terminal() {
        return Ok(());
    }
    let checkpoint_note = FilingCheckpoint::load(&row.audio_path)
        .ok()
        .and_then(|checkpoint| checkpoint.owned_note());
    let note = authoritative_note(
        checkpoint_note,
        preferred_note(&job.payload),
        row.note_path.clone(),
    );
    pipeline::fail_with_note(
        ctx,
        id,
        &row.meta(),
        format!("gave up after {} attempts: {error}", job.attempts),
        note.as_ref(),
    )?;
    let terminal_row = ctx.queue.get(id)?.unwrap_or(row);
    if let Err(cleanup_error) = enqueue_aws_cleanup(&ctx.queue, &terminal_row) {
        warn!(
            target: "corti::pipeline",
            job_id = %id,
            error = %format!("{cleanup_error:#}"),
            "recording failed durably, but AWS cleanup scheduling will wait for the periodic sweep"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryAction {
    Stale,
    FileCheckpoint,
    CompleteCanonicalNote,
    Transcribe,
    TranscribeLegacyPendingNote,
    MissingAudio,
}

fn retry_action(
    status: JobStatus,
    checkpoint_valid: bool,
    canonical_completion: bool,
    audio_exists: bool,
) -> RetryAction {
    match status {
        JobStatus::Done | JobStatus::Failed => RetryAction::Stale,
        // The checkpoint is the authoritative durable phase marker even when the immediately-following
        // SQLite transition failed or the process died before it. Never repeat ASR in that window.
        _ if checkpoint_valid => RetryAction::FileCheckpoint,
        // Vagus already returned this path (or live filing finalized it). Only SQLite completion remains;
        // neither a moved path nor missing raw audio permits another ASR/add-note side effect.
        _ if canonical_completion => RetryAction::CompleteCanonicalNote,
        JobStatus::Recording => RetryAction::Stale,
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
    let checkpoint = FilingCheckpoint::load(&row.audio_path).ok();
    let checkpoint_valid = checkpoint.is_some();
    if checkpoint_path.is_file() && !checkpoint_valid {
        warn!(
            target: "corti::pipeline",
            job_id = %id,
            checkpoint = %checkpoint_path.display(),
            "filing checkpoint is unreadable; retained audio will be transcribed again"
        );
    }
    let checkpoint_note = checkpoint.as_ref().and_then(FilingCheckpoint::owned_note);
    let payload_note = preferred_note(&job.payload);
    // Canonical ownership always beats a partial path. At equal provenance the checkpoint is newer than the
    // retry payload; this prevents an old live payload from rolling a later checkpoint backward.
    let owned_note =
        authoritative_note(checkpoint_note.clone(), payload_note, row.note_path.clone());
    let canonical_completion = owned_note.as_ref().is_some_and(|note| note.canonical)
        && !checkpoint_note.as_ref().is_some_and(|note| note.canonical);
    let action = retry_action(
        row.status,
        checkpoint_valid && !canonical_completion,
        canonical_completion,
        row.audio_path.exists(),
    );
    if action != RetryAction::Stale
        && let Some(note) = owned_note.as_ref()
    {
        // Repair the row before every exit path, including missing audio/exhaustion. A newer checkpoint path
        // wins over an older payload, so this write can never roll ownership backward.
        ctx.queue
            .update(
                id,
                corti_queue::JobUpdate {
                    note_path: Some(note.path.clone()),
                    ..Default::default()
                },
            )
            .context("persisting retry-owned note")?;
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
            pipeline::file_checkpoint(ctx, id, &meta, &row.audio_path, None)
        }
        RetryAction::CompleteCanonicalNote => pipeline::complete_canonical_note(
            ctx,
            id,
            &meta,
            &row.audio_path,
            &owned_note
                .context("canonical retry missing its note path")?
                .path,
            None,
        ),
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
            pipeline::transcribe_and_file(
                ctx,
                id,
                &meta,
                &row.audio_path,
                owned_note.as_ref(),
                None,
            )
        }
        RetryAction::Transcribe => pipeline::transcribe_and_file(
            ctx,
            id,
            &meta,
            &row.audio_path,
            owned_note.as_ref(),
            None,
        ),
        RetryAction::MissingAudio => pipeline::fail_with_note(
            ctx,
            id,
            &meta,
            "audio file is gone — cannot transcribe".to_string(),
            owned_note.as_ref(),
        ),
    }
}

fn enqueue_aws_cleanup(queue: &Queue, row: &corti_queue::Job) -> Result<()> {
    if !row.status.is_terminal() || !crate::checkpoint::has_unresolved_aws_staging(&row.audio_path)
    {
        return Ok(());
    }
    queue.jobs().enqueue(
        CLEANUP_AWS_STAGING,
        &cleanup_payload(&row.id, &row.audio_path),
        u32::MAX,
        Utc::now(),
    )?;
    Ok(())
}

/// Ensure every terminal unresolved marker has an effectively non-exhausting cleanup owner. Called at
/// startup and by the periodic sweep so a crash between terminal failure and enqueue cannot strand it.
pub(crate) fn enqueue_terminal_aws_cleanup(queue: &Queue) -> Result<()> {
    for row in queue.all()? {
        enqueue_aws_cleanup(queue, &row)?;
    }
    Ok(())
}

fn cleanup_aws_staging(ctx: &mut Ctx, job: &ClaimedJob) -> Result<()> {
    let audio = job.payload["audio_path"]
        .as_str()
        .map(PathBuf::from)
        .context("AWS cleanup payload missing audio path")?;
    if let Some(id) = job.payload["id"].as_str()
        && let Some(row) = ctx.queue.get(id)?
        && !row.status.is_terminal()
    {
        return Ok(()); // a manual retry took ownership back before cleanup ran
    }
    pipeline::cleanup_aws_staging(ctx, &audio)
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
    enqueue_terminal_aws_cleanup(&ctx.queue)?;
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
        if crate::checkpoint::has_unresolved_aws_staging(&row.audio_path) {
            // The local marker is the only durable address for cloud PHI. Keep the row, raw audio, and
            // checkpoint until a cleanup job durably acknowledges deletion.
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
        crate::checkpoint::aws_staging_path_for(audio),
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
                retry_action(status, true, false, true),
                RetryAction::FileCheckpoint
            );
            // The durable transcript is sufficient even if the raw audio is unavailable.
            assert_eq!(
                retry_action(status, true, false, false),
                RetryAction::FileCheckpoint
            );
        }
    }

    #[test]
    fn legacy_pending_note_uses_a_new_stable_attempt() {
        assert_eq!(
            retry_action(JobStatus::PendingNote, false, false, true),
            RetryAction::TranscribeLegacyPendingNote
        );
        assert_eq!(legacy_attempt_name("recording"), "recording-checkpoint-v1");
        assert_eq!(
            retry_action(JobStatus::PendingNote, false, false, false),
            RetryAction::MissingAudio
        );
    }

    #[test]
    fn transcription_without_a_checkpoint_still_requires_audio() {
        for status in [JobStatus::PendingTranscription, JobStatus::Transcribing] {
            assert_eq!(
                retry_action(status, false, false, true),
                RetryAction::Transcribe
            );
            assert_eq!(
                retry_action(status, false, false, false),
                RetryAction::MissingAudio
            );
        }
        assert_eq!(
            retry_action(JobStatus::Done, true, false, true),
            RetryAction::Stale
        );
    }

    #[test]
    fn canonical_note_retries_completion_without_checkpoint_or_audio() {
        for status in [
            JobStatus::Recording,
            JobStatus::PendingTranscription,
            JobStatus::Transcribing,
            JobStatus::PendingNote,
        ] {
            assert_eq!(
                retry_action(status, false, true, false),
                RetryAction::CompleteCanonicalNote
            );
        }
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
    fn retry_payload_durably_carries_note_and_attempt_provenance() {
        let partial = OwnedNote::partial("/vault/live note.md");
        let payload = retry_payload("recording", Some(&partial), AttemptKind::AutomaticRetry);
        assert_eq!(payload["id"], "recording");
        assert_eq!(payload["attempt_kind"], "automatic_retry");
        assert_eq!(preferred_note(&payload), Some(partial));

        let canonical = OwnedNote::canonical("/vault/filed note.md");
        let payload = retry_payload("recording", Some(&canonical), AttemptKind::Recovery);
        assert_eq!(payload["attempt_kind"], "recovery");
        assert_eq!(preferred_note(&payload), Some(canonical));
        assert_eq!(
            preferred_note(&id_payload("recording", AttemptKind::ManualRetry)),
            None
        );
    }

    #[test]
    fn every_retry_attempt_kind_projects_to_the_catalogue_and_survives_payload_rewrites() {
        for (kind, expected) in [
            (AttemptKind::AutomaticRetry, "automatic_retry"),
            (AttemptKind::ManualRetry, "manual_retry"),
            (AttemptKind::Recovery, "recovery"),
        ] {
            let payload = retry_payload("recording", None, kind);
            assert_eq!(trace_attempt_kind(RETRY_TRANSCRIPTION, &payload), expected);
            let rewritten = retry_payload(
                "recording",
                Some(&OwnedNote::partial("/vault/note.md")),
                retry_attempt_kind(&payload),
            );
            assert_eq!(
                trace_attempt_kind(RETRY_TRANSCRIPTION, &rewritten),
                expected
            );
        }
        assert_eq!(
            trace_attempt_kind(RETRY_TRANSCRIPTION, &serde_json::json!({ "id": "legacy" })),
            "automatic_retry"
        );
        assert_eq!(
            trace_attempt_kind(SWEEP_EXPIRED, &serde_json::json!({})),
            "initial"
        );
    }

    #[test]
    fn retry_enqueue_dedupes_recording_identity_across_attempt_provenance() {
        let dir = test_dir("retry-attempt-dedupe");
        let queue = Queue::open_at(dir.join("queue.db")).unwrap();
        assert!(
            enqueue_retry(
                &queue,
                "recording",
                None,
                AttemptKind::AutomaticRetry,
                RETRY_MAX_ATTEMPTS,
                Utc::now(),
            )
            .unwrap()
            .is_some()
        );
        assert!(
            enqueue_retry(
                &queue,
                "recording",
                None,
                AttemptKind::Recovery,
                RETRY_MAX_ATTEMPTS,
                Utc::now(),
            )
            .unwrap()
            .is_none()
        );
        let active = queue.jobs().active_for(RETRY_TRANSCRIPTION).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0["attempt_kind"], "automatic_retry");
        drop(queue);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn canonical_provenance_wins_then_checkpoint_wins_equal_provenance() {
        let checkpoint_canonical = OwnedNote::canonical("/brain/new-canonical.md");
        let payload_partial = OwnedNote::partial("/brain/old-partial.md");
        assert_eq!(
            authoritative_note(
                Some(checkpoint_canonical.clone()),
                Some(payload_partial),
                None,
            ),
            Some(checkpoint_canonical)
        );

        let checkpoint_partial = OwnedNote::partial("/brain/new-partial.md");
        let payload_canonical = OwnedNote::canonical("/brain/returned-canonical.md");
        assert_eq!(
            authoritative_note(
                Some(checkpoint_partial.clone()),
                Some(payload_canonical.clone()),
                None,
            ),
            Some(payload_canonical)
        );
        assert_eq!(
            authoritative_note(
                Some(checkpoint_partial.clone()),
                Some(OwnedNote::partial("/brain/old-partial.md")),
                None,
            ),
            Some(checkpoint_partial)
        );
    }

    #[test]
    fn live_preferred_note_with_checkpoint_retries_filing_only_without_audio() {
        let note = OwnedNote::partial("/vault/live note.md");
        let payload = retry_payload("recording", Some(&note), AttemptKind::AutomaticRetry);
        assert_eq!(preferred_note(&payload), Some(note));
        assert_eq!(
            retry_action(JobStatus::PendingTranscription, true, false, false),
            RetryAction::FileCheckpoint,
            "a live fallback path must not pull a durable checkpoint back across ASR"
        );
    }

    #[test]
    fn unresolved_aws_owner_blocks_artifact_and_row_retention() {
        let dir = test_dir("aws-owner-retention");
        let audio = dir.join("recording.wav");
        std::fs::write(&audio, b"raw").unwrap();
        let queue = Queue::open_at(dir.join("queue.db")).unwrap();
        let id = queue.enqueue(&meta(audio.clone())).unwrap();
        queue.set_status(&id, JobStatus::Failed).unwrap();
        let staging = crate::checkpoint::AwsStaging {
            bucket: "private".into(),
            key_prefix: "corti/".into(),
            job_name: id.clone(),
            region: Some("us-east-1".into()),
        };
        staging.store(&audio).unwrap();

        let future = Local::now() + chrono::Duration::days(366);
        assert_eq!(sweep_recordings(&queue, 1, future).unwrap(), (0, 0));
        assert!(audio.exists());
        assert!(crate::checkpoint::aws_staging_path_for(&audio).exists());
        assert!(queue.get(&id).unwrap().is_some());

        crate::checkpoint::AwsStaging::remove(&audio).unwrap();
        let (files, rows) = sweep_recordings(&queue, 1, future).unwrap();
        assert_eq!((files, rows), (1, 1));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn terminal_aws_owner_gets_a_deduplicated_non_exhausting_cleanup_job() {
        let dir = test_dir("aws-cleanup-job");
        let audio = dir.join("recording.wav");
        let queue = Queue::open_at(dir.join("queue.db")).unwrap();
        let id = queue.enqueue(&meta(audio.clone())).unwrap();
        queue.set_status(&id, JobStatus::Failed).unwrap();
        crate::checkpoint::AwsStaging {
            bucket: "private".into(),
            key_prefix: "corti/".into(),
            job_name: id,
            region: None,
        }
        .store(&audio)
        .unwrap();

        enqueue_terminal_aws_cleanup(&queue).unwrap();
        enqueue_terminal_aws_cleanup(&queue).unwrap();
        let jobs = queue.jobs().active_for(CLEANUP_AWS_STAGING).unwrap();
        assert_eq!(jobs.len(), 1);
        let claimed = queue.jobs().claim_due(Utc::now()).unwrap().unwrap();
        assert_eq!(claimed.kind, CLEANUP_AWS_STAGING);
        assert_eq!(claimed.max_attempts, u32::MAX);
        std::fs::remove_dir_all(dir).ok();
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
