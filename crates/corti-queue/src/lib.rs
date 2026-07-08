//! Durable job store for corti: crash-recoverable pipeline state.
//!
//! Every recording moves through `Recording → PendingTranscription → Transcribing → PendingNote → Done`
//! (or `Failed`); each non-terminal state is **resumable on startup** so a crash mid-upload/transcribe/file
//! never loses the note (guardrail 7). The store is one SQLite database (WAL) at
//! `~/.local/share/corti/queue.db` — outside any vault (guardrail 5). [`JobStatus`] and its
//! [`is_terminal`](corti_core::JobStatus::is_terminal) live in `corti-core`; this crate persists it.
//!
//! Idempotency (so crash recovery is free and safe): the job id is the recording's filename stem, so
//! [`enqueue`](Queue::enqueue) is a no-op if the recording is already tracked, and the AWS backend's
//! `transcribe_job` name is stored so a re-poll re-attaches to the existing job instead of paying to start
//! a new one. `note_path` is stored so a re-file doesn't duplicate the note.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use corti_core::{JobStatus, OwningApp, RecordingMeta};
use rusqlite::{Connection, OptionalExtension, params};

/// Where the queue DB lives. Outside any vault. Override with `$CORTI_DATA_DIR`.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("CORTI_DATA_DIR") {
        return Ok(PathBuf::from(d));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("cannot resolve $HOME")?;
    Ok(home.join(".local/share/corti"))
}

/// One tracked recording and where it is in the pipeline. Returned by [`Queue::get`],
/// [`Queue::resumable`], and [`Queue::all`].
// No `Eq`: `transcribe_secs` is an `f64` (only `PartialEq`), same situation as `AppConfig`.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// Stable id (the recording's filename stem, e.g. `20260530-135756-slack`).
    pub id: String,
    pub started_at: DateTime<Local>,
    pub ended_at: Option<DateTime<Local>>,
    /// Friendly app name (e.g. `Zoom`).
    pub owning_app: String,
    pub bundle_id: Option<String>,
    pub audio_path: PathBuf,
    pub status: JobStatus,
    /// `s3://…` URI of the staged audio (AWS backend).
    pub s3_uri: Option<String>,
    /// AWS Transcribe job name, kept so a re-poll attaches to the existing job.
    pub transcribe_job: Option<String>,
    /// Path of the filed vagus note, kept so a re-file doesn't duplicate it.
    pub note_path: Option<PathBuf>,
    /// Last error (set alongside `Failed`).
    pub error: Option<String>,
    /// Wall-clock seconds the (successful) transcription took, for the Queue UI's
    /// "transcribed 55 min in 30 s" line.
    pub transcribe_secs: Option<f64>,
    pub updated_at: DateTime<Local>,
}

impl Job {
    /// Reconstruct the [`RecordingMeta`] this job was enqueued from, for resuming the pipeline.
    pub fn meta(&self) -> RecordingMeta {
        RecordingMeta {
            started_at: self.started_at,
            ended_at: self.ended_at,
            owning_app: OwningApp {
                bundle_id: self.bundle_id.clone(),
                name: self.owning_app.clone(),
            },
            audio_path: self.audio_path.clone(),
        }
    }
}

/// A partial update to a job's fields. Only `Some` fields are written (`None` leaves the stored value
/// unchanged); a field cannot be cleared back to NULL through this. Use with [`Queue::update`].
#[derive(Debug, Default, Clone)]
pub struct JobUpdate {
    pub status: Option<JobStatus>,
    pub s3_uri: Option<String>,
    pub transcribe_job: Option<String>,
    pub note_path: Option<PathBuf>,
    pub error: Option<String>,
    pub transcribe_secs: Option<f64>,
    /// Re-point the row at a different audio file (the legacy-WAV backfill swaps `.wav` → `.ogg`).
    pub audio_path: Option<PathBuf>,
    /// Stamp the recording's end time. Needed by #87: a row created mid-call (live note filing) has
    /// no end time yet, and the later `enqueue` is an `INSERT OR IGNORE` no-op that can't supply it.
    pub ended_at: Option<DateTime<Local>>,
}

/// The durable job store. Open with [`Queue::open`] (or [`Queue::open_at`] for an explicit path).
pub struct Queue {
    conn: Connection,
}

