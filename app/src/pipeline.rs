//! The pipeline worker: the single thread that owns the durable [`Queue`] and pushes every recording
//! through `enqueue → transcribe → file → Done`, resuming crashed jobs on startup (guardrail 7).
//!
//! Why one thread: rusqlite's `Connection` is `Send` but not `Sync`, so the `Queue` has exactly one owner
//! here and is never shared. Transcription blocks (the backend runs its own runtime), so doing it on this
//! dedicated thread keeps it off the Tauri UI loop (guardrail 9). Jobs run serially — fine, since
//! transcription is inherently sequential and a second call simply waits in the channel.
//!
//! ## Crash recovery shape (maps 1:1 to `JobStatus`)
//! - `PendingTranscription` / `Transcribing` → (re)transcribe; the stable AWS job name re-attaches.
//! - `PendingNote` → re-file from the persisted transcript **sidecar** (or mark Done if already filed).
//! - `Recording` → an orphaned capture; no recorder survives a crash, so fail it.
//!
//! The **sidecar** (`<recording>.transcript.json`) is corti's own durability detail: we persist the parsed
//! transcript the moment it's ready, so a crash between "transcript ready" and "note filed" can re-file
//! without re-transcribing — the AWS-staged result may already have been deleted.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;

use anyhow::{Context, Result};
use corti_core::{DiarizedTranscript, JobStatus, RecordingMeta};
use corti_queue::{Job, JobUpdate, Queue};
use corti_vagus::Vagus;
use tauri::AppHandle;

use crate::imp::{HISTORY_LIMIT, HistoryEntry};
use crate::settings::SharedConfig;
use crate::transcribe::Backend;
use crate::tray;

/// How long a `Done` recording (and its WAV) is kept before [`Queue::prune_done`] reclaims it.
const RETENTION_DAYS: i64 = 30;

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
    app: AppHandle,
}

