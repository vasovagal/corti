//! Background job kinds and handlers — what the pipeline worker's drain loop actually runs.
//!
//! Kinds are plain strings persisted in queue.db's `jobs` table (see `corti-jobs`). Payloads carry just
//! enough to re-find durable state (a recording id): handlers are idempotent re-dispatches over the
//! `recordings` row, not bearers of state themselves, so replaying one after a crash is always safe.

use anyhow::{Context, Result};
use corti_core::JobStatus;
use corti_jobs::ClaimedJob;

use crate::pipeline::{self, Ctx};

/// Re-run a recording that failed transcription/filing, with backoff. Payload: `{"id": "<stem>"}`.
pub(crate) const RETRY_TRANSCRIPTION: &str = "retry_transcription";

/// Attempts a recording gets via the retry job before it is terminally failed (the live-path failure
/// that scheduled the job was attempt zero).
pub(crate) const RETRY_MAX_ATTEMPTS: u32 = 5;

pub(crate) fn retry_payload(id: &str) -> serde_json::Value {
    serde_json::json!({ "id": id })
}

/// Dispatch one claimed job by kind. An unrecognized kind fails with backoff so version skew surfaces
/// in the jobs table instead of crash-looping or silently vanishing.
pub(crate) fn run(ctx: &mut Ctx, job: &ClaimedJob) -> Result<()> {
    match job.kind.as_str() {
        RETRY_TRANSCRIPTION => retry_transcription(ctx, job),
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

/// Push a recording the rest of the way through the pipeline, from whatever state a failure or crash
/// left it in — the same arms the startup resume used to run inline. `Done`/`Failed`/missing rows
/// complete silently: the work already happened or was superseded (idempotency).
fn retry_transcription(ctx: &mut Ctx, job: &ClaimedJob) -> Result<()> {
    let id = job.payload["id"]
        .as_str()
        .context("payload missing recording id")?;
    let Some(row) = ctx.queue.get(id)? else {
        return Ok(()); // row deleted meanwhile — nothing to retry
    };
    let meta = row.meta();
    match row.status {
        // Terminal, or a live capture that isn't ours to touch: the job is stale.
        JobStatus::Done | JobStatus::Failed | JobStatus::Recording => Ok(()),
        // ADR 0007 keeps no transcript sidecar, so a `PendingNote` re-file has nothing to file from —
        // re-run the whole transcribe→file path from the capture audio, same as a transcription retry.
        JobStatus::PendingTranscription | JobStatus::Transcribing | JobStatus::PendingNote => {
            if !row.audio_path.exists() {
                // Retrying can't bring the file back: terminal-fail the recording, complete the job.
                pipeline::fail(
                    ctx,
                    id,
                    &meta,
                    "audio file is gone — cannot transcribe".to_string(),
                );
                return Ok(());
            }
            pipeline::transcribe_and_file(ctx, id, &meta, &row.audio_path)
        }
    }
}