impl Queue {
    /// Open (creating if needed) the queue DB at the default location ([`data_dir`]`/queue.db`).
    pub fn open() -> Result<Self> {
        Self::open_at(data_dir()?.join("queue.db"))
    }

    /// Open (creating if needed) the queue DB at an explicit path. Creates parent dirs, enables WAL, and
    /// runs the schema migration.
    pub fn open_at(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut conn =
            Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("setting busy_timeout")?;
        // WAL keeps readers and the writer from blocking each other and survives crashes cleanly.
        // (journal_mode returns the resulting mode as a row, so read it back rather than execute().)
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .context("enabling WAL")?;
        migrate(&mut conn).context("migrating queue schema")?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
        corti_jobs::Jobs::ensure_schema(&conn)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .context("stamping user_version")?;
        Ok(Self { conn })
    }

    /// Borrow the embedded background-jobs store. Same connection, same single owner thread — the
    /// returned view must not outlive this `Queue` (the borrow enforces it).
    pub fn jobs(&self) -> corti_jobs::Jobs<'_> {
        corti_jobs::Jobs::new(&self.conn)
    }

    /// Open the queue **read-only** — for UI command threads that must never become a second writer
    /// (the pipeline thread stays the sole one; WAL supports concurrent readers beside it). Skips the
    /// migration (read-only can't run it; the pipeline's open already has). Errors if the DB doesn't
    /// exist yet, and any accidental write fails loudly at the SQLite layer.
    pub fn open_read_only() -> Result<Self> {
        let path = data_dir()?.join("queue.db");
        let conn = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening {} read-only", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("setting busy_timeout")?;
        Ok(Self { conn })
    }

    /// Enqueue a finished recording for transcription, returning its job id. **Idempotent**: if this
    /// recording (same id) is already tracked, the existing row is left untouched (its progress is
    /// preserved) and its id is returned.
    pub fn enqueue(&self, meta: &RecordingMeta) -> Result<String> {
        let id = job_id(meta);
        let started = fmt_dt(meta.started_at);
        let ended = meta.ended_at.map(fmt_dt);
        let name = meta.owning_app.name.clone();
        let bundle = meta.owning_app.bundle_id.clone();
        let audio = meta.audio_path.to_string_lossy().into_owned();
        let status = status_to_string(JobStatus::PendingTranscription);
        let now = fmt_dt(Local::now());
        let inserted = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO recordings
                   (id, started_at, ended_at, owning_app, bundle_id, audio_path, status, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, started, ended, name, bundle, audio, status, now],
            )
            .context("inserting recording row")?;
        // `INSERT OR IGNORE` is a no-op when the recording is already tracked; say so rather than implying a
        // fresh enqueue, so the log reads honestly when diagnosing a re-delivery.
        if inserted > 0 {
            tracing::debug!(target: "corti::queue", job_id = %id, status = %status, "enqueue");
        } else {
            tracing::debug!(target: "corti::queue", job_id = %id, "enqueue (already tracked)");
        }
        Ok(id)
    }

    /// Fetch a single job by id.
    pub fn get(&self, id: &str) -> Result<Option<Job>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLS} FROM recordings WHERE id = ?1"))?;
        let raw = stmt.query_row(params![id], read_row).optional()?;
        raw.map(raw_to_job).transpose()
    }

    /// Set a job's pipeline status.
    pub fn set_status(&self, id: &str, status: JobStatus) -> Result<()> {
        let status_str = status_to_string(status);
        let n = self
            .conn
            .execute(
                "UPDATE recordings SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, status_str, fmt_dt(Local::now())],
            )
            .context("updating status")?;
        if n == 0 {
            bail!("no recording with id {id}");
        }
        tracing::debug!(target: "corti::queue", job_id = %id, status = %status_str, "set_status");
        Ok(())
    }

    /// Apply a partial [`JobUpdate`] atomically (only `Some` fields change; `updated_at` always bumps).
    pub fn update(&self, id: &str, fields: JobUpdate) -> Result<()> {
        let status = fields.status.map(status_to_string);
        let note_path = fields.note_path.map(|p| p.to_string_lossy().into_owned());
        let status_log = status.clone();
        let note_changed = note_path.is_some();
        let n = self
            .conn
            .execute(
                "UPDATE recordings SET
                   status          = COALESCE(?2, status),
                   s3_uri          = COALESCE(?3, s3_uri),
                   transcribe_job  = COALESCE(?4, transcribe_job),
                   note_path       = COALESCE(?5, note_path),
                   error           = COALESCE(?6, error),
                   transcribe_secs = COALESCE(?7, transcribe_secs),
                   audio_path      = COALESCE(?8, audio_path),
                   ended_at        = COALESCE(?9, ended_at),
                   updated_at      = ?10
                 WHERE id = ?1",
                params![
                    id,
                    status,
                    fields.s3_uri,
                    fields.transcribe_job,
                    note_path,
                    fields.error,
                    fields.transcribe_secs,
                    fields.audio_path.map(|p| p.to_string_lossy().into_owned()),
                    fields.ended_at.map(fmt_dt),
                    fmt_dt(Local::now()),
                ],
            )
            .context("updating job fields")?;
        if n == 0 {
            bail!("no recording with id {id}");
        }
        tracing::debug!(
            target: "corti::queue",
            job_id = %id,
            status = status_log.as_deref().unwrap_or("(unchanged)"),
            note_path_set = note_changed,
            "update"
        );
        Ok(())
    }

    /// Reset a `Failed` recording for a (manual) retry: status back to `PendingTranscription` with the
    /// error **cleared** — the COALESCE-based [`update`](Queue::update) can't NULL a field, hence the
    /// dedicated method. Errors if the row isn't currently `Failed` (the UI races the pipeline).
    pub fn retry_reset(&self, id: &str) -> Result<()> {
        let n = self
            .conn
            .execute(
                "UPDATE recordings SET status = ?2, error = NULL, updated_at = ?3
                 WHERE id = ?1 AND status = ?4",
                params![
                    id,
                    status_to_string(JobStatus::PendingTranscription),
                    fmt_dt(Local::now()),
                    status_to_string(JobStatus::Failed),
                ],
            )
            .context("resetting failed recording")?;
        if n == 0 {
            bail!("no Failed recording with id {id}");
        }
        Ok(())
    }

    /// Mark a job `Failed` with an error message (convenience over [`update`](Queue::update)).
    pub fn fail(&self, id: &str, error: &str) -> Result<()> {
        tracing::warn!(target: "corti::queue", job_id = %id, error, "job failed");
        self.update(
            id,
            JobUpdate {
                status: Some(JobStatus::Failed),
                error: Some(error.to_string()),
                ..Default::default()
            },
        )
    }

    /// All jobs in a non-terminal state, oldest first — the work to resume on startup.
    pub fn resumable(&self) -> Result<Vec<Job>> {
        let done = status_to_string(JobStatus::Done);
        let failed = status_to_string(JobStatus::Failed);
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM recordings WHERE status NOT IN (?1, ?2) ORDER BY started_at"
        ))?;
        let raws = stmt
            .query_map(params![done, failed], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raws.into_iter().map(raw_to_job).collect()
    }

    /// Every job, oldest first (for inspection/debugging).
    pub fn all(&self) -> Result<Vec<Job>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM recordings ORDER BY started_at"
        ))?;
        let raws = stmt
            .query_map([], read_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raws.into_iter().map(raw_to_job).collect()
    }

    /// Terminal (`Done` | `Failed`) rows last updated before `older_than` — the retention sweep's
    /// audio-deletion candidates, oldest first. **Read-only**: rows now outlive their audio, so the
    /// Recording Queue window keeps showing history ("Filed in brain") after the files are reclaimed.
    pub fn expired(&self, older_than: DateTime<Local>) -> Result<Vec<Job>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM recordings
             WHERE status IN (?1, ?2) AND updated_at < ?3 ORDER BY started_at"
        ))?;
        let raws = stmt
            .query_map(
                params![
                    status_to_string(JobStatus::Done),
                    status_to_string(JobStatus::Failed),
                    fmt_dt(older_than),
                ],
                read_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raws.into_iter().map(raw_to_job).collect()
    }

    /// Row GC: delete terminal (`Done` | `Failed`) rows last updated before `older_than`. Bounds
    /// queue.db and the Recording Queue's history list on a much longer horizon than the audio
    /// retention (90 days vs ~7), so "Filed in brain" history stays useful for a quarter. Returns how
    /// many rows went; non-terminal rows are never touched.
    pub fn delete_terminal_older_than(&self, older_than: DateTime<Local>) -> Result<usize> {
        self.conn
            .execute(
                "DELETE FROM recordings WHERE status IN (?1, ?2) AND updated_at < ?3",
                params![
                    status_to_string(JobStatus::Done),
                    status_to_string(JobStatus::Failed),
                    fmt_dt(older_than),
                ],
            )
            .context("deleting expired terminal rows")
    }
}

