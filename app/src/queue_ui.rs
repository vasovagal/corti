//! Tauri commands behind the Recording Queue window (printer-queue view of the durable queue).
//!
//! Reads go through a per-call **read-only** SQLite connection ([`corti_queue::Queue::open_read_only`])
//! — the pipeline thread stays the database's only writer. Mutations (the Retry button) are messages
//! to that thread via [`PipelineTx`], never direct writes. The window refetches on the coarse
//! `queue-changed` event the pipeline emits whenever anything moves.

use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use corti_core::RecordingMode;
use corti_queue::Queue;
use serde::Serialize;
use tauri::State;

use crate::jobs;
use crate::pipeline::PipelineMsg;

/// A clone of the pipeline worker's channel, managed as Tauri state so commands can message the
/// thread that owns the queue.
pub struct PipelineTx(pub Mutex<Sender<PipelineMsg>>);

/// One row of the Recording Queue window. Everything the state-label mapping needs is resolved
/// server-side at query time (file existence, retry-job presence), so the webview stays a pure view.
#[derive(Debug, Clone, Serialize)]
pub struct RecordingDto {
    pub id: String,
    /// Friendly owning-app name ("Zoom", "Webinar").
    pub app: String,
    /// "call" | "webinar".
    pub mode: String,
    /// RFC3339 timestamps (started always; ended once known).
    pub started_at: String,
    pub ended_at: Option<String>,
    pub duration_secs: Option<i64>,
    /// `JobStatus` wire form ("recording", "pending_transcription", … "done", "failed").
    pub status: String,
    pub error: Option<String>,
    /// Transcription wall-time, for "transcribed 55 min in 30 s".
    pub transcribe_secs: Option<f64>,
    pub note_path: Option<String>,
    /// Whether the note still exists at `note_path` — `false` after vagus reorganizes it out of the
    /// inbox, which the UI shows as "Filed in brain" (not an error).
    pub note_exists: bool,
    pub audio_exists: bool,
    pub audio_bytes: Option<u64>,
    /// An active retry job exists for this recording ("Will retry (attempt n/5)").
    pub retry_pending: bool,
    pub retry_attempts: Option<u32>,
}

/// Every tracked recording, newest first, with file-system facts resolved.
#[tauri::command]
pub fn list_recordings() -> Result<Vec<RecordingDto>, String> {
    // No DB yet (fresh install, nothing recorded) ⇒ an empty queue, not an error.
    let queue = match Queue::open_read_only() {
        Ok(q) => q,
        Err(_) => return Ok(Vec::new()),
    };
    let rows = queue.all().map_err(|e| format!("{e:#}"))?;
    let retries: Vec<(serde_json::Value, u32)> = queue
        .jobs()
        .active_for(jobs::RETRY_TRANSCRIPTION)
        .unwrap_or_default();
    let retry_for = |id: &str| {
        retries
            .iter()
            .find(|(payload, _)| payload["id"].as_str() == Some(id))
            .map(|(_, attempts)| *attempts)
    };

    Ok(rows
        .into_iter()
        .rev() // all() is oldest-first; the window wants newest on top
        .map(|job| {
            let audio_meta = std::fs::metadata(&job.audio_path).ok();
            let note_exists = job.note_path.as_deref().is_some_and(Path::exists);
            let retry_attempts = retry_for(&job.id);
            let mode = match job.meta().mode() {
                RecordingMode::Call => "call",
                RecordingMode::Webinar => "webinar",
            };
            RecordingDto {
                app: job.owning_app.clone(),
                mode: mode.to_string(),
                started_at: job.started_at.to_rfc3339(),
                ended_at: job.ended_at.map(|t| t.to_rfc3339()),
                duration_secs: job.ended_at.map(|end| (end - job.started_at).num_seconds()),
                status: serde_json::to_value(job.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                error: job.error,
                transcribe_secs: job.transcribe_secs,
                note_path: job.note_path.map(|p| p.to_string_lossy().into_owned()),
                note_exists,
                audio_exists: audio_meta.is_some(),
                audio_bytes: audio_meta.map(|m| m.len()),
                retry_pending: retry_attempts.is_some(),
                retry_attempts,
                id: job.id,
            }
        })
        .collect())
}

/// Retry a failed recording: hand the id to the pipeline thread (which re-validates and re-runs it).
#[tauri::command]
pub fn retry_recording(id: String, tx: State<'_, PipelineTx>) -> Result<(), String> {
    tx.0.lock()
        .unwrap()
        .send(PipelineMsg::Retry { id })
        .map_err(|_| "pipeline worker is gone".to_string())
}

/// Open the filed note in the default Markdown handler — only while it still exists at that path.
#[tauri::command]
pub fn open_note(path: String) -> Result<(), String> {
    if !Path::new(&path).exists() {
        return Err("the note has moved (filed in brain)".to_string());
    }
    std::process::Command::new("open")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("opening note: {e}"))
}

/// Reveal a recording's audio file in Finder.
#[tauri::command]
pub fn reveal_audio(id: String) -> Result<(), String> {
    let queue = Queue::open_read_only().map_err(|e| format!("{e:#}"))?;
    let job = queue
        .get(&id)
        .map_err(|e| format!("{e:#}"))?
        .ok_or_else(|| format!("no recording {id}"))?;
    if !job.audio_path.exists() {
        return Err("the audio has already expired".to_string());
    }
    std::process::Command::new("open")
        .arg("-R")
        .arg(&job.audio_path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("revealing audio: {e}"))
}
