//! The pipeline worker: the single thread that owns the [`Queue`] and pushes every recording through
//! `enqueue → transcribe → file → Done`.
//!
//! Why one thread: rusqlite's `Connection` is `Send` but not `Sync`, so the `Queue` has exactly one owner
//! here and is never shared. Transcription blocks (the backend runs its own runtime), so doing it on this
//! dedicated thread keeps it off the Tauri UI loop (guardrail 9). Jobs run serially — fine, since
//! transcription is inherently sequential and a second call simply waits in the channel.
//!
//! ## Durable post-ASR filing boundary
//! A successful backend result is atomically checkpointed beside the retained raw recording before the row
//! enters `PendingNote`. Filing retries load that small checkpoint and never rerun ASR; transcription retries
//! still use the raw audio. The hourly sweep is the only raw-audio deletion authority. Startup narrowly
//! recovers every valid post-ASR checkpoint; broader first-attempt reconciliation remains deferred.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use corti_core::{JobStatus, RecordingMeta};
use corti_jobs::{Backoff, ClaimedJob, FailOutcome};
use corti_queue::{Job, JobUpdate, Queue};
use corti_vagus::Vagus;
use tauri::AppHandle;
use tracing::{error, info, warn};

use crate::checkpoint::{FilingCheckpoint, path_for as checkpoint_path};
use crate::imp::{HISTORY_LIMIT, HistoryEntry, Stage, set_stage};
use crate::settings::SharedConfig;
use crate::transcribe::Backend;
use crate::tray;

/// Backoff for one-shot background jobs: 1 m, 2 m, 4 m, … capped at 1 h.
const JOB_BACKOFF: Backoff = Backoff {
    base: Duration::from_secs(60),
    cap: Duration::from_secs(3600),
};

/// Longest the worker blocks on the channel before re-checking the jobs table — bounds how late a
/// periodic job can fire when no message wakes the loop sooner.
const MAX_IDLE_WAIT: Duration = Duration::from_secs(60);

/// Work for the pipeline worker.
pub enum PipelineMsg {
    /// A finished recording to push through transcribe → file → Done.
    Process {
        meta: RecordingMeta,
        audio_path: PathBuf,
    },
    /// The Recording Queue window's Retry button: reset a `Failed` recording and re-run it with a
    /// fresh attempt budget (a deliberate user action earns one).
    Retry { id: String },
    /// Re-read the shared config and rebuild the backend + AEC toggle. Sent by the Settings screen on save;
    /// handled between jobs (the worker is serial) so it applies to the next recording — or immediately when
    /// the worker is idle and blocked waiting for one.
    ReloadConfig,
    /// #87: the live thread created the inbox note mid-recording. Persist its path into a durable row
    /// right away — the no-double-notes guarantee and crash-recovery rewrites key on the row's
    /// `note_path`.
    LiveNoteCreated {
        meta: RecordingMeta,
        note_path: PathBuf,
    },
    /// #87: the recording ended without a `Process` (discarded as too short, or its capture failed to
    /// finish). Tear down the live session — it deletes its own partial note — and close any queue
    /// row `LiveNoteCreated` made.
    LiveDiscarded { id: String },
}

/// Everything the worker owns. Built once on the worker thread; never shared.
pub(crate) struct Ctx {
    pub(crate) queue: Queue,
    /// Discovered at startup; if that failed, re-probed on each filing attempt (see [`file_and_done`])
    /// so installing vagus mid-session works without a relaunch. `Err` is the stringified, user-facing
    /// reason shown when filing fails — we still record + transcribe rather than blocking the whole
    /// pipeline at startup.
    vagus: Result<Vagus, String>,
    backend: Backend,
    /// Whether to clean speaker bleed (offline AEC) before transcribing. Captured from config at startup.
    aec_enabled: bool,
    aec_config: corti_aec::AecConfig,
    pub(crate) app: AppHandle,
    /// Clone of the managed stats buffer; the worker records coarse stage wall-clock here.
    stats: crate::stats::StatsBuffer,
    /// Low-cardinality backend label for stage samples; refreshed on a live backend switch.
    backend_label: &'static str,
    /// The live runtime config — the retention sweep reads `retention_days` from here on every run, so
    /// a Settings change applies to the next sweep without any reload plumbing.
    pub(crate) config: SharedConfig,
    /// #87: the live-filing sessions the detector hook spawns; the worker finalizes (or discards) them.
    pub(crate) live: Arc<crate::live::LiveManager>,
}