/// Columns in a fixed order shared by every `SELECT` so [`read_row`] indices stay aligned.
const COLS: &str = "id, started_at, ended_at, owning_app, bundle_id, audio_path, \
                    status, s3_uri, transcribe_job, note_path, error, updated_at, \
                    transcribe_secs";

/// Bumped whenever [`migrate`] gains a step; stamped into `PRAGMA user_version` on every open.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS recordings(
  id              TEXT PRIMARY KEY,
  started_at      TEXT NOT NULL,
  ended_at        TEXT,
  owning_app      TEXT NOT NULL,
  bundle_id       TEXT,
  audio_path      TEXT NOT NULL,
  status          TEXT NOT NULL,
  s3_uri          TEXT,
  transcribe_job  TEXT,
  note_path       TEXT,
  error           TEXT,
  updated_at      TEXT NOT NULL,
  transcribe_secs REAL
);
CREATE INDEX IF NOT EXISTS idx_recordings_status ON recordings(status);
";

/// Bring a pre-existing DB up to [`SCHEMA_VERSION`]. Runs before `execute_batch(SCHEMA)`, so a fresh DB
/// (no `recordings` table yet) skips straight through and is created in the current shape. Every step is
/// safe to re-run: a crash between a step committing and the `user_version` stamp just replays it.
fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .context("reading user_version")?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    let has_table: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='recordings')",
            [],
            |r| r.get(0),
        )
        .context("probing for recordings table")?;
    if version < 1 && has_table {
        migrate_v0_to_v1(conn).context("migrating queue schema v0 → v1")?;
    }
    Ok(())
}

