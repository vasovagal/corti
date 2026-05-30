//! Durable job store for corti — **shell, not yet implemented**.
//!
//! Persists each recording's pipeline state so a crash mid-upload/transcribe/file resumes rather than loses
//! the note (guardrail 7). Plan + schema in `design/03-corti-queue.md`. Backed by rusqlite (WAL) at
//! `~/.local/share/corti/queue.db` — outside any vault.

use std::path::PathBuf;

use anyhow::{Result, anyhow};
use corti_core::RecordingMeta;

/// Where the queue DB lives. Outside any vault. Override with `$CORTI_DATA_DIR`.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CORTI_DATA_DIR") {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot resolve $HOME"))?;
    Ok(home.join(".local/share/corti"))
}

/// The durable job store. Construct with [`Queue::open`].
pub struct Queue {
    _db_path: PathBuf,
}

impl Queue {
    /// Open (and create) the queue DB. Currently a shell: resolves the path but does not yet open SQLite.
    pub fn open() -> Result<Self> {
        let path = data_dir()?.join("queue.db");
        // TODO(design/03-corti-queue.md): create dir, open rusqlite (WAL), run schema migrations.
        Ok(Self { _db_path: path })
    }

    /// Enqueue a finished recording for transcription. Returns its job id.
    pub fn enqueue(&self, _meta: &RecordingMeta) -> Result<String> {
        Err(anyhow!("corti-queue is not yet implemented"))
    }
}
