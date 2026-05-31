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
use chrono::{DateTime, Local};
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("setting busy_timeout")?;
        // WAL keeps readers and the writer from blocking each other and survives crashes cleanly.
        // (journal_mode returns the resulting mode as a row, so read it back rather than execute().)
        let _: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .context("enabling WAL")?;
        conn.execute_batch(SCHEMA).context("creating schema")?;
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
        self.conn
            .execute(
                "INSERT OR IGNORE INTO recordings
                   (id, started_at, ended_at, owning_app, bundle_id, audio_path, status, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![id, started, ended, name, bundle, audio, status, now],
            )
            .context("inserting recording row")?;
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
        let n = self
            .conn
            .execute(
                "UPDATE recordings SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, status_to_string(status), fmt_dt(Local::now())],
            )
            .context("updating status")?;
        if n == 0 {
            bail!("no recording with id {id}");
        }
        Ok(())
    }

    /// Apply a partial [`JobUpdate`] atomically (only `Some` fields change; `updated_at` always bumps).
    pub fn update(&self, id: &str, fields: JobUpdate) -> Result<()> {
        let status = fields.status.map(status_to_string);
        let note_path = fields.note_path.map(|p| p.to_string_lossy().into_owned());
        let n = self
            .conn
            .execute(
                "UPDATE recordings SET
                   status         = COALESCE(?2, status),
                   s3_uri         = COALESCE(?3, s3_uri),
                   transcribe_job = COALESCE(?4, transcribe_job),
                   note_path      = COALESCE(?5, note_path),
                   error          = COALESCE(?6, error),
                   updated_at     = ?7
                 WHERE id = ?1",
                params![
                    id,
                    status,
                    fields.s3_uri,
                    fields.transcribe_job,
                    note_path,
                    fields.error,
                    fmt_dt(Local::now()),
                ],
            )
            .context("updating job fields")?;
        if n == 0 {
            bail!("no recording with id {id}");
        }
        Ok(())
    }

    /// Mark a job `Failed` with an error message (convenience over [`update`](Queue::update)).
    pub fn fail(&self, id: &str, error: &str) -> Result<()> {
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

    /// Delete `Done` rows last updated before `older_than`, returning the audio paths of the pruned
    /// recordings so the caller can delete the cached WAVs (this method touches only the DB). Lets the
    /// recordings cache be reclaimed on a retention policy, e.g. `Local::now() - Duration::days(30)`.
    pub fn prune_done(&self, older_than: DateTime<Local>) -> Result<Vec<PathBuf>> {
        let done = status_to_string(JobStatus::Done);
        let cutoff = fmt_dt(older_than);
        let paths: Vec<PathBuf> = {
            let mut stmt = self.conn.prepare(
                "SELECT audio_path FROM recordings WHERE status = ?1 AND updated_at < ?2",
            )?;
            stmt.query_map(params![done, cutoff], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(PathBuf::from)
                .collect()
        };
        self.conn
            .execute(
                "DELETE FROM recordings WHERE status = ?1 AND updated_at < ?2",
                params![done, cutoff],
            )
            .context("pruning done rows")?;
        Ok(paths)
    }
}

/// Columns in storage order; shared by every `SELECT` so [`read_row`] indices stay aligned.
const COLS: &str = "id, started_at, ended_at, owning_app, bundle_id, audio_path, \
                    status, s3_uri, transcribe_job, note_path, error, updated_at";

const SCHEMA: &str = "
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

/// The job id for a recording: its filename stem (already unique + stable), falling back to the start
/// time if the path somehow has no stem.
fn job_id(meta: &RecordingMeta) -> String {
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
        updated_at: parse_dt(&r.updated_at)?,
    })
}

fn fmt_dt(dt: DateTime<Local>) -> String {
    dt.to_rfc3339()
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
    fn prune_done_removes_old_done_rows_and_returns_paths() {
        let q = Queue::memory();
        let a = q.enqueue(&meta("/cache/a.wav", "us.zoom.xos")).unwrap();
        let b = q.enqueue(&meta("/cache/b.wav", "us.zoom.xos")).unwrap();
        q.set_status(&a, JobStatus::Done).unwrap();
        // b stays PendingTranscription.

        // Cutoff in the future ⇒ the Done row counts as old and is pruned.
        let removed = q
            .prune_done(Local::now() + chrono::Duration::seconds(60))
            .unwrap();
        assert_eq!(removed, vec![PathBuf::from("/cache/a.wav")]);
        assert!(q.get(&a).unwrap().is_none());
        assert!(q.get(&b).unwrap().is_some()); // non-terminal never pruned

        // A past cutoff prunes nothing (the just-marked Done row is newer than it).
        let a2 = q.enqueue(&meta("/cache/a.wav", "us.zoom.xos")).unwrap();
        q.set_status(&a2, JobStatus::Done).unwrap();
        assert!(
            q.prune_done(Local::now() - chrono::Duration::hours(1))
                .unwrap()
                .is_empty()
        );
        assert!(q.get(&a2).unwrap().is_some());
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