/// v0 → v1: add `transcribe_secs`, and rewrite every stored timestamp to the fixed-width UTC `…Z` form
/// [`fmt_dt`] now writes. v0 stored `DateTime<Local>::to_rfc3339()` — local-*offset* strings whose lexical
/// order diverges from chronological order the moment two rows carry different offsets (DST transition,
/// timezone travel), corrupting `ORDER BY started_at` and every `updated_at < cutoff` retention check.
fn migrate_v0_to_v1(conn: &mut Connection) -> Result<()> {
    let tx = conn.transaction().context("starting migration tx")?;
    // Replayed after a crash mid-migration, the column may already be there; the rewrite below is
    // idempotent (re-formatting a `…Z` string is a no-op).
    let has_col: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('recordings') WHERE name='transcribe_secs')",
            [],
            |r| r.get(0),
        )
        .context("probing for transcribe_secs column")?;
    if !has_col {
        tx.execute("ALTER TABLE recordings ADD COLUMN transcribe_secs REAL", [])
            .context("adding transcribe_secs column")?;
    }
    let rows: Vec<(String, String, Option<String>, String)> = {
        let mut stmt = tx.prepare("SELECT id, started_at, ended_at, updated_at FROM recordings")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()
            .context("reading rows for timestamp rewrite")?
    };
    {
        let mut write = tx.prepare(
            "UPDATE recordings SET started_at = ?2, ended_at = ?3, updated_at = ?4 WHERE id = ?1",
        )?;
        for (id, started, ended, updated) in rows {
            write
                .execute(params![
                    id,
                    reformat_utc(&started)?,
                    ended.as_deref().map(reformat_utc).transpose()?,
                    reformat_utc(&updated)?,
                ])
                .with_context(|| format!("rewriting timestamps for {id}"))?;
        }
    }
    tx.commit().context("committing v0 → v1 migration")
}

/// Any-offset RFC3339 → the canonical UTC `…Z` form (what [`fmt_dt`] writes).
fn reformat_utc(s: &str) -> Result<String> {
    Ok(fmt_dt(parse_dt(s)?))
}