/// Worker entry point. Opens the queue, seeds the tray history, then drains the channel until the app
/// exits. Holds the [`SharedConfig`] so a Settings save (`PipelineMsg::ReloadConfig`) can rebuild the
/// backend live.
pub fn run(
    app: AppHandle,
    config: SharedConfig,
    rx: Receiver<PipelineMsg>,
    stats: crate::stats::StatsBuffer,
    live: Arc<crate::live::LiveManager>,
) {
    let queue = match Queue::open() {
        Ok(q) => q,
        Err(e) => {
            error!(target: "corti::pipeline", error = %format!("{e:#}"), "cannot open job queue");
            tray::set_status(&app, format!("⚠ queue unavailable: {e}"));
            return;
        }
    };

    let vagus = Vagus::discover().map_err(|e| format!("{e:#}"));
    if let Err(e) = &vagus {
        warn!(target: "corti::pipeline", error = %e, "vagus not available — notes can't be filed");
    }

    // Snapshot the shared config to build the initial backend. Capture the AEC toggle before `Backend::init`
    // consumes the snapshot.
    let cfg = config.lock().unwrap().clone();
    let aec_enabled = cfg.aec_enabled;
    let aec_config = cfg.aec_config();
    let backend_label = cfg.backend_label(); // read BEFORE Backend::init consumes cfg
    let mut ctx = Ctx {
        queue,
        vagus,
        backend: Backend::init(cfg),
        aec_enabled,
        aec_config,
        app,
        stats,
        backend_label,
        config: config.clone(),
        live,
    };

    seed_history(&ctx);
    match ctx.queue.jobs().recover_running() {
        Ok(0) => {}
        Ok(n) => eprintln!("[corti] re-queued {n} background job(s) orphaned by the last shutdown"),
        Err(e) => eprintln!("[corti] cannot recover background jobs: {e:#}"),
    }
    // Narrow first-attempt recovery at the new durable boundary: if the checkpoint exists, ASR is already
    // done regardless of whether the adjacent PendingNote write committed. Other first-attempt states
    // remain outside this PR's startup-reconciliation scope.
    for row in recover_filing_checkpoints(&ctx.queue) {
        eprintln!(
            "[corti] recovering durable filing checkpoint for {}",
            row.id
        );
        tray::update_history(
            &ctx.app,
            &row.id,
            JobStatus::PendingNote,
            None,
            row.error,
            row.note_path,
        );
    }
    // #87: revive/close out rows stranded at `Recording` by a quit or crash mid-call. Without this
    // they sit non-terminal forever, their notes stay at `State: transcribing`, and the retention
    // sweep (which only matches terminal rows) never reclaims their audio.
    for (row, outcome) in reap_recording_rows(&ctx.queue) {
        match outcome {
            Reaped::Retrying => {
                eprintln!(
                    "[corti] recovering recording {} stranded by the last shutdown",
                    row.id
                );
                tray::update_history(
                    &ctx.app,
                    &row.id,
                    JobStatus::PendingTranscription,
                    None,
                    None,
                    None,
                );
            }
            Reaped::Failed(msg) => {
                tray::update_history(&ctx.app, &row.id, JobStatus::Failed, None, Some(msg), None);
            }
        }
    }
    // The retention sweep runs hourly AND right now (enqueue_periodic arms it due immediately).
    if let Err(e) = ctx
        .queue
        .jobs()
        .enqueue_periodic(crate::jobs::SWEEP_EXPIRED, crate::jobs::SWEEP_PERIOD)
    {
        eprintln!("[corti] cannot schedule the retention sweep: {e:#}");
    }
    tray::set_status(&ctx.app, "Idle — waiting for a call".to_string());

    // Messages take priority (they're user-facing); due background jobs drain after each wake, so a
    // recording never queues behind a backlog of background work. Everything stays serial on this one
    // thread — a job mid-run delays the next message exactly like a long transcription always has.
    loop {
        match rx.recv_timeout(next_wake(&ctx)) {
            Ok(PipelineMsg::Process { meta, audio_path }) => match ctx.queue.enqueue(&meta) {
                Ok(id) => {
                    info!(
                        target: "corti::pipeline",
                        job_id = %id,
                        app = %meta.owning_app.name,
                        path = %audio_path.display(),
                        "enqueued recording for transcription"
                    );
                    // The capture-start site already pushed a `Recording` entry (keyed by this same id);
                    // advance it to `Queued` and stamp the now-known end time before transcribing.
                    tray::update_history(
                        &ctx.app,
                        &id,
                        JobStatus::PendingTranscription,
                        meta.ended_at,
                        None,
                        None,
                    );
                    // #87: a row created mid-call by LiveNoteCreated has no end time (this enqueue was
                    // an INSERT OR IGNORE no-op for it) — stamp it now that the recording is over.
                    if meta.ended_at.is_some()
                        && let Err(e) = ctx.queue.update(
                            &id,
                            JobUpdate {
                                ended_at: meta.ended_at,
                                ..Default::default()
                            },
                        )
                    {
                        warn!(
                            target: "corti::pipeline",
                            job_id = %id,
                            error = %format!("{e:#}"),
                            "could not persist the recording end time"
                        );
                    }
                    // #87: if a live session filed this recording as it happened, the job is already
                    // done — batch transcription is skipped entirely. Any other live outcome (no
                    // session, no note, error) falls back to today's batch path; a partial live note
                    // is persisted first so `file_and_done` rewrites it instead of double-filing.
                    match ctx.live.finalize(&id) {
                        Some(crate::live::LiveOutcome::Filed { note_path }) => {
                            if let Err(e) = live_filed(&mut ctx, &id, &meta, &audio_path, note_path)
                            {
                                schedule_retry(&ctx, &id, &meta, e);
                            }
                        }
                        live_result => {
                            if let Some(crate::live::LiveOutcome::Failed { error, note_path }) =
                                live_result
                            {
                                warn!(
                                    target: "corti::live",
                                    job_id = %id,
                                    error = %error,
                                    "live filing failed — falling back to batch transcription"
                                );
                                if let Some(partial) = note_path
                                    && let Err(e) = ctx.queue.update(
                                        &id,
                                        JobUpdate {
                                            note_path: Some(partial),
                                            ..Default::default()
                                        },
                                    )
                                {
                                    warn!(
                                        target: "corti::live",
                                        job_id = %id,
                                        error = %format!("{e:#}"),
                                        "could not persist the partial live note path"
                                    );
                                }
                            }
                            if let Err(e) = transcribe_and_file(&mut ctx, &id, &meta, &audio_path) {
                                schedule_retry(&ctx, &id, &meta, e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(
                        target: "corti::pipeline",
                        path = %audio_path.display(),
                        error = %format!("{e:#}"),
                        "enqueue failed"
                    );
                    // #87: still settle any live session so its note isn't stranded at
                    // `State: transcribing` — with no queue row the outcome can only be logged.
                    match ctx.live.finalize(&corti_queue::job_id(&meta)) {
                        Some(crate::live::LiveOutcome::Filed { note_path }) => warn!(
                            target: "corti::live",
                            note_path = %note_path.display(),
                            "live note filed but its recording row could not be created"
                        ),
                        Some(crate::live::LiveOutcome::Failed { error, .. }) => warn!(
                            target: "corti::live",
                            error = %error,
                            "live session failed alongside the enqueue failure"
                        ),
                        Some(crate::live::LiveOutcome::NoNote) | None => {}
                    }
                    // No job id yet, so `fail()` (which resets the stage) is never reached — reset here so
                    // the diagram doesn't sit on Transcribing forever.
                    set_stage(&ctx.app, Stage::Idle);
                    tray::set_status(&ctx.app, format!("⚠ enqueue failed: {e}"));
                }
            },
            Ok(PipelineMsg::Retry { id }) => manual_retry(&mut ctx, &id),
            Ok(PipelineMsg::ReloadConfig) => reload_config(&mut ctx, &config),
            Ok(PipelineMsg::LiveNoteCreated { meta, note_path }) => {
                live_note_created(&ctx, &meta, note_path)
            }
            Ok(PipelineMsg::LiveDiscarded { id }) => live_discarded(&ctx, &id),
            // Nothing arrived before the next background job came due — fall through to the drain.
            Err(RecvTimeoutError::Timeout) => {}
            // Every sender is gone: the app is shutting down.
            Err(RecvTimeoutError::Disconnected) => break,
        }
        drain_due_jobs(&mut ctx);
    }
}

/// How long to block on the channel: until the next pending background job is due, clamped to
/// `[0, MAX_IDLE_WAIT]`. No pending jobs (or a read error) ⇒ the max — the drain re-checks anyway.
fn next_wake(ctx: &Ctx) -> Duration {
    match ctx.queue.jobs().next_due_at() {
        Ok(Some(due)) => (due - Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO)
            .min(MAX_IDLE_WAIT),
        Ok(None) => MAX_IDLE_WAIT,
        Err(e) => {
            eprintln!("[corti] cannot read next job due time: {e:#}");
            MAX_IDLE_WAIT
        }
    }
}

/// Claim and run every due background job, oldest due first.
fn drain_due_jobs(ctx: &mut Ctx) {
    loop {
        let job = match ctx.queue.jobs().claim_due(Utc::now()) {
            Ok(Some(j)) => j,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[corti] cannot claim background job: {e:#}");
                return;
            }
        };
        let result = crate::jobs::run(ctx, &job);
        finish_job(ctx, &job, result);
    }
}

/// Settle a finished job: completed ⇒ delete (or re-arm a periodic); failed ⇒ backoff or park (and let
/// the kind surface the exhaustion on whatever artifact it was working for).
fn finish_job(ctx: &Ctx, job: &ClaimedJob, result: Result<()>) {
    match result {
        Ok(()) => {
            if let Err(e) = ctx.queue.jobs().complete(job) {
                eprintln!(
                    "[corti] cannot complete job {} ({}): {e:#}",
                    job.id, job.kind
                );
            }
        }
        Err(err) => {
            let msg = format!("{err:#}");
            eprintln!("[corti] job {} ({}) failed: {msg}", job.id, job.kind);
            match ctx.queue.jobs().fail(job, &msg, &JOB_BACKOFF) {
                Ok(FailOutcome::Rescheduled { next_run_at }) => {
                    eprintln!("[corti]   will retry at {next_run_at}");
                    if job.kind == crate::jobs::RETRY_TRANSCRIPTION {
                        record_retry_backoff(ctx, job, &msg);
                    }
                }
                Ok(FailOutcome::Exhausted) => {
                    eprintln!("[corti]   out of attempts — parked as failed");
                    crate::jobs::on_exhausted(ctx, job, &msg);
                }
                Err(e) => eprintln!("[corti] cannot record job failure: {e:#}"),
            }
            // A retry attempt's `transcribe_and_file` left the stage at Transcribing/Filing; that attempt
            // just ended (parked in backoff, exhausted → `fail`, or a bookkeeping error), so drop back to
            // Idle. Gated to the transcription retry so the retention sweep never disturbs the stage — it
            // runs on this same thread while a capture may be live (Recording is owned by the detector).
            if job.kind == crate::jobs::RETRY_TRANSCRIPTION {
                set_stage(&ctx.app, Stage::Idle);
            }
        }
    }
}

fn retry_status(row: &Job, audio: &Path) -> JobStatus {
    if row.status == JobStatus::PendingNote || FilingCheckpoint::load(audio).is_ok() {
        JobStatus::PendingNote
    } else {
        JobStatus::PendingTranscription
    }
}

/// Reflect a durable retry job's backoff on the recording row. A filing failure remains `PendingNote`
/// because its checkpoint is the recovery input; a transcription failure returns to
/// `PendingTranscription`. This keeps the Queue window truthful while no attempt is actively running.
fn record_retry_backoff(ctx: &Ctx, job: &ClaimedJob, error: &str) {
    let Some(id) = job.payload["id"].as_str() else {
        return;
    };
    let Ok(Some(row)) = ctx.queue.get(id) else {
        return;
    };
    if row.status.is_terminal() {
        return;
    }
    let status = retry_status(&row, &row.audio_path);
    if let Err(e) = ctx.queue.update(
        id,
        JobUpdate {
            status: Some(status),
            error: Some(error.to_string()),
            ..Default::default()
        },
    ) {
        warn!(
            target: "corti::pipeline",
            job_id = %id,
            error = %format!("{e:#}"),
            "could not reflect retry backoff on recording"
        );
        return;
    }
    tray::update_history(&ctx.app, id, status, None, Some(error.to_string()), None);
    tray::set_status(
        &ctx.app,
        format!("⚠ {} — will retry: {error}", row.owning_app),
    );
}

/// The Queue window's Retry button. A `Failed` recording qualifies while either raw audio or a filing
/// checkpoint survives (the UI normally keys on retained raw audio; re-check here on the owner thread).
/// Old failed-job debris is cleared so the fresh enqueue gets a clean
/// slate and a full attempt budget; the loop drains due jobs right after this message, so the retry
/// starts immediately.
fn manual_retry(ctx: &mut Ctx, id: &str) {
    let row = match ctx.queue.get(id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            tray::set_status(&ctx.app, format!("⚠ recording {id} no longer exists"));
            return;
        }
        Err(e) => {
            eprintln!("[corti] retry lookup for {id} failed: {e:#}");
            return;
        }
    };
    if row.status != JobStatus::Failed {
        return; // already moving again (raced a resume/retry) — nothing to do
    }
    let has_checkpoint = checkpoint_path(&row.audio_path).is_file();
    if !row.audio_path.exists() && !has_checkpoint {
        tray::set_status(
            &ctx.app,
            "⚠ can't retry — the recovery files have already expired".to_string(),
        );
        return;
    }
    let retry_status = if has_checkpoint {
        JobStatus::PendingNote
    } else {
        JobStatus::PendingTranscription
    };
    let _ = ctx.queue.jobs().delete_failed(
        crate::jobs::RETRY_TRANSCRIPTION,
        &crate::jobs::id_payload(id),
    );
    if let Err(e) = ctx.queue.retry_reset_to(id, retry_status) {
        eprintln!("[corti] cannot reset {id} for retry: {e:#}");
        return;
    }
    if let Err(e) = ctx.queue.jobs().enqueue(
        crate::jobs::RETRY_TRANSCRIPTION,
        &crate::jobs::id_payload(id),
        crate::jobs::RETRY_MAX_ATTEMPTS,
        Utc::now(),
    ) {
        eprintln!("[corti] cannot schedule retry for {id}: {e:#}");
        return;
    }
    tray::update_history(&ctx.app, id, retry_status, None, None, None);
    let phase = if retry_status == JobStatus::PendingNote {
        "Retrying filing"
    } else {
        "Retrying"
    };
    tray::set_status(&ctx.app, format!("{phase} — {}…", row.owning_app));
}

/// A transcribe/file failure on the live path: don't terminal-fail — record the error and schedule the
/// durable retry job at the first backoff step. Filing failures preserve `PendingNote` so the checkpoint,
/// not ASR, is retried; every earlier failure returns to `PendingTranscription`.
fn schedule_retry(ctx: &Ctx, id: &str, meta: &RecordingMeta, err: anyhow::Error) {
    let msg = format!("{err:#}");
    eprintln!("[corti] job {id} failed (will retry): {msg}");
    let retry_status = ctx
        .queue
        .get(id)
        .ok()
        .flatten()
        .map(|row| retry_status(&row, &meta.audio_path))
        .unwrap_or_else(|| {
            if FilingCheckpoint::load(&meta.audio_path).is_ok() {
                JobStatus::PendingNote
            } else {
                JobStatus::PendingTranscription
            }
        });
    let update = ctx.queue.update(
        id,
        JobUpdate {
            status: Some(retry_status),
            error: Some(msg.clone()),
            ..Default::default()
        },
    );
    let enqueue = ctx.queue.jobs().enqueue(
        crate::jobs::RETRY_TRANSCRIPTION,
        &crate::jobs::id_payload(id),
        crate::jobs::RETRY_MAX_ATTEMPTS,
        Utc::now() + chrono::Duration::from_std(JOB_BACKOFF.base).unwrap_or_default(),
    );
    if let Err(e) = update.and(enqueue.map(|_| ())) {
        eprintln!("[corti] cannot schedule retry for {id}: {e:#}");
        fail(ctx, id, meta, msg);
        return;
    }
    tray::update_history(&ctx.app, id, retry_status, None, Some(msg.clone()), None);
    tray::set_status(
        &ctx.app,
        format!("⚠ {} — will retry: {msg}", meta.owning_app.name),
    );
    // The attempt is over; the recording now waits in backoff until the retry job fires. Nothing is
    // actively running, so the diagram returns to Idle.
    set_stage(&ctx.app, Stage::Idle);
}

/// #87 happy path: the live session already wrote the whole note and flipped its state line, so the
/// job goes straight to `Done` and batch transcription is skipped. Completion is one fallible SQL update;
/// raw audio remains under the configured retention policy and is also the fallback if completion fails.
fn live_filed(
    ctx: &mut Ctx,
    id: &str,
    meta: &RecordingMeta,
    audio: &Path,
    note: PathBuf,
) -> Result<()> {
    info!(
        target: "corti::pipeline",
        job_id = %id,
        note_path = %note.display(),
        audio_path = %audio.display(),
        "note filed live during the recording — skipping batch transcription"
    );
    // The note can first be created during the final live flush, after its asynchronous
    // `LiveNoteCreated` message. Persist the returned path synchronously before the fallible Done update so
    // a completion failure/crash still lets batch recovery rewrite this exact note instead of creating a
    // second one.
    ctx.queue
        .update(
            id,
            JobUpdate {
                note_path: Some(note.clone()),
                ..Default::default()
            },
        )
        .context("persisting live note path before completion")?;
    ctx.queue
        .complete_with_note(id, &note)
        .context("persisting live note completion")?;
    tray::update_history(&ctx.app, id, JobStatus::Done, None, None, Some(note));
    set_stage(&ctx.app, Stage::Idle);
    tray::set_status(&ctx.app, format!("✓ Filed — {}", meta.note_title()));
    Ok(())
}

/// #87: persist a just-created live note's path. Mid-recording there is no queue row yet, so one is
/// created (status `Recording` — truthful in the Queue window) **only while the session is still
/// active**: the first segment can close while a discarded call's backlog drains, in which case
/// `LiveDiscarded` already tore the session down and its Discard verdict will delete the note — a
/// row created now would be a zombie. If the row already exists (the note was created during the
/// finalize tail, after `Process` enqueued it — or the job even finished already) only `note_path`
/// is recorded and the row's current status is preserved, never regressed.
fn live_note_created(ctx: &Ctx, meta: &RecordingMeta, note_path: PathBuf) {
    let id = corti_queue::job_id(meta);
    let (result, history_status) = match ctx.queue.get(&id) {
        Ok(None) => {
            if !ctx.live.is_active(&id) {
                warn!(
                    target: "corti::live",
                    job_id = %id,
                    "ignoring a live note from an already-torn-down session"
                );
                return;
            }
            let created = ctx.queue.enqueue(meta).and_then(|id| {
                ctx.queue.update(
                    &id,
                    JobUpdate {
                        status: Some(JobStatus::Recording),
                        note_path: Some(note_path.clone()),
                        ..Default::default()
                    },
                )
            });
            (created, live_note_history_status(None))
        }
        Ok(Some(row)) => (
            ctx.queue.update(
                &id,
                JobUpdate {
                    note_path: Some(note_path.clone()),
                    ..Default::default()
                },
            ),
            live_note_history_status(Some(row.status)),
        ),
        Err(e) => (Err(e), live_note_history_status(None)),
    };
    match result {
        // The history entry gains the note path, so the tray's line is clickable and opens the
        // (possibly still growing) note.
        Ok(()) => tray::update_history(&ctx.app, &id, history_status, None, None, Some(note_path)),
        Err(e) => warn!(
            target: "corti::live",
            job_id = %id,
            error = %format!("{e:#}"),
            "could not persist the live note path"
        ),
    }
}

/// #87: the tray status to stamp when a live note path lands. A fresh row is `Recording`; an
/// existing row keeps whatever status the pipeline already drove it to — a short call's
/// `LiveNoteCreated` can be handled after `live_filed` marked the job `Done`, and must never
/// regress it back to `Recording`.
fn live_note_history_status(existing: Option<JobStatus>) -> JobStatus {
    existing.unwrap_or(JobStatus::Recording)
}

/// #87: a recording ended without a `Process` (discarded as too short, or its capture failed to
/// finish). Tear down the live session — non-joining; the session deletes its own partial note —
/// and terminally close a queue row `LiveNoteCreated` may have made, so nothing dangles at
/// `Recording`.
fn live_discarded(ctx: &Ctx, id: &str) {
    ctx.live.discard(id);
    match ctx.queue.get(id) {
        Ok(Some(row)) if !row.status.is_terminal() => {
            let _ = ctx.queue.update(
                id,
                JobUpdate {
                    status: Some(JobStatus::Failed),
                    error: Some("Discarded — too short".to_string()),
                    ..Default::default()
                },
            );
            tray::emit_queue_changed(&ctx.app, id);
        }
        _ => {}
    }
}

/// Apply a saved config change: re-read the shared runtime config and rebuild the backend + AEC toggle. A
/// job already transcribing finishes on the old backend (the worker is serial), so this is exactly "takes
/// effect on the next recording".
fn reload_config(ctx: &mut Ctx, config: &SharedConfig) {
    let cfg = config.lock().unwrap().clone();
    let backend_name = cfg.backend_name();
    ctx.aec_enabled = cfg.aec_enabled;
    ctx.aec_config = cfg.aec_config();
    ctx.backend_label = cfg.backend_label(); // read BEFORE Backend::init consumes cfg
    ctx.backend = Backend::init(cfg);
    info!(
        target: "corti::pipeline",
        backend = backend_name,
        aec = if ctx.aec_enabled { "on" } else { "off" },
        "settings saved — backend reloaded"
    );
}

/// At startup, schedule only rows that have crossed the durable post-ASR boundary. This intentionally
/// does not broaden into reconciliation of every first-attempt state: a valid checkpoint is the proof that
/// filing alone is safe, even if the row still says Transcribing because the adjacent write failed.
fn recover_filing_checkpoints(queue: &Queue) -> Vec<Job> {
    let rows = match queue.resumable() {
        Ok(rows) => rows,
        Err(err) => {
            eprintln!("[corti] cannot scan for durable filing checkpoints: {err:#}");
            return Vec::new();
        }
    };
    let mut recovered = Vec::new();
    for row in rows.into_iter().filter(|row| {
        matches!(
            row.status,
            JobStatus::PendingTranscription | JobStatus::Transcribing | JobStatus::PendingNote
        ) && FilingCheckpoint::load(&row.audio_path).is_ok()
    }) {
        // Enqueue first. If the following status repair fails, the handler still recognizes the checkpoint
        // from any nonterminal transcription state and promotes it itself.
        let enqueued = queue.jobs().enqueue(
            crate::jobs::RETRY_TRANSCRIPTION,
            &crate::jobs::id_payload(&row.id),
            crate::jobs::RETRY_MAX_ATTEMPTS,
            Utc::now(),
        );
        let promoted = enqueued.and_then(|_| {
            queue.update(
                &row.id,
                JobUpdate {
                    status: Some(JobStatus::PendingNote),
                    ..Default::default()
                },
            )
        });
        match promoted {
            Ok(()) => recovered.push(row),
            Err(err) => eprintln!(
                "[corti] cannot resume filing checkpoint for {}: {err:#}",
                row.id
            ),
        }
    }
    recovered
}

/// Outcome of reaping one row stranded at `Recording`.
enum Reaped {
    /// Audio still on disk — reset to `PendingTranscription` with a due-now durable retry (which
    /// reaches `file_and_done`'s rewrite-in-place branch via the persisted `note_path`).
    Retrying,
    /// Audio gone — terminally failed; the note (if any) was flipped + annotated.
    Failed(String),
}

/// #87: startup reaper for rows stranded at `Recording` by a quit/crash mid-call (they only exist
/// because a live session persisted its note path mid-recording). Nothing else ever touches them —
/// `retry_transcription` skips `Recording` and the retention sweep only matches terminal rows — so
/// without this the row, its `State: transcribing` note, and its audio would sit forever. Queue-only
/// (no tray) so it is testable; the caller surfaces each outcome in the tray.
fn reap_recording_rows(queue: &Queue) -> Vec<(Job, Reaped)> {
    let rows = match queue.all() {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("[corti] cannot scan for stranded recordings: {e:#}");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for row in rows
        .into_iter()
        .filter(|r| r.status == JobStatus::Recording)
    {
        let outcome = if row.audio_path.exists() {
            let reset = queue.update(
                &row.id,
                JobUpdate {
                    status: Some(JobStatus::PendingTranscription),
                    ..Default::default()
                },
            );
            let retry = queue.jobs().enqueue(
                crate::jobs::RETRY_TRANSCRIPTION,
                &crate::jobs::id_payload(&row.id),
                crate::jobs::RETRY_MAX_ATTEMPTS,
                Utc::now(),
            );
            match reset.and(retry.map(|_| ())) {
                Ok(()) => Reaped::Retrying,
                Err(e) => {
                    eprintln!("[corti] cannot revive stranded recording {}: {e:#}", row.id);
                    continue;
                }
            }
        } else {
            let msg = "app quit during the recording — audio incomplete".to_string();
            if let Err(e) = queue.fail(&row.id, &msg) {
                eprintln!("[corti] cannot fail stranded recording {}: {e:#}", row.id);
                continue;
            }
            if let Some(note) = &row.note_path {
                close_out_note(note, &msg);
            }
            Reaped::Failed(msg)
        };
        out.push((row, outcome));
    }
    out
}

/// #87: a recording is terminally failing while its note may still read `State: transcribing` —
/// flip the state line (inbox agents must not wait on it forever) and append why the transcript is
/// incomplete. Best-effort; the flip is idempotent.
fn close_out_note(note: &Path, reason: &str) {
    if !note.exists() {
        return;
    }
    if let Err(e) = corti_vagus::note::flip_state(note) {
        warn!(
            target: "corti::live",
            note_path = %note.display(),
            error = %format!("{e:#}"),
            "could not flip the state line of a failed recording's note"
        );
        return;
    }
    let line = format!("\n> corti: transcription incomplete — {reason}\n");
    if let Err(e) = corti_vagus::note::append(note, &line) {
        warn!(
            target: "corti::live",
            note_path = %note.display(),
            error = %format!("{e:#}"),
            "could not annotate a failed recording's note"
        );
    }
}

/// Seed the tray's `History ▸` submenu with the most recent recordings from the durable queue, so the
/// history survives a restart (issue #3). Best-effort: a read failure just leaves the history empty until
/// the next live recording. Runs on the worker thread (the sole `Queue` owner), never the UI loop.
fn seed_history(ctx: &Ctx) {
    let jobs = match ctx.queue.all() {
        Ok(j) => j,
        Err(e) => {
            warn!(target: "corti::pipeline", error = %format!("{e:#}"), "could not read recordings for tray history");
            return;
        }
    };
    // `all()` is oldest-first; take the newest HISTORY_LIMIT and push oldest→newest so the front ends up
    // being the most recent (push_history prepends).
    for job in jobs.iter().rev().take(HISTORY_LIMIT).rev() {
        tray::push_history(&ctx.app, history_entry_from_job(job));
    }
}

/// Build a tray [`HistoryEntry`] from a durable [`Job`] row (for startup seeding / resume). The capture
/// mode is re-derived from the persisted owning-app signals via `meta()` — no new column (issue #28).
fn history_entry_from_job(job: &Job) -> HistoryEntry {
    HistoryEntry {
        id: job.id.clone(),
        label: job.owning_app.clone(),
        started_at: job.started_at,
        ended_at: job.ended_at,
        status: job.status,
        mode: job.meta().mode(),
        error: job.error.clone(),
        note_path: job.note_path.clone(),
    }
}

/// Transcribe `audio`, durably checkpoint the backend result, then file it. Used for fresh recordings
/// and transcription retry-job runs. Once `PendingNote` is committed, later retries use
/// [`file_checkpoint`] and never invoke the backend again.
pub(crate) fn transcribe_and_file(
    ctx: &mut Ctx,
    id: &str,
    meta: &RecordingMeta,
    audio: &Path,
) -> Result<()> {
    // A checkpoint can be newer than the row when its atomic rename succeeded but the adjacent SQLite
    // transition failed (or the process died between them). Treat it as authoritative from every dispatch
    // path, including a duplicate first-attempt Process message.
    if FilingCheckpoint::load(audio).is_ok() {
        ctx.queue
            .update(
                id,
                JobUpdate {
                    status: Some(JobStatus::PendingNote),
                    ..Default::default()
                },
            )
            .context("promoting durable checkpoint to PendingNote")?;
        return file_checkpoint(ctx, id, meta, audio);
    }

    // Reuse the persisted stable Transcribe name across retries. Legacy PendingNote recovery writes a new
    // compatibility name first; ordinary recordings default to their id.
    let transcribe_job = ctx
        .queue
        .get(id)
        .context("reading transcription attempt")?
        .and_then(|row| row.transcribe_job)
        .unwrap_or_else(|| id.to_string());
    ctx.queue
        .update(
            id,
            JobUpdate {
                status: Some(JobStatus::Transcribing),
                transcribe_job: Some(transcribe_job.clone()),
                ..Default::default()
            },
        )
        .context("queue update before transcribe")?;
    set_stage(&ctx.app, Stage::Transcribing);
    tray::set_status(
        &ctx.app,
        format!("Transcribing — {}…", meta.owning_app.name),
    );
    tray::update_history(&ctx.app, id, JobStatus::Transcribing, None, None, None);

    // Run AEC + backend from the retained raw recording. Pipeline AWS attempts use the row's stable name;
    // one-shot CLI calls deliberately pass no stable name.
    let t0 = std::time::Instant::now();
    let transcribed = crate::transcribe::transcribe_recording(
        &ctx.backend,
        ctx.aec_enabled,
        false,
        &ctx.aec_config,
        crate::transcribe::TranscriptionAttempt::durable_named(id, &transcribe_job),
        meta,
        audio,
    );
    ctx.stats
        .record_stage("transcribe", t0.elapsed(), ctx.backend_label);
    let transcribe_secs = t0.elapsed().as_secs_f64();
    let (transcript, _input) = transcribed.context("transcription failed")?;

    // A partial live note may already exist. Put it in the checkpoint too, so filing idempotency does not
    // depend on a second successful queue read after this boundary.
    let note_path = ctx
        .queue
        .get(id)
        .context("reading recording before checkpoint")?
        .and_then(|row| row.note_path);
    FilingCheckpoint::new(transcript, note_path)
        .store(audio)
        .context("persisting transcript checkpoint")?;

    ctx.queue
        .update(
            id,
            JobUpdate {
                status: Some(JobStatus::PendingNote),
                transcribe_secs: Some(transcribe_secs),
                ..Default::default()
            },
        )
        .context("queue set PendingNote")?;

    file_checkpoint(ctx, id, meta, audio)
}

/// Load a durable transcript checkpoint and run only the filing stage. On success the raw recording stays
/// under retention, while the reproducible clean WAV and checkpoint are removed.
pub(crate) fn file_checkpoint(
    ctx: &mut Ctx,
    id: &str,
    meta: &RecordingMeta,
    audio: &Path,
) -> Result<()> {
    let mut checkpoint = FilingCheckpoint::load(audio).context("loading transcript checkpoint")?;
    set_stage(&ctx.app, Stage::Filing);
    tray::update_history(&ctx.app, id, JobStatus::PendingNote, None, None, None);
    tray::set_status(&ctx.app, format!("Filing note — {}…", meta.owning_app.name));

    // Cloud objects stopped being recovery inputs once the checkpoint was durable, but their privacy
    // cleanup is not best-effort: keep PendingNote/checkpoint and retry this idempotent deletion before
    // allowing terminal completion.
    let aws_job_name = ctx
        .queue
        .get(id)
        .context("reading transcription attempt before cloud cleanup")?
        .and_then(|row| row.transcribe_job)
        .unwrap_or_else(|| id.to_string());
    ctx.backend
        .cleanup_after_checkpoint(&aws_job_name)
        .context("cleaning staged AWS objects after checkpoint")?;

    let tf = std::time::Instant::now();
    let result = file_and_done(ctx, id, meta, audio, &mut checkpoint);
    ctx.stats.record_stage("file", tf.elapsed(), "vagus");
    if result.is_ok() {
        cleanup_completed_artifacts(audio);
    }
    result
}

/// File the checkpointed transcript and atomically commit `note_path + Done`. The checkpoint is updated
/// with a newly returned note path before the SQL completion attempt, so a queue failure retries the same
/// note rather than invoking `vagus add-note` again.
fn file_and_done(
    ctx: &mut Ctx,
    id: &str,
    meta: &RecordingMeta,
    audio: &Path,
    checkpoint: &mut FilingCheckpoint,
) -> Result<()> {
    let queued_note = ctx
        .queue
        .get(id)
        .context("reading recording before filing")?
        .and_then(|row| row.note_path);
    let existing = checkpoint
        .note_path
        .clone()
        .filter(|path| path.exists())
        .or_else(|| queued_note.filter(|path| path.exists()));

    let note = if let Some(existing) = existing {
        corti_vagus::note::rewrite_body(
            &existing,
            &corti_vagus::recording_body(meta, &checkpoint.transcript),
        )
        .context("rewriting the existing note")?;
        info!(
            target: "corti::pipeline",
            job_id = %id,
            note_path = %existing.display(),
            "batch transcript rewrote the existing note in place"
        );
        existing
    } else {
        // Startup discovery may have failed only because vagus was not installed yet. Re-probe on every
        // filing attempt so installing it during backoff does not require relaunching Corti.
        if ctx.vagus.is_err() {
            ctx.vagus = Vagus::discover().map_err(|e| format!("{e:#}"));
            if let Ok(v) = &ctx.vagus {
                info!(
                    target: "corti::pipeline",
                    bin = %v.bin().display(),
                    "vagus now available — filing enabled"
                );
            }
        }
        let vagus = match &ctx.vagus {
            Ok(v) => v,
            Err(e) => anyhow::bail!("vagus unavailable: {e}"),
        };
        let note = vagus
            .file_recording(meta, &checkpoint.transcript)
            .context("filing note failed")?;
        info!(
            target: "corti::pipeline",
            job_id = %id,
            note_path = %note.display(),
            "note filed"
        );
        note
    };

    checkpoint.note_path = Some(note.clone());
    let checkpoint_write = checkpoint
        .store(audio)
        .context("persisting returned note path in transcript checkpoint");
    let completion = ctx
        .queue
        .complete_with_note(id, &note)
        .context("persisting note path and Done status");

    match (checkpoint_write, completion) {
        (Ok(()), Ok(())) => {}
        // The database is the durable authority once this succeeds; a checkpoint rewrite failure is no
        // longer dangerous and the successful cleanup below removes the stale checkpoint.
        (Err(e), Ok(())) => warn!(
            target: "corti::pipeline",
            job_id = %id,
            error = %format!("{e:#}"),
            "recording completed but returned note path was not refreshed in checkpoint"
        ),
        (Ok(()), Err(e)) => return Err(e),
        (Err(checkpoint_err), Err(queue_err)) => {
            return Err(anyhow::anyhow!(
                "checkpointing returned note path failed: {checkpoint_err:#}; queue completion failed: {queue_err:#}"
            ));
        }
    }

    let title = meta.note_title();
    tray::update_history(&ctx.app, id, JobStatus::Done, None, None, Some(note));
    set_stage(&ctx.app, Stage::Idle);
    tray::set_status(&ctx.app, format!("✓ Filed — {title}"));
    Ok(())
}

/// Completion cleanup has one authority: keep the raw recording for the configured sweep, but remove
/// reproducible/transient derivatives. Failures are logged and the sweep retries all of these paths later.
fn cleanup_completed_artifacts(raw: &Path) {
    for path in [corti_capture::clean_wav_path(raw), checkpoint_path(raw)] {
        if !path.exists() {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => info!(
                target: "corti::pipeline",
                path = %path.display(),
                "removed completed pipeline derivative"
            ),
            Err(e) => warn!(
                target: "corti::pipeline",
                path = %path.display(),
                error = %e,
                "could not remove completed pipeline derivative"
            ),
        }
    }
}

/// Terminally fail a recording and surface it in the tray (both the status line and its history
/// entry). Reserved for unrecoverable states — transient errors go through [`schedule_retry`] /
/// the retry job's backoff instead.
pub(crate) fn fail(ctx: &Ctx, id: &str, meta: &RecordingMeta, msg: String) {
    error!(target: "corti::pipeline", job_id = %id, error = %msg, "job failed");
    // #87: a partial live note for this recording may still read `State: transcribing`; the batch
    // pass that would have rewritten it is never coming, so close it out.
    if let Ok(Some(row)) = ctx.queue.get(id)
        && let Some(note) = row.note_path
    {
        close_out_note(&note, &msg);
    }
    let _ = ctx.queue.fail(id, &msg);
    set_stage(&ctx.app, Stage::Idle);
    tray::update_history(
        &ctx.app,
        id,
        JobStatus::Failed,
        None,
        Some(msg.clone()),
        None,
    );
    tray::set_status(&ctx.app, format!("⚠ {} — {msg}", meta.owning_app.name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{DiarizedTranscript, OwningApp};

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("corti-pipeline-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn meta(audio: PathBuf) -> RecordingMeta {
        RecordingMeta {
            started_at: chrono::Local::now(),
            ended_at: None,
            owning_app: OwningApp::from_bundle_id("us.zoom.xos"),
            audio_path: audio,
        }
    }

    #[test]
    fn startup_recovers_only_valid_post_asr_checkpoints() {
        let dir = test_dir("checkpoint-startup-recovery");
        let queue = Queue::open_at(dir.join("queue.db")).unwrap();

        let add = |name: &str, status: JobStatus, checkpoint: bool| {
            let audio = dir.join(format!("{name}.wav"));
            std::fs::write(&audio, b"raw").unwrap();
            let id = queue.enqueue(&meta(audio.clone())).unwrap();
            queue.set_status(&id, status).unwrap();
            if checkpoint {
                FilingCheckpoint::new(DiarizedTranscript::new(Vec::new()), None)
                    .store(&audio)
                    .unwrap();
            }
            id
        };
        let transcribing = add("transcribing", JobStatus::Transcribing, true);
        let pending_note = add("pending-note", JobStatus::PendingNote, true);
        let no_checkpoint = add("no-checkpoint", JobStatus::Transcribing, false);
        let done = add("done", JobStatus::Done, true);

        let mut recovered = recover_filing_checkpoints(&queue)
            .into_iter()
            .map(|row| row.id)
            .collect::<Vec<_>>();
        recovered.sort();
        let mut expected = vec![transcribing.clone(), pending_note.clone()];
        expected.sort();
        assert_eq!(recovered, expected);
        for id in [&transcribing, &pending_note] {
            assert_eq!(
                queue.get(id).unwrap().unwrap().status,
                JobStatus::PendingNote
            );
        }
        assert_eq!(
            queue.get(&no_checkpoint).unwrap().unwrap().status,
            JobStatus::Transcribing
        );
        assert_eq!(queue.get(&done).unwrap().unwrap().status, JobStatus::Done);
        assert_eq!(
            queue
                .jobs()
                .active_for(crate::jobs::RETRY_TRANSCRIPTION)
                .unwrap()
                .len(),
            2
        );

        // Active-job deduplication makes the startup scan idempotent.
        assert_eq!(recover_filing_checkpoints(&queue).len(), 2);
        assert_eq!(
            queue
                .jobs()
                .active_for(crate::jobs::RETRY_TRANSCRIPTION)
                .unwrap()
                .len(),
            2
        );
        drop(queue);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn completion_cleanup_retains_raw_and_removes_only_derivatives() {
        let dir = test_dir("completion-cleanup");
        let raw = dir.join("recording.wav");
        let clean = corti_capture::clean_wav_path(&raw);
        let checkpoint = checkpoint_path(&raw);
        std::fs::write(&raw, b"raw").unwrap();
        std::fs::write(&clean, b"clean").unwrap();
        std::fs::write(&checkpoint, b"checkpoint").unwrap();

        cleanup_completed_artifacts(&raw);

        assert!(raw.exists(), "raw audio belongs to the retention sweep");
        assert!(!clean.exists(), "clean WAV is reproducible");
        assert!(
            !checkpoint.exists(),
            "Done no longer needs a filing checkpoint"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    /// #87 startup reaper: a `Recording` row with audio on disk is revived (PendingTranscription +
    /// due-now retry); one whose audio is gone is terminally failed and its live note flipped +
    /// annotated; terminal rows are untouched.
    #[test]
    fn reaper_revives_or_fails_stranded_recording_rows() {
        let dir = test_dir("reaper");
        let queue = Queue::open_at(dir.join("queue.db")).unwrap();

        // A: stranded mid-call, audio still on disk.
        let audio_a = dir.join("a.wav");
        std::fs::write(&audio_a, b"x").unwrap();
        let a = queue.enqueue(&meta(audio_a)).unwrap();
        queue.set_status(&a, JobStatus::Recording).unwrap();

        // B: stranded mid-call, audio gone, live note still saying `State: transcribing`.
        let note_b = dir.join("note-b.md");
        std::fs::write(
            &note_b,
            format!(
                "---\nsource: x\n---\n\n# T\n\n{}\n\n> ctx\n\n## Transcript\n\n",
                corti_vagus::note::STATE_TRANSCRIBING
            ),
        )
        .unwrap();
        let b = queue.enqueue(&meta(dir.join("missing.wav"))).unwrap();
        queue
            .update(
                &b,
                JobUpdate {
                    status: Some(JobStatus::Recording),
                    note_path: Some(note_b.clone()),
                    ..Default::default()
                },
            )
            .unwrap();

        // C: already terminal — never touched.
        let c = queue.enqueue(&meta(dir.join("c.wav"))).unwrap();
        queue.set_status(&c, JobStatus::Done).unwrap();

        let outcomes = reap_recording_rows(&queue);
        assert_eq!(outcomes.len(), 2, "only the two Recording rows are reaped");

        let for_id = |id: &str| {
            outcomes
                .iter()
                .find(|(row, _)| row.id == id)
                .map(|(_, o)| o)
                .unwrap()
        };
        assert!(matches!(for_id(&a), Reaped::Retrying));
        assert!(matches!(for_id(&b), Reaped::Failed(_)));

        // A: reset + a due-now durable retry queued.
        assert_eq!(
            queue.get(&a).unwrap().unwrap().status,
            JobStatus::PendingTranscription
        );
        assert!(
            queue.jobs().next_due_at().unwrap().is_some(),
            "a retry job must be queued"
        );

        // B: terminally failed; note flipped in place + annotated.
        let row_b = queue.get(&b).unwrap().unwrap();
        assert_eq!(row_b.status, JobStatus::Failed);
        let note = std::fs::read_to_string(&note_b).unwrap();
        assert!(note.contains("State: transcribed \n"), "got: {note}");
        assert!(!note.contains(corti_vagus::note::STATE_TRANSCRIBING));
        assert!(note.contains("transcription incomplete"), "got: {note}");

        // C untouched.
        assert_eq!(queue.get(&c).unwrap().unwrap().status, JobStatus::Done);

        // Idempotent: nothing left at Recording.
        assert!(reap_recording_rows(&queue).is_empty());
    }

    /// #87: a late `LiveNoteCreated` must never regress an existing row's tray status (a short
    /// call's message can be handled after the job is already `Done`).
    #[test]
    fn live_note_history_status_never_regresses() {
        assert_eq!(live_note_history_status(None), JobStatus::Recording);
        assert_eq!(
            live_note_history_status(Some(JobStatus::Done)),
            JobStatus::Done
        );
        assert_eq!(
            live_note_history_status(Some(JobStatus::Transcribing)),
            JobStatus::Transcribing
        );
        assert_eq!(
            live_note_history_status(Some(JobStatus::Failed)),
            JobStatus::Failed
        );
    }
}
