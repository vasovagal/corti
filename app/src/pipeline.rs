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

use crate::imp::{HISTORY_LIMIT, HistoryEntry};
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
}

/// Worker entry point. Opens the queue, seeds the tray history, then drains the channel until the app
/// exits. Holds the [`SharedConfig`] so a Settings save (`PipelineMsg::ReloadConfig`) can rebuild the
/// backend live.
pub fn run(
    app: AppHandle,
    config: SharedConfig,
    rx: Receiver<PipelineMsg>,
    stats: crate::stats::StatsBuffer,
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
    };

    seed_history(&ctx);
    match ctx.queue.jobs().recover_running() {
        Ok(0) => {}
        Ok(n) => eprintln!("[corti] re-queued {n} background job(s) orphaned by the last shutdown"),
        Err(e) => eprintln!("[corti] cannot recover background jobs: {e:#}"),
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
                    if let Err(e) = transcribe_and_file(&mut ctx, &id, &meta, &audio_path) {
                        schedule_retry(&ctx, &id, &meta, e);
                    }
                }
                Err(e) => {
                    error!(
                        target: "corti::pipeline",
                        path = %audio_path.display(),
                        error = %format!("{e:#}"),
                        "enqueue failed"
                    );
                    tray::set_status(&ctx.app, format!("⚠ enqueue failed: {e}"));
                }
            },
            Ok(PipelineMsg::Retry { id }) => manual_retry(&mut ctx, &id),
            Ok(PipelineMsg::ReloadConfig) => reload_config(&mut ctx, &config),
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
fn schedule_retry(ctx: &Ctx, id: &str, meta: &RecordingMeta, err: anyhow::Error) {
    let msg = format!("{err:#}");
    eprintln!("[corti] job {id} failed (will retry): {msg}");
    let update = ctx.queue.update(
        id,
        JobUpdate {
            status: Some(JobStatus::PendingTranscription),
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
    tray::update_history(&ctx.app, id, JobStatus::PendingNote, None, None, None);
    // File stage (vagus filing). No backend dimension; label "vagus".
    let tf = std::time::Instant::now();
    let result = file_and_done(ctx, id, meta, &transcript);
    ctx.stats.record_stage("file", tf.elapsed(), "vagus");
    result
}

/// File the transcript into vagus and mark the job `Done`. Returns `Err` on a filing failure so the caller
/// can schedule a retry rather than terminally failing.
fn file_and_done(
    ctx: &mut Ctx,
    id: &str,
    meta: &RecordingMeta,
    transcript: &DiarizedTranscript,
) -> Result<()> {
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

/// Terminally fail a recording and surface it in the tray (both the status line and its history
/// entry). Reserved for unrecoverable states — transient errors go through [`schedule_retry`] /
/// the retry job's backoff instead.
pub(crate) fn fail(ctx: &Ctx, id: &str, meta: &RecordingMeta, msg: String) {
    error!(target: "corti::pipeline", job_id = %id, error = %msg, "job failed");
    let _ = ctx.queue.fail(id, &msg);
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