/// The job id for a recording: its filename stem (already unique + stable), falling back to the start
/// time if the path somehow has no stem. Public so callers (the tray) can key a pre-enqueue history
/// entry by the same id the queue will assign on [`enqueue`](Queue::enqueue).
pub fn job_id(meta: &RecordingMeta) -> String {
    meta.audio_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| meta.started_at.format("%Y%m%d-%H%M%S%3f").to_string())
}

/// Raw column values straight from SQLite, before typed conversion (keeps the rusqlite closure free of
/// fallible parsing — that happens in [`raw_to_job`] with `anyhow` errors).
struct RawRow {
    id: String,
    started_at: String,
    ended_at: Option<String>,
    owning_app: String,
    bundle_id: Option<String>,
    audio_path: String,
    status: String,
    s3_uri: Option<String>,
    transcribe_job: Option<String>,
    note_path: Option<String>,
    error: Option<String>,
    updated_at: String,
    transcribe_secs: Option<f64>,
}

fn read_row(row: &rusqlite::Row) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        id: row.get(0)?,
        started_at: row.get(1)?,
        ended_at: row.get(2)?,
        owning_app: row.get(3)?,
        bundle_id: row.get(4)?,
        audio_path: row.get(5)?,
        status: row.get(6)?,
        s3_uri: row.get(7)?,
        transcribe_job: row.get(8)?,
        note_path: row.get(9)?,
        error: row.get(10)?,
        updated_at: row.get(11)?,
        transcribe_secs: row.get(12)?,
    })
}

fn raw_to_job(r: RawRow) -> Result<Job> {
    Ok(Job {
        id: r.id,
        started_at: parse_dt(&r.started_at)?,
        ended_at: r.ended_at.as_deref().map(parse_dt).transpose()?,
        owning_app: r.owning_app,
        bundle_id: r.bundle_id,
        audio_path: PathBuf::from(r.audio_path),
        status: status_from_str(&r.status)?,
        s3_uri: r.s3_uri,
        transcribe_job: r.transcribe_job,
        note_path: r.note_path.map(PathBuf::from),
        error: r.error,
        transcribe_secs: r.transcribe_secs,
        updated_at: parse_dt(&r.updated_at)?,
    })
}

/// The canonical stored form: UTC, millisecond precision, `Z` suffix — fixed-width, so lexical order
/// (which SQLite's TEXT comparisons use) equals chronological order. Never store a local offset here;
/// that's the v0 bug [`migrate_v0_to_v1`] cleans up.
fn fmt_dt(dt: DateTime<Local>) -> String {
    dt.with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_dt(s: &str) -> Result<DateTime<Local>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("parsing timestamp {s:?}"))?
        .with_timezone(&Local))
}

/// `JobStatus` → its snake_case wire form (the canonical encoding the `status` column stores). Using the
/// `corti-core` serde representation keeps the DB in sync if a variant is ever added.
fn status_to_string(status: JobStatus) -> String {
    match serde_json::to_value(status) {
        Ok(serde_json::Value::String(v)) => v,
        _ => unreachable!("JobStatus always serializes to a JSON string"),
    }
}

