//! The pipeline worker: the single thread that owns the [`Queue`] and pushes every recording through
//! `enqueue → transcribe → file → Done`.
//!
//! Why one thread: rusqlite's `Connection` is `Send` but not `Sync`, so the `Queue` has exactly one owner
//! here and is never shared. Transcription blocks (the backend runs its own runtime), so doing it on this
//! dedicated thread keeps it off the Tauri UI loop (guardrail 9). Jobs run serially — fine, since
//! transcription is inherently sequential and a second call simply waits in the channel.
//!
//! ## Durability deferred (ADR 0007), with in-session retry
//! Crash recovery and retention are intentionally not implemented: the pipeline runs in-process and a crash
//! or quit mid-call loses *that* call. The `Queue` is kept as a session-spanning record so the tray history
//! (issue #3) and `corti --list` still work across restarts. What *is* implemented is transient-failure
//! retry: a transcription/filing failure on the live path doesn't terminally fail — it records the error,
//! keeps the row at `PendingTranscription` ("will retry"), and schedules a durable `retry_transcription`
//! background job with backoff (see `crate::jobs`). The captured audio is the only input, so retry is only
//! possible while it's still on disk (a transcription failure keeps it; once transcribed it is pruned).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use corti_core::{DiarizedTranscript, JobStatus, RecordingMeta};
use corti_jobs::{Backoff, ClaimedJob, FailOutcome};
use corti_queue::{Job, JobUpdate, Queue};
use corti_vagus::Vagus;
use tauri::AppHandle;
use tracing::{error, info, warn};

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

/// What the pipeline does with a collected live result. Only a zero-drop [`crate::live::LiveOutcome::Filed`] is
/// canonical; every other result takes the lossless batch path, optionally retaining a partial note to
/// rewrite in place.
enum LiveResolution {
    Filed(PathBuf),
    Batch { fallback: Option<LiveFallback> },
}

struct LiveFallback {
    reason: String,
    note_path: Option<PathBuf>,
}

