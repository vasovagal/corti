//! The pipeline worker: the single thread that owns the [`Queue`] and pushes every recording through
//! `enqueue → transcribe → file → Done`.
//!
//! Why one thread: rusqlite's `Connection` is `Send` but not `Sync`, so the `Queue` has exactly one owner
//! here and is never shared. Transcription blocks (the backend runs its own runtime), so doing it on this
//! dedicated thread keeps it off the Tauri UI loop (guardrail 9). Jobs run serially — fine, since
//! transcription is inherently sequential and a second call simply waits in the channel.
//!
//! ## Durability deferred (ADR 0007)
//! Crash recovery and retention are intentionally not implemented: the pipeline runs in-process and a crash
//! or quit mid-call loses *that* call. The `Queue` is kept as a session-spanning record so the tray history
//! (issue #3) and `corti --list` still work across restarts, but no startup recovery or retention sweep
//! runs. Revisit before any real release.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use anyhow::Result;
use corti_core::{DiarizedTranscript, JobStatus, RecordingMeta};
use corti_queue::{Job, JobUpdate, Queue};
use corti_vagus::Vagus;
use tauri::AppHandle;
use tracing::{error, info, warn};

use crate::imp::{HISTORY_LIMIT, HistoryEntry, Stage, set_stage};
use crate::settings::SharedConfig;
use crate::transcribe::Backend;
use crate::tray;

/// Work for the pipeline worker.
pub enum PipelineMsg {
    /// A finished recording to push through transcribe → file → Done.
    Process {
        meta: RecordingMeta,
        audio_path: PathBuf,
    },
    /// Re-read the shared config and rebuild the backend + AEC toggle. Sent by the Settings screen on save;
    /// handled between jobs (the worker is serial) so it applies to the next recording — or immediately when
    /// the worker is idle and blocked waiting for one.
    ReloadConfig,
}

/// Everything the worker owns. Built once on the worker thread; never shared.
struct Ctx {
    queue: Queue,
    /// Discovered at startup; if that failed, re-probed on each filing attempt (see [`file_and_done`])
    /// so installing vagus mid-session works without a relaunch. `Err` is the stringified, user-facing
    /// reason shown when filing fails — we still record + transcribe rather than blocking the whole
    /// pipeline at startup.
    vagus: Result<Vagus, String>,
    backend: Backend,
    /// Whether to clean speaker bleed (offline AEC) before transcribing. Captured from config at startup.
    aec_enabled: bool,
    aec_config: corti_aec::AecConfig,
    app: AppHandle,
    /// Clone of the managed stats buffer; the worker records coarse stage wall-clock here.
    stats: crate::stats::StatsBuffer,
    /// Low-cardinality backend label for stage samples; refreshed on a live backend switch.
    backend_label: &'static str,
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
    };

    seed_history(&ctx);
    tray::set_status(&ctx.app, "Idle — waiting for a call".to_string());

    for msg in rx {
        match msg {
            PipelineMsg::Process { meta, audio_path } => match ctx.queue.enqueue(&meta) {
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
                    transcribe_and_file(&mut ctx, &id, &meta, &audio_path);
                }
                Err(e) => {
                    error!(
                        target: "corti::pipeline",
                        path = %audio_path.display(),
                        error = %format!("{e:#}"),
                        "enqueue failed"
                    );
                    // No job id yet, so `fail()` (which resets the stage) is never reached — reset here so
                    // the diagram doesn't sit on Transcribing forever.
                    set_stage(&ctx.app, Stage::Idle);
                    tray::set_status(&ctx.app, format!("⚠ enqueue failed: {e}"));
                }
            },
            PipelineMsg::ReloadConfig => reload_config(&mut ctx, &config),
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

/// Transcribe `audio`, then file it.
fn transcribe_and_file(ctx: &mut Ctx, id: &str, meta: &RecordingMeta, audio: &Path) {
    // Record the stable Transcribe job name + advance to Transcribing before transcribing.
    if let Err(e) = ctx.queue.update(
        id,
        JobUpdate {
            status: Some(JobStatus::Transcribing),
            transcribe_job: Some(id.to_string()),
            ..Default::default()
        },
    ) {
        fail(
            ctx,
            id,
            meta,
            format!("queue update before transcribe: {e:#}"),
        );
        return;
    }
    set_stage(&ctx.app, Stage::Transcribing);
    tray::set_status(
        &ctx.app,
        format!("Transcribing — {}…", meta.owning_app.name),
    );
    tray::update_history(&ctx.app, id, JobStatus::Transcribing, None, None, None);

    // Run the shared transcription core: AEC (speaker-bleed removal) → backend. The pipeline's input is the
    // lossless 2-track capture, so `skip_aec` is false; a tap-only recording is handled inside (`Ok(None)`).
    // Only a real transcription failure returns `Err` here.
    let _t0 = std::time::Instant::now();
    let transcript = match crate::transcribe::transcribe_recording(
        &ctx.backend,
        ctx.aec_enabled,
        false,
        &ctx.aec_config,
        id,
        meta,
        audio,
    ) {
        Ok((transcript, input)) => {
            // Record the wall-clock of the transcribe (AEC + backend) stage. For the AWS backend this is
            // poll-sleep I/O-wait; for local it is CPU — the backend label lets the panel distinguish them.
            ctx.stats
                .record_stage("transcribe", _t0.elapsed(), ctx.backend_label);
            // ADR 0007 (no retention): the capture audio is a **transient** intermediate, kept only long
            // enough to transcribe. Delete it — and the cleaned sibling AEC produced, when distinct — now
            // that the transcript is in hand. Durability/retention are deferred, so nothing keeps these.
            prune_transient(audio, &input);
            transcript
        }
        Err(e) => {
            // Still record the wall-clock spent before failure (useful for spotting AWS timeouts).
            ctx.stats
                .record_stage("transcribe", _t0.elapsed(), ctx.backend_label);
            // Transcription failed — keep the capture audio so a `--redo` can re-run it (AEC has no raw
            // fallback once it runs at capture time; the lossless 2-track on disk is the only copy).
            fail(ctx, id, meta, format!("transcription failed: {e:#}"));
            return;
        }
    };

    if let Err(e) = ctx.queue.set_status(id, JobStatus::PendingNote) {
        fail(ctx, id, meta, format!("queue set PendingNote: {e:#}"));
        return;
    }
    set_stage(&ctx.app, Stage::Filing);
    tray::update_history(&ctx.app, id, JobStatus::PendingNote, None, None, None);
    // File stage (vagus filing). No backend dimension; label "vagus". Recorded after the call, which
    // always returns (file_and_done is a separate fn), so this fires on both success and filing failure.
    let _tf = std::time::Instant::now();
    file_and_done(ctx, id, meta, &transcript);
    ctx.stats.record_stage("file", _tf.elapsed(), "vagus");
}

/// File the transcript into vagus and mark the job `Done`.
fn file_and_done(ctx: &mut Ctx, id: &str, meta: &RecordingMeta, transcript: &DiarizedTranscript) {
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
        Err(e) => {
            fail(ctx, id, meta, format!("vagus unavailable: {e}"));
            return;
        }
    };

    let note = match vagus.file_recording(meta, transcript) {
        Ok(p) => p,
        Err(e) => {
            fail(ctx, id, meta, format!("filing note failed: {e:#}"));
            return;
        }
    };
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

/// Fail a job and surface it in the tray (both the status line and its history entry).
fn fail(ctx: &Ctx, id: &str, meta: &RecordingMeta, msg: String) {
    error!(target: "corti::pipeline", job_id = %id, error = %msg, "job failed");
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