fn status_from_str(s: &str) -> Result<JobStatus> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .with_context(|| format!("unrecognized JobStatus {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    impl Queue {
        /// In-memory DB for tests (no filesystem, no path collisions).
        fn memory() -> Self {
            let conn = Connection::open_in_memory().unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            corti_jobs::Jobs::ensure_schema(&conn).unwrap();
            Self { conn }
        }
    }

    fn meta(path: &str, bundle: &str) -> RecordingMeta {
        RecordingMeta {
            started_at: Local.with_ymd_and_hms(2026, 5, 30, 14, 5, 0).unwrap(),
            ended_at: Some(Local.with_ymd_and_hms(2026, 5, 30, 14, 35, 0).unwrap()),
            owning_app: OwningApp::from_bundle_id(bundle),
            audio_path: PathBuf::from(path),
        }
    }

    #[test]
    fn enqueue_creates_pending_and_get_round_trips_meta() {
        let q = Queue::memory();
        let m = meta("/cache/20260530-140500-zoom.wav", "us.zoom.xos");
        let id = q.enqueue(&m).unwrap();
        assert_eq!(id, "20260530-140500-zoom");

        let job = q.get(&id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::PendingTranscription);
        assert_eq!(job.owning_app, "Zoom");
        assert_eq!(job.bundle_id.as_deref(), Some("us.zoom.xos"));
        assert_eq!(job.audio_path, m.audio_path);

        // meta() faithfully reconstructs the enqueued metadata.
        let back = job.meta();
        assert_eq!(back.owning_app.name, "Zoom");
        assert_eq!(back.owning_app.bundle_id.as_deref(), Some("us.zoom.xos"));
        assert_eq!(back.started_at, m.started_at);
        assert_eq!(back.ended_at, m.ended_at);
        assert_eq!(back.audio_path, m.audio_path);
    }

    #[test]
    fn enqueue_is_idempotent_and_preserves_progress() {
        let q = Queue::memory();
        let m = meta("/cache/a.wav", "us.zoom.xos");
        let id = q.enqueue(&m).unwrap();
        q.update(
            &id,
            JobUpdate {
                status: Some(JobStatus::Transcribing),
                transcribe_job: Some("job-123".into()),
                ..Default::default()
            },
        )
        .unwrap();

        // Re-enqueue the same recording: same id, existing progress untouched.
        let id2 = q.enqueue(&m).unwrap();
        assert_eq!(id, id2);
        let job = q.get(&id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Transcribing);
        assert_eq!(job.transcribe_job.as_deref(), Some("job-123"));
    }

    #[test]
    fn set_status_updates_and_missing_id_errors() {
        let q = Queue::memory();
        let id = q.enqueue(&meta("/cache/a.wav", "us.zoom.xos")).unwrap();
        q.set_status(&id, JobStatus::PendingNote).unwrap();
        assert_eq!(q.get(&id).unwrap().unwrap().status, JobStatus::PendingNote);
        assert!(q.set_status("nope", JobStatus::Done).is_err());
    }

    #[test]
    fn partial_update_leaves_other_fields_intact() {
        let q = Queue::memory();
        let id = q.enqueue(&meta("/cache/a.wav", "us.zoom.xos")).unwrap();
        q.update(
            &id,
            JobUpdate {
                s3_uri: Some("s3://bucket/a.wav".into()),
                ..Default::default()
            },
        )
        .unwrap();
        // A second, disjoint update must not wipe s3_uri.
        q.update(
            &id,
            JobUpdate {
                note_path: Some(PathBuf::from("/vault/note.md")),
                ..Default::default()
            },
        )
        .unwrap();

        let job = q.get(&id).unwrap().unwrap();
        assert_eq!(job.s3_uri.as_deref(), Some("s3://bucket/a.wav"));
        assert_eq!(job.note_path, Some(PathBuf::from("/vault/note.md")));
    }

    /// #87: a row created mid-call has no end time; `JobUpdate.ended_at` stamps it later (and a
    /// `None` leaves an existing value intact, like every other partial field).
    #[test]
    fn update_stamps_ended_at() {
        let q = Queue::memory();
        let mut m = meta("/cache/a.wav", "us.zoom.xos");
        m.ended_at = None; // row created while still recording (LiveNoteCreated)
        let id = q.enqueue(&m).unwrap();
        assert_eq!(q.get(&id).unwrap().unwrap().ended_at, None);

        let ended = Local.with_ymd_and_hms(2026, 5, 30, 14, 35, 0).unwrap();
        q.update(
            &id,
            JobUpdate {
                ended_at: Some(ended),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(q.get(&id).unwrap().unwrap().ended_at, Some(ended));

        // A later disjoint update must not clear it.
        q.update(
            &id,
            JobUpdate {
                status: Some(JobStatus::Done),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(q.get(&id).unwrap().unwrap().ended_at, Some(ended));
    }

    #[test]
    fn resumable_excludes_terminal_states() {
        let q = Queue::memory();
        let a = q.enqueue(&meta("/cache/a.wav", "us.zoom.xos")).unwrap();
        let b = q
            .enqueue(&meta("/cache/b.wav", "com.tinyspeck.slackmacgap"))
            .unwrap();
        let c = q.enqueue(&meta("/cache/c.wav", "com.hnc.Discord")).unwrap();
        q.set_status(&a, JobStatus::Transcribing).unwrap();
        q.set_status(&b, JobStatus::Done).unwrap();
        q.fail(&c, "boom").unwrap();

        let ids: Vec<String> = q.resumable().unwrap().into_iter().map(|j| j.id).collect();
        assert_eq!(ids, vec!["a".to_string()]);

        let failed = q.get(&c).unwrap().unwrap();
        assert_eq!(failed.status, JobStatus::Failed);
        assert_eq!(failed.error.as_deref(), Some("boom"));
    }

    #[test]
    fn expired_lists_terminal_rows_read_only() {
        let q = Queue::memory();
        let a = q.enqueue(&meta("/cache/a.ogg", "us.zoom.xos")).unwrap();
        let b = q.enqueue(&meta("/cache/b.ogg", "us.zoom.xos")).unwrap();
        let c = q.enqueue(&meta("/cache/c.ogg", "us.zoom.xos")).unwrap();
        q.set_status(&a, JobStatus::Done).unwrap();
        q.fail(&b, "boom").unwrap();
        // c stays PendingTranscription.

        // Future cutoff ⇒ both terminal rows are sweep candidates; the pending one never is.
        let future = Local::now() + chrono::Duration::seconds(60);
        let ids: Vec<String> = q
            .expired(future)
            .unwrap()
            .into_iter()
            .map(|j| j.id)
            .collect();
        assert_eq!(ids, vec![a.clone(), b.clone()]);
        // Read-only: every row (and its status) survives the query.
        assert_eq!(q.get(&a).unwrap().unwrap().status, JobStatus::Done);
        assert_eq!(q.get(&b).unwrap().unwrap().status, JobStatus::Failed);
        let _ = c;

        // A past cutoff matches nothing (the rows were just touched).
        assert!(
            q.expired(Local::now() - chrono::Duration::hours(1))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn delete_terminal_older_than_spares_non_terminal() {
        let q = Queue::memory();
        let a = q.enqueue(&meta("/cache/a.ogg", "us.zoom.xos")).unwrap();
        let b = q.enqueue(&meta("/cache/b.ogg", "us.zoom.xos")).unwrap();
        let c = q.enqueue(&meta("/cache/c.ogg", "us.zoom.xos")).unwrap();
        q.set_status(&a, JobStatus::Done).unwrap();
        q.fail(&b, "boom").unwrap();

        let n = q
            .delete_terminal_older_than(Local::now() + chrono::Duration::seconds(60))
            .unwrap();
        assert_eq!(n, 2);
        assert!(q.get(&a).unwrap().is_none());
        assert!(q.get(&b).unwrap().is_none());
        assert!(
            q.get(&c).unwrap().is_some(),
            "non-terminal rows are never GC'd"
        );
    }

    #[test]
    fn status_strings_round_trip_all_variants() {
        for s in [
            JobStatus::Recording,
            JobStatus::PendingTranscription,
            JobStatus::Transcribing,
            JobStatus::PendingNote,
            JobStatus::Done,
            JobStatus::Failed,
        ] {
            assert_eq!(status_from_str(&status_to_string(s)).unwrap(), s);
        }
        assert_eq!(
            status_to_string(JobStatus::PendingTranscription),
            "pending_transcription"
        );
        assert!(status_from_str("not_a_status").is_err());
    }

    /// The v0 schema verbatim (pre-`user_version`, pre-`transcribe_secs`, local-offset timestamps), for
    /// migration tests. Keep frozen — this is what shipped DBs look like.
    const V0_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS recordings(
  id             TEXT PRIMARY KEY,
  started_at     TEXT NOT NULL,
  ended_at       TEXT,
  owning_app     TEXT NOT NULL,
  bundle_id      TEXT,
  audio_path     TEXT NOT NULL,
  status         TEXT NOT NULL,
  s3_uri         TEXT,
  transcribe_job TEXT,
  note_path      TEXT,
  error          TEXT,
  updated_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_recordings_status ON recordings(status);
";

    #[test]
    fn migrates_v0_db_to_utc_and_adds_transcribe_secs() {
        let dir = std::env::temp_dir().join("corti-queue-test-migrate-v0");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.db");

        // Build a v0 DB whose lexical timestamp order CONTRADICTS chronological order:
        // `earlier` = 23:30+12:00 = 11:30Z, `later` = 08:00-07:00 = 15:00Z — yet "08:…" < "23:…".
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(V0_SCHEMA).unwrap();
            for (id, ts) in [
                ("later", "2026-03-08T08:00:00-07:00"),
                ("earlier", "2026-03-08T23:30:00+12:00"),
            ] {
                conn.execute(
                    "INSERT INTO recordings (id, started_at, owning_app, audio_path, status, updated_at)
                     VALUES (?1, ?2, 'Zoom', '/cache/x.wav', 'done', ?2)",
                    params![id, ts],
                )
                .unwrap();
            }
            // v0 lexical ordering really is wrong — the precondition this migration exists for.
            let first: String = conn
                .query_row(
                    "SELECT id FROM recordings ORDER BY started_at LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(first, "later");
        }

        let q = Queue::open_at(&path).unwrap();
        let version: i64 = q
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Chronological order restored, timestamps canonicalized, new column readable as NULL.
        let jobs = q.all().unwrap();
        assert_eq!(
            jobs.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(),
            vec!["earlier", "later"]
        );
        for job in &jobs {
            assert_eq!(job.transcribe_secs, None);
            let raw: String = q
                .conn
                .query_row(
                    "SELECT started_at FROM recordings WHERE id = ?1",
                    params![&job.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(raw.ends_with('Z'), "expected UTC Z form, got {raw}");
        }

        // Reopening (already at v1) is a clean no-op.
        drop(q);
        let q = Queue::open_at(&path).unwrap();
        assert_eq!(q.all().unwrap().len(), 2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fmt_dt_is_fixed_width_sortable_utc() {
        let dt = Local.with_ymd_and_hms(2026, 5, 30, 14, 5, 0).unwrap();
        let s = fmt_dt(dt);
        assert!(s.ends_with('Z'), "expected Z suffix: {s}");
        assert_eq!(s.len(), "2026-05-30T21:05:00.000Z".len());
        assert_eq!(parse_dt(&s).unwrap(), dt); // round-trips through the canonical form
    }

    #[test]
    fn retry_reset_requires_failed_and_clears_error() {
        let q = Queue::memory();
        let id = q.enqueue(&meta("/cache/a.wav", "us.zoom.xos")).unwrap();
        // Not Failed yet ⇒ refuses (the UI can race the pipeline).
        assert!(q.retry_reset(&id).is_err());

        q.fail(&id, "boom").unwrap();
        q.retry_reset(&id).unwrap();
        let job = q.get(&id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::PendingTranscription);
        assert_eq!(job.error, None);
    }

    #[test]
    fn update_records_transcribe_secs() {
        let q = Queue::memory();
        let id = q.enqueue(&meta("/cache/a.wav", "us.zoom.xos")).unwrap();
        q.update(
            &id,
            JobUpdate {
                transcribe_secs: Some(30.5),
                ..Default::default()
            },
        )
        .unwrap();
        let job = q.get(&id).unwrap().unwrap();
        assert_eq!(job.transcribe_secs, Some(30.5));
        assert_eq!(job.status, JobStatus::PendingTranscription); // untouched
    }

    #[test]
    fn persists_across_reopen() {
        // The heart of the durability story: write, drop the connection, reopen, read it back.
        let dir = std::env::temp_dir().join("corti-queue-test-reopen");
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("queue.db");

        let id = {
            let q = Queue::open_at(&path).unwrap();
            q.enqueue(&meta("/cache/x.wav", "us.zoom.xos")).unwrap()
        };

        let q = Queue::open_at(&path).unwrap();
        let job = q.get(&id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::PendingTranscription);
        assert_eq!(job.owning_app, "Zoom");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn data_dir_respects_override() {
        // SAFETY: single-threaded test; only this test reads/writes CORTI_DATA_DIR.
        unsafe { std::env::set_var("CORTI_DATA_DIR", "/tmp/corti-queue-test-data") };
        assert_eq!(
            data_dir().unwrap(),
            PathBuf::from("/tmp/corti-queue-test-data")
        );
        unsafe { std::env::remove_var("CORTI_DATA_DIR") };
    }
}