/// Worker entry point. Opens the queue, recovers/prunes, then drains the channel until the app exits. Holds
/// the [`SharedConfig`] so a Settings save (`PipelineMsg::ReloadConfig`) can rebuild the backend live.
pub fn run(app: AppHandle, config: SharedConfig, rx: Receiver<PipelineMsg>) {
    let queue = match Queue::open() {
        Ok(q) => q,
        Err(e) => {
            eprintln!("[corti] cannot open job queue: {e:#}");
            tray::set_status(&app, format!("⚠ queue unavailable: {e}"));
            return;
        }
    };

    let vagus = Vagus::discover().map_err(|e| format!("{e:#}"));
    if let Err(e) = &vagus {
        eprintln!("[corti] vagus not available — notes can't be filed: {e}");
    }

    // Snapshot the shared config to build the initial backend. Capture the AEC toggle before `Backend::init`
    // consumes the snapshot.
    let cfg = config.lock().unwrap().clone();
    let aec_enabled = cfg.aec_enabled;
    let mut ctx = Ctx {
        queue,
        vagus,
        backend: Backend::init(cfg),
        aec_enabled,
        app,
    };

    seed_history(&ctx);
    recover(&mut ctx);
    prune(&ctx);
    tray::set_status(&ctx.app, "Idle — waiting for a call".to_string());

    for msg in rx {
        match msg {
            PipelineMsg::Process { meta, audio_path } => match ctx.queue.enqueue(&meta) {
                Ok(id) => {
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
                    eprintln!("[corti] enqueue failed for {}: {e:#}", audio_path.display());
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
    ctx.backend = Backend::init(cfg);
    eprintln!(
        "[corti] settings saved — backend now {backend_name}; AEC {}",
        if ctx.aec_enabled { "on" } else { "off" }
    );
}

/// Seed the tray's `History ▸` submenu with the most recent recordings from the durable queue, so the
/// history survives a restart (issue #3). Best-effort: a read failure just leaves the history empty until
/// the next live recording. Runs on the worker thread (the sole `Queue` owner), never the UI loop.
fn seed_history(ctx: &Ctx) {
    let jobs = match ctx.queue.all() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[corti] could not read recordings for tray history: {e:#}");
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

/// Resume every non-terminal job left by a previous run.
fn recover(ctx: &mut Ctx) {
    let jobs = match ctx.queue.resumable() {
        Ok(j) => j,
        Err(e) => {
            eprintln!("[corti] could not read resumable jobs: {e:#}");
            return;
        }
    };
    if jobs.is_empty() {
        return;
    }
    eprintln!("[corti] resuming {} unfinished job(s)", jobs.len());
    tray::set_status(
        &ctx.app,
        format!("Resuming {} unfinished job(s)…", jobs.len()),
    );
    for job in jobs {
        resume(ctx, job);
    }
}

fn resume(ctx: &mut Ctx, job: Job) {
    let meta = job.meta();
    match job.status {
        JobStatus::Recording => {
            // A capture orphaned by the crash; no recorder survives (no partial WAV). Nothing to salvage.
            let msg = "recording interrupted by shutdown";
            let _ = ctx.queue.fail(&job.id, msg);
            tray::update_history(
                &ctx.app,
                &job.id,
                JobStatus::Failed,
                None,
                Some(msg.to_string()),
                None,
            );
        }
        JobStatus::PendingTranscription | JobStatus::Transcribing => {
            transcribe_and_file(ctx, &job.id, &meta, &job.audio_path);
        }
        JobStatus::PendingNote => finish_note(ctx, &job, &meta),
        // Terminal states are never returned by `resumable()`.
        JobStatus::Done | JobStatus::Failed => {}
    }
}

/// Transcribe `audio`, persist the transcript sidecar, then file it. Used for both fresh recordings and
/// `PendingTranscription` / `Transcribing` resumes.
fn transcribe_and_file(ctx: &mut Ctx, id: &str, meta: &RecordingMeta, audio: &Path) {
    // Record the stable Transcribe job name + advance to Transcribing BEFORE transcribing, so a crash
    // mid-transcribe re-attaches to the same (idempotent) AWS job on resume.
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
    tray::set_status(
        &ctx.app,
        format!("Transcribing — {}…", meta.owning_app.name),
    );
    tray::update_history(&ctx.app, id, JobStatus::Transcribing, None, None, None);

    // Run the shared transcription core: offline AEC (regenerating the clean WAV on resume is an
    // idempotent, cheap overwrite) → backend → persist the transcript sidecar next to the WAV. The
    // pipeline's input is always the raw 2-track recording, so `skip_aec` is false; a tap-only recording
    // is handled inside (`Ok(None)`). Only a real transcription failure returns `Err` here.
    let transcript = match crate::transcribe::transcribe_recording(
        &ctx.backend,
        ctx.aec_enabled,
        false,
        id,
        meta,
        audio,
    ) {
        Ok((transcript, _input)) => transcript,
        Err(e) => {
            fail(ctx, id, meta, format!("transcription failed: {e:#}"));
            return;
        }
    };

    // Persist the transcript next to the WAV so re-filing after a crash never needs to re-transcribe.
    // Best-effort: even if this fails we still hold the transcript in memory for this run. (The shared
    // `transcribe_recording` no longer does this — it's a pipeline/crash-recovery concern, not the CLI's.)
    let sidecar = sidecar_path(audio);
    if let Err(e) = write_sidecar(&sidecar, &transcript) {
        eprintln!(
            "[corti] could not persist transcript sidecar {}: {e:#}",
            sidecar.display()
        );
    }

    if let Err(e) = ctx.queue.set_status(id, JobStatus::PendingNote) {
        fail(ctx, id, meta, format!("queue set PendingNote: {e:#}"));
        return;
    }
    tray::update_history(&ctx.app, id, JobStatus::PendingNote, None, None, None);
    file_and_done(ctx, id, meta, &transcript, &sidecar);
}

/// Resume a `PendingNote` job: it's already transcribed, so just re-file (idempotently).
fn finish_note(ctx: &mut Ctx, job: &Job, meta: &RecordingMeta) {
    let sidecar = sidecar_path(&job.audio_path);

    // Already filed once (note_path set): nothing to redo — just mark Done and clean up.
    if job.note_path.is_some() {
        let _ = ctx.queue.set_status(&job.id, JobStatus::Done);
        let _ = std::fs::remove_file(&sidecar);
        return;
    }

    match read_sidecar(&sidecar) {
        Some(transcript) => file_and_done(ctx, &job.id, meta, &transcript, &sidecar),
        // No sidecar (e.g. it failed to write): fall back to re-transcribing. The stable job name lets the
        // AWS job re-attach if its staged output still exists.
        None => transcribe_and_file(ctx, &job.id, meta, &job.audio_path),
    }
}

/// File the transcript into vagus and mark the job `Done`.
fn file_and_done(
    ctx: &mut Ctx,
    id: &str,
    meta: &RecordingMeta,
    transcript: &DiarizedTranscript,
    sidecar: &Path,
) {
    // Startup discovery may have failed only because vagus wasn't installed yet — `brew install vagus`
    // mid-session is the expected fix, and a tray app shouldn't need a relaunch to notice. Retry once
    // per filing attempt: one `vagus --version` spawn, only on the filing path of a concluded recording,
    // so effectively free. A fresh failure replaces the cached error so the tray always reports current
    // reality, not the state at app launch.
    if ctx.vagus.is_err() {
        ctx.vagus = Vagus::discover().map_err(|e| format!("{e:#}"));
        if let Ok(v) = &ctx.vagus {
            eprintln!(
                "[corti] vagus now available at `{}` — filing enabled",
                v.bin().display()
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

    // Record the note path first (so a crash here doesn't re-file), then mark Done.
    if let Err(e) = ctx.queue.update(
        id,
        JobUpdate {
            note_path: Some(note.clone()),
            ..Default::default()
        },
    ) {
        eprintln!("[corti] note filed but could not persist note_path for {id}: {e:#}");
    }
    let _ = ctx.queue.set_status(id, JobStatus::Done);
    let _ = std::fs::remove_file(sidecar); // transcript is safely in the note now

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
}

/// Reclaim old `Done` recordings: delete their cached WAVs (and any leftover sidecar) on the retention
/// policy. The queue deletes its own rows and hands back the WAV paths to unlink.
fn prune(ctx: &Ctx) {
    let cutoff = chrono::Local::now() - chrono::Duration::days(RETENTION_DAYS);
    match ctx.queue.prune_done(cutoff) {
        Ok(paths) => {
            for p in &paths {
                let _ = std::fs::remove_file(p);
                let _ = std::fs::remove_file(sidecar_path(p));
                let _ = std::fs::remove_file(corti_capture::clean_wav_path(p));
            }
            if !paths.is_empty() {
                eprintln!("[corti] pruned {} old recording(s)", paths.len());
            }
        }
        Err(e) => eprintln!("[corti] prune failed: {e:#}"),
    }
}

/// Fail a job and surface it in the tray (both the status line and its history entry).
fn fail(ctx: &Ctx, id: &str, meta: &RecordingMeta, msg: String) {
    eprintln!("[corti] job {id} failed: {msg}");
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

/// `<recording>.wav` → `<recording>.transcript.json` (next to the WAV, in the recordings cache).
fn sidecar_path(audio: &Path) -> PathBuf {
    audio.with_extension("transcript.json")
}

fn write_sidecar(path: &Path, transcript: &DiarizedTranscript) -> Result<()> {
    let file =
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?;
    serde_json::to_writer(std::io::BufWriter::new(file), transcript)
        .context("serializing transcript sidecar")?;
    Ok(())
}

fn read_sidecar(path: &Path) -> Option<DiarizedTranscript> {
    let file = std::fs::File::open(path).ok()?;
    serde_json::from_reader(std::io::BufReader::new(file)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{Speaker, TranscriptSegment};

    #[test]
    fn sidecar_sits_next_to_the_wav() {
        assert_eq!(
            sidecar_path(Path::new(
                "/cache/corti/recordings/20260530-140500-zoom.wav"
            )),
            PathBuf::from("/cache/corti/recordings/20260530-140500-zoom.transcript.json")
        );
    }

    #[test]
    fn sidecar_round_trips_a_transcript() {
        let dir = std::env::temp_dir().join("corti-app-sidecar-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = sidecar_path(&dir.join("rec.wav"));

        let transcript = DiarizedTranscript::new(vec![TranscriptSegment {
            speaker: Speaker::Me,
            start: 0.0,
            end: 1.5,
            text: "hello".into(),
        }]);
        write_sidecar(&path, &transcript).unwrap();
        assert_eq!(read_sidecar(&path), Some(transcript));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_sidecar_is_none_when_absent() {
        assert!(read_sidecar(Path::new("/nope/does-not-exist.transcript.json")).is_none());
    }
}