fn resolve_live(outcome: Option<crate::live::LiveOutcome>) -> LiveResolution {
    match outcome {
        Some(crate::live::LiveOutcome::Filed { note_path }) => LiveResolution::Filed(note_path),
        Some(crate::live::LiveOutcome::Fallback { reason, note_path }) => LiveResolution::Batch {
            fallback: Some(LiveFallback { reason, note_path }),
        },
        Some(crate::live::LiveOutcome::NoNote) | None => LiveResolution::Batch { fallback: None },
    }
}

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
    /// finish). The detector has already delivered the recording-specific discard verdict; close any
    /// queue row `LiveNoteCreated` made (the manager call remains idempotent defense-in-depth).
    LiveDiscarded { id: String },
    /// A discarded-session unlink failed both in its note writer and detached reaper. Retry once on the
    /// pipeline and, if it still fails, durably retain/close the path instead of forgetting it.
    LiveDiscardCleanup {
        meta: RecordingMeta,
        note_path: PathBuf,
        error: String,
    },
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
    /// #87: recording-scoped live sessions. The detector delivers terminal verdicts; the worker collects
    /// finished IDs and owns any last-resort discard cleanup.
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
                    // #87: the detector already delivered this recording's finish verdict before
                    // queuing `Process`. Collect exactly that ID: a canonical, zero-drop live note skips
                    // batch; every other outcome falls back to the lossless WAV. Persist a partial live
                    // note first so `file_and_done` rewrites the same file instead of double-filing.
                    match resolve_live(ctx.live.collect(&id)) {
                        LiveResolution::Filed(note_path) => {
                            live_filed(&mut ctx, &id, &meta, &audio_path, note_path);
                        }
                        LiveResolution::Batch { fallback } => {
                            let preferred_note = if let Some(LiveFallback { reason, note_path }) =
                                fallback
                            {
                                warn!(
                                    target: "corti::live",
                                    job_id = %id,
                                    reason = %reason,
                                    "live result is not canonical — falling back to batch transcription"
                                );
                                note_path
                            } else {
                                None
                            };
                            if let Some(partial) = preferred_note.as_ref()
                                && let Err(e) = ctx.queue.update(
                                    &id,
                                    JobUpdate {
                                        note_path: Some(partial.clone()),
                                        ..Default::default()
                                    },
                                )
                            {
                                // Do not start batch with an untracked live note. The durable retry payload
                                // carries the path even when this queue write is exactly what failed.
                                schedule_retry(
                                    &ctx,
                                    &id,
                                    &meta,
                                    anyhow::anyhow!("persisting partial live note path: {e:#}"),
                                    Some(partial),
                                );
                                continue;
                            }
                            if let Err(e) = transcribe_and_file(
                                &mut ctx,
                                &id,
                                &meta,
                                &audio_path,
                                preferred_note.as_deref(),
                            ) {
                                schedule_retry(&ctx, &id, &meta, e, preferred_note.as_deref());
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
                    // #87: still collect this recording's already-delivered finish result so its model
                    // session is released — with no queue row the outcome can only be logged.
                    match ctx.live.collect(&corti_queue::job_id(&meta)) {
                        Some(crate::live::LiveOutcome::Filed { note_path }) => warn!(
                            target: "corti::live",
                            note_path = %note_path.display(),
                            "live note filed but its recording row could not be created"
                        ),
                        Some(crate::live::LiveOutcome::Fallback { reason, note_path }) => {
                            warn!(
                                target: "corti::live",
                                reason = %reason,
                                note_path = ?note_path,
                                "live session required fallback alongside the enqueue failure"
                            );
                            if let Some(note_path) = note_path {
                                close_out_note(
                                    &note_path,
                                    "queue enqueue failed before canonical batch fallback",
                                );
                            }
                        }
                        Some(crate::live::LiveOutcome::NoNote) | None => {}
                    }
                    // No durable row exists for terminal failure handling — reset here so
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
            Ok(PipelineMsg::LiveDiscardCleanup {
                meta,
                note_path,
                error,
            }) => live_discard_cleanup(&ctx, &meta, &note_path, &error),
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

/// The Queue window's Retry button. Only a `Failed` recording with its audio still on disk qualifies
/// (the UI greys the button out otherwise, but it can race the pipeline/sweep — re-check here, on the
/// thread that owns the truth). Old failed-job debris is cleared so the fresh enqueue gets a clean
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
    if !row.audio_path.exists() {
        tray::set_status(
            &ctx.app,
            "⚠ can't retry — the audio has already expired".to_string(),
        );
        return;
    }
    let _ = ctx.queue.jobs().delete_failed(
        crate::jobs::RETRY_TRANSCRIPTION,
        &crate::jobs::id_payload(id),
    );
    if let Err(e) = ctx.queue.retry_reset(id) {
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
    tray::update_history(
        &ctx.app,
        id,
        JobStatus::PendingTranscription,
        None,
        None,
        None,
    );
    tray::set_status(&ctx.app, format!("Retrying — {}…", row.owning_app));
}

/// A transcribe/file failure on the live path: don't terminal-fail — record the error, keep the row at
/// `PendingTranscription` ("will retry" in the UI), and schedule the durable retry job at the first
/// backoff step. If even that can't be persisted, fall back to a terminal failure so the recording is
/// never silently stuck.
fn schedule_retry(
    ctx: &Ctx,
    id: &str,
    meta: &RecordingMeta,
    err: anyhow::Error,
    preferred_note: Option<&Path>,
) {
    let msg = format!("{err:#}");
    eprintln!("[corti] job {id} failed (will retry): {msg}");
    let payload = crate::jobs::retry_payload(id, preferred_note);
    let enqueue = ctx.queue.jobs().enqueue(
        crate::jobs::RETRY_TRANSCRIPTION,
        &payload,
        crate::jobs::RETRY_MAX_ATTEMPTS,
        Utc::now() + chrono::Duration::from_std(JOB_BACKOFF.base).unwrap_or_default(),
    );
    if let Err(e) = enqueue {
        eprintln!("[corti] cannot schedule retry for {id}: {e:#}");
        fail_with_note(ctx, id, meta, msg, preferred_note);
        return;
    }
    // The job payload is already durable and carries the preferred note path. A simultaneous recording-row
    // write failure therefore cannot erase the no-double-note authority; the retry can repair it later.
    if let Err(e) = ctx.queue.update(
        id,
        JobUpdate {
            status: Some(JobStatus::PendingTranscription),
            note_path: preferred_note.map(Path::to_path_buf),
            error: Some(msg.clone()),
            ..Default::default()
        },
    ) {
        warn!(
            target: "corti::pipeline",
            job_id = %id,
            error = %format!("{e:#}"),
            "retry job persisted but recording backoff state could not be updated"
        );
    }
    tray::update_history(
        &ctx.app,
        id,
        JobStatus::PendingTranscription,
        None,
        Some(msg.clone()),
        None,
    );
    tray::set_status(
        &ctx.app,
        format!("⚠ {} — will retry: {msg}", meta.owning_app.name),
    );
    // The attempt is over; the recording now waits in backoff until the retry job fires. Nothing is
    // actively running, so the diagram returns to Idle.
    set_stage(&ctx.app, Stage::Idle);
}

/// #87 happy path: the live session already wrote the whole note and flipped its state line, so the
/// job goes straight to `Done` — batch transcription is skipped. The WAV was still written by the
/// capture writer (the tee is a tee), so the transient prune runs exactly as on the batch path.
fn live_filed(ctx: &mut Ctx, id: &str, meta: &RecordingMeta, audio: &Path, note: PathBuf) {
    info!(
        target: "corti::pipeline",
        job_id = %id,
        note_path = %note.display(),
        "note filed live during the recording — skipping batch transcription"
    );
    prune_transient(audio, &corti_capture::clean_wav_path(audio));
    // Same crash-safe ordering as file_and_done: persist note_path first, then mark Done.
    if let Err(e) = ctx.queue.update(
        id,
        JobUpdate {
            note_path: Some(note.clone()),
            ..Default::default()
        },
    ) {
        warn!(
            target: "corti::pipeline",
            job_id = %id,
            error = %format!("{e:#}"),
            "live note filed but could not persist note_path"
        );
    }
    let _ = ctx.queue.set_status(id, JobStatus::Done);
    tray::update_history(&ctx.app, id, JobStatus::Done, None, None, Some(note));
    set_stage(&ctx.app, Stage::Idle);
    tray::set_status(&ctx.app, format!("✓ Filed — {}", meta.note_title()));
}

/// #87: persist a just-created live note's path. Mid-recording there is no queue row yet, so one is
/// created (status `Recording` — truthful in the Queue window) **only while the session is still
/// owned**: this includes a finish-delivered session awaiting ID-specific collection, because its tail
/// can create the first note while `Process` waits behind another job. A discarded session is never
/// accepted — its reaper deletes a late partial note, and creating a row would make a zombie. If the row
/// already exists (the note was created during the finish tail after `Process` enqueued it) only `note_path`
/// is recorded and the row's current status is preserved, never regressed.
fn live_note_created(ctx: &Ctx, meta: &RecordingMeta, note_path: PathBuf) {
    let id = corti_queue::job_id(meta);
    let (result, history_status) = match ctx.queue.get(&id) {
        Ok(None) => {
            if !ctx.live.accepts_note(&id) {
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
/// finish). The detector already delivered the non-joining discard verdict before this later pipeline
/// message; repeat it idempotently, then terminally close a queue row `LiveNoteCreated` may have made so
/// nothing dangles at `Recording`.
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

/// Last-resort ownership for a discarded note that could not be unlinked on the live or reaper thread.
/// Retry once here; a persistent failure becomes a path-bearing Failed row and a closed/annotated note,
/// never an unreferenced `State: transcribing` artifact.
fn live_discard_cleanup(ctx: &Ctx, meta: &RecordingMeta, note_path: &Path, previous_error: &str) {
    let id = corti_queue::job_id(meta);
    match std::fs::remove_file(note_path) {
        Ok(()) => info!(
            target: "corti::live",
            job_id = %id,
            note_path = %note_path.display(),
            "pipeline retry deleted discarded live note"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            let reason =
                format!("discarded live note could not be deleted: {e} (reaper: {previous_error})");
            let persisted = ctx.queue.enqueue(meta).and_then(|id| {
                let row = ctx
                    .queue
                    .get(&id)?
                    .context("discard cleanup row disappeared")?;
                if row.status == JobStatus::Done {
                    return Ok(());
                }
                ctx.queue.update(
                    &id,
                    JobUpdate {
                        status: Some(JobStatus::Failed),
                        note_path: Some(note_path.to_path_buf()),
                        error: Some(reason.clone()),
                        ..Default::default()
                    },
                )
            });
            if let Err(persist_error) = persisted {
                warn!(
                    target: "corti::live",
                    job_id = %id,
                    note_path = %note_path.display(),
                    error = %format!("{persist_error:#}"),
                    "could not persist failed discarded-note cleanup"
                );
            }
            close_out_note(note_path, &reason);
            tray::emit_queue_changed(&ctx.app, &id);
        }
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

/// Transcribe `audio`, then file it. Used for both fresh recordings and retry-job re-runs. Errors are
/// returned to the caller — the live path schedules the durable retry, the retry job applies its backoff
/// — never terminally failed here.
pub(crate) fn transcribe_and_file(
    ctx: &mut Ctx,
    id: &str,
    meta: &RecordingMeta,
    audio: &Path,
    preferred_note: Option<&Path>,
) -> Result<()> {
    // Record the stable Transcribe job name + advance to Transcribing before transcribing.
    ctx.queue
        .update(
            id,
            JobUpdate {
                status: Some(JobStatus::Transcribing),
                transcribe_job: Some(id.to_string()),
                ..Default::default()
            },
        )
        .context("queue update before transcribe")?;
    // An attempt is now actively running — both the live path and a retry job land here.
    set_stage(&ctx.app, Stage::Transcribing);
    tray::set_status(
        &ctx.app,
        format!("Transcribing — {}…", meta.owning_app.name),
    );
    tray::update_history(&ctx.app, id, JobStatus::Transcribing, None, None, None);

    // Run the shared transcription core: AEC (speaker-bleed removal) → backend. The pipeline's input is the
    // lossless 2-track capture, so `skip_aec` is false; a tap-only recording is handled inside (`Ok(None)`).
    // Only a real transcription failure returns `Err` here. On failure we keep the capture audio so the
    // durable retry job can re-run it (the lossless 2-track on disk is the only copy).
    let t0 = std::time::Instant::now();
    let transcribed = crate::transcribe::transcribe_recording(
        &ctx.backend,
        ctx.aec_enabled,
        false,
        &ctx.aec_config,
        id,
        meta,
        audio,
    );
    // Record the wall-clock of the transcribe (AEC + backend) stage on both success and failure. For the
    // AWS backend this is poll-sleep I/O-wait; for local it is CPU — the backend label distinguishes them.
    ctx.stats
        .record_stage("transcribe", t0.elapsed(), ctx.backend_label);
    let transcribe_secs = t0.elapsed().as_secs_f64();
    let (transcript, input) = transcribed.context("transcription failed")?;
    // ADR 0007 (no retention): the capture audio is a **transient** intermediate, kept only long enough to
    // transcribe. Delete it — and the cleaned sibling AEC produced, when distinct — now that the transcript
    // is in hand. (Only reached on success; a transcription failure above returns early and keeps the audio
    // so the retry job can re-run it.)
    prune_transient(audio, &input);

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
    set_stage(&ctx.app, Stage::Filing);
    tray::update_history(&ctx.app, id, JobStatus::PendingNote, None, None, None);
    // File stage (vagus filing). No backend dimension; label "vagus".
    let tf = std::time::Instant::now();
    let result = file_and_done(ctx, id, meta, &transcript, preferred_note);
    ctx.stats.record_stage("file", tf.elapsed(), "vagus");
    result
}

/// Rewrite a queue-authoritative live note with the canonical batch body. `rewrite_body` truncates/writes
/// the existing file rather than renaming over it, preserving both its path and inode. A missing/deleted
/// note returns `None` so the caller can file a fresh one.
fn rewrite_existing_note(
    queue: &Queue,
    id: &str,
    meta: &RecordingMeta,
    transcript: &DiarizedTranscript,
    preferred_note: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let preferred = preferred_note
        .filter(|path| path.exists())
        .map(Path::to_path_buf);
    let queued = if preferred.is_none() {
        queue
            .get(id)
            .context("reading recording before existing-note rewrite")?
            .and_then(|row| row.note_path)
            .filter(|path| path.exists())
    } else {
        None
    };
    let Some(existing) = preferred.or(queued) else {
        return Ok(None);
    };
    corti_vagus::note::rewrite_body(&existing, &corti_vagus::recording_body(meta, transcript))
        .context("rewriting the existing note")?;
    Ok(Some(existing))
}

/// File the transcript into vagus and mark the job `Done`. Returns `Err` on a filing failure so the caller
/// can schedule a retry rather than terminally failing.
fn file_and_done(
    ctx: &mut Ctx,
    id: &str,
    meta: &RecordingMeta,
    transcript: &DiarizedTranscript,
    preferred_note: Option<&Path>,
) -> Result<()> {
    // #87: a note may already exist for this recording — a partial/non-canonical live note, or a
    // fully-filed one re-reached by crash recovery. The queue's note_path is the no-double-notes authority.
    if let Some(existing) = rewrite_existing_note(&ctx.queue, id, meta, transcript, preferred_note)?
    {
        info!(
            target: "corti::pipeline",
            job_id = %id,
            note_path = %existing.display(),
            "batch transcript rewrote the existing note in place"
        );
        ctx.queue
            .update(
                id,
                JobUpdate {
                    note_path: Some(existing.clone()),
                    ..Default::default()
                },
            )
            .context("persisting rewritten live note path")?;
        ctx.queue
            .set_status(id, JobStatus::Done)
            .context("marking rewritten live note Done")?;
        let title = meta.note_title();
        tray::update_history(&ctx.app, id, JobStatus::Done, None, None, Some(existing));
        set_stage(&ctx.app, Stage::Idle);
        tray::set_status(&ctx.app, format!("✓ Filed — {title}"));
        return Ok(());
    }

    // Startup discovery may have failed only because vagus wasn't installed yet — `brew install vagus`
    // mid-session is the expected fix, and a tray app shouldn't need a relaunch to notice. Retry once
    // per filing attempt: one `vagus --version` spawn, only on the filing path of a concluded recording,
    // so effectively free. A fresh failure replaces the cached error so the tray always reports current
    // reality, not the state at app launch.
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
        .file_recording(meta, transcript)
        .context("filing note failed")?;
    info!(
        target: "corti::pipeline",
        job_id = %id,
        note_path = %note.display(),
        "note filed"
    );

    // Record the note path first (so a crash here doesn't re-file), then mark Done.
    if let Err(e) = ctx.queue.update(
        id,
        JobUpdate {
            note_path: Some(note.clone()),
            ..Default::default()
        },
    ) {
        warn!(
            target: "corti::pipeline",
            job_id = %id,
            error = %format!("{e:#}"),
            "note filed but could not persist note_path"
        );
    }
    let _ = ctx.queue.set_status(id, JobStatus::Done);

    let title = meta.note_title();
    tray::update_history(
        &ctx.app,
        id,
        JobStatus::Done,
        None,
        None,
        Some(note.clone()),
    );
    set_stage(&ctx.app, Stage::Idle);
    tray::set_status(&ctx.app, format!("✓ Filed — {title}"));
    Ok(())
}

/// Delete the transient capture audio (and the AEC-cleaned sibling, when distinct) after a successful
/// transcription. ADR 0007 defers retention: the captured audio exists only to be transcribed, so once the
/// transcript is filed there is nothing left to keep. Best-effort — a leftover file is a disk nuisance, not
/// a correctness problem, so failures are only logged.
fn prune_transient(raw: &Path, used: &Path) {
    for p in [used, raw] {
        if p.exists() {
            match std::fs::remove_file(p) {
                Ok(()) => info!(
                    target: "corti::pipeline",
                    path = %p.display(),
                    "pruned transient capture audio"
                ),
                Err(e) => warn!(
                    target: "corti::pipeline",
                    path = %p.display(),
                    error = %e,
                    "could not remove transient capture audio"
                ),
            }
        }
    }
}

/// Terminally fail a recording and surface it in the tray. A directly-owned fallback note can be supplied
/// when its earlier row write failed; path + Failed then land in one SQL statement before the note is
/// forgotten.
pub(crate) fn fail_with_note(
    ctx: &Ctx,
    id: &str,
    meta: &RecordingMeta,
    msg: String,
    preferred_note: Option<&Path>,
) {
    error!(target: "corti::pipeline", job_id = %id, error = %msg, "job failed");
    let note_path = preferred_note.map(Path::to_path_buf).or_else(|| {
        ctx.queue
            .get(id)
            .ok()
            .flatten()
            .and_then(|row| row.note_path)
    });
    if let Some(note) = note_path.as_ref() {
        close_out_note(note, &msg);
    }
    if let Err(e) = ctx.queue.update(
        id,
        JobUpdate {
            status: Some(JobStatus::Failed),
            note_path: note_path.clone(),
            error: Some(msg.clone()),
            ..Default::default()
        },
    ) {
        warn!(
            target: "corti::pipeline",
            job_id = %id,
            error = %format!("{e:#}"),
            "could not persist terminal recording failure"
        );
    }
    set_stage(&ctx.app, Stage::Idle);
    tray::update_history(
        &ctx.app,
        id,
        JobStatus::Failed,
        None,
        Some(msg.clone()),
        note_path,
    );
    tray::set_status(&ctx.app, format!("⚠ {} — {msg}", meta.owning_app.name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{OwningApp, Speaker, TranscriptSegment};
    use std::os::unix::fs::MetadataExt;

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

    /// A dropped live result is never accepted as Filed. Its existing note path is carried directly into
    /// the batch branch and rewritten canonically without changing the file/inode, even if queue persistence
    /// did not land first.
    #[test]
    fn dropped_live_result_routes_to_batch_and_rewrites_same_inode() {
        let dir = test_dir("dropped-live-fallback");
        let queue = Queue::open_at(dir.join("queue.db")).unwrap();
        let meta = meta(dir.join("recording.wav"));
        let id = queue.enqueue(&meta).unwrap();
        let note = dir.join("live.md");
        std::fs::write(
            &note,
            format!(
                "---\nsource: x\n---\n\n# T\n\n{}",
                corti_vagus::live_initial_body(&meta)
            ),
        )
        .unwrap();
        let inode_before = std::fs::metadata(&note).unwrap().ino();

        let resolution = resolve_live(Some(crate::live::LiveOutcome::Fallback {
            reason: "live capture tee dropped 2 chunk(s)".to_string(),
            note_path: Some(note.clone()),
        }));
        let LiveResolution::Batch {
            fallback:
                Some(LiveFallback {
                    reason,
                    note_path: Some(partial),
                }),
        } = resolution
        else {
            panic!("a dropped live result must take the batch branch");
        };
        assert!(reason.contains("dropped 2"));
        // Simulate the reviewed failure: the queue write did not land. The in-memory/persisted-retry
        // preferred path must still be sufficient to rewrite this exact note.
        assert_eq!(queue.get(&id).unwrap().unwrap().note_path, None);

        let transcript = DiarizedTranscript::new(vec![TranscriptSegment {
            speaker: Speaker::Me,
            start: 0.0,
            end: 1.0,
            text: "canonical batch text".to_string(),
        }]);
        let rewritten = rewrite_existing_note(&queue, &id, &meta, &transcript, Some(&partial))
            .unwrap()
            .expect("the persisted partial note must be rewritten");
        assert_eq!(rewritten, note);
        assert_eq!(std::fs::metadata(&note).unwrap().ino(), inode_before);
        let content = std::fs::read_to_string(&note).unwrap();
        assert!(content.contains("State: transcribed \n"), "got: {content}");
        assert!(!content.contains("State: transcribing\n"));
        assert!(content.contains("canonical batch text"));
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
