# 03 — corti-queue

> **Stale — design-time snapshot.** Superseded by [docs/transcription.md](../docs/transcription.md); kept for
> design rationale. The `recordings` state machine below is accurate, but durability is now delivered by the
> `corti-jobs` layer on top of this queue (#85), not the `resumable()`/`prune_done` helpers sketched here:
> transient transcribe/file failures schedule a durable retry job with backoff, orphaned jobs are recovered
> at startup, and an hourly sweep enforces retention. See `crates/corti-jobs/src/lib.rs` and `app/src/jobs.rs`.

Durable job store so a crash mid-pipeline never loses a recording (guardrail 7). A recording moves
`Recording → PendingTranscription → Transcribing → PendingNote → Done` (or `Failed`); every non-terminal
state is resumable on startup.

## Storage
`rusqlite` (bundled feature) at `~/.local/share/corti/queue.db`, WAL mode. One row per recording:
```sql
CREATE TABLE recordings(
  id            TEXT PRIMARY KEY,   -- uuid or the recording stem
  started_at    TEXT NOT NULL,      -- RFC3339
  ended_at      TEXT,
  owning_app    TEXT,               -- friendly name
  bundle_id     TEXT,
  audio_path    TEXT NOT NULL,      -- 2-track WAV in the recordings cache
  status        TEXT NOT NULL,      -- JobStatus (snake_case, matches corti-core)
  s3_uri        TEXT,               -- aws backend
  transcribe_job TEXT,              -- aws job name (to re-poll)
  note_path     TEXT,              -- vagus note once filed
  error         TEXT,
  updated_at    TEXT NOT NULL
);
```

## API (implemented)
```rust
pub struct Queue { /* rusqlite::Connection */ }
impl Queue {
    pub fn open() -> Result<Self>;                                   // default path; creates dir + schema
    pub fn open_at(path: impl AsRef<Path>) -> Result<Self>;          // explicit path (tests, custom data dir)
    pub fn enqueue(&self, meta: &RecordingMeta) -> Result<String>;   // id = recording stem; idempotent;
                                                                      //   status = PendingTranscription
    pub fn get(&self, id: &str) -> Result<Option<Job>>;
    pub fn set_status(&self, id: &str, status: JobStatus) -> Result<()>;
    pub fn update(&self, id: &str, fields: JobUpdate) -> Result<()>; // partial: s3_uri/transcribe_job/
                                                                      //   note_path/error/status (the doc's
                                                                      //   `set_fields`, via SQL COALESCE)
    pub fn fail(&self, id: &str, error: &str) -> Result<()>;         // status=Failed + error
    pub fn resumable(&self) -> Result<Vec<Job>>;                     // rows in non-terminal states
    pub fn all(&self) -> Result<Vec<Job>>;                           // every row (inspection)
    pub fn prune_done(&self, older_than: DateTime<Local>) -> Result<Vec<PathBuf>>; // delete old Done rows,
                                                                      //   return their WAV paths to unlink
}
```
`Job` mirrors a row; `Job::meta()` rebuilds the `RecordingMeta` to resume the pipeline. `JobStatus` and its
`is_terminal()` live in `corti-core`; the `status` column stores its snake_case serde form (single source
of truth, so adding a variant Just Works). **Idempotency** is keyed on the **recording filename stem** as
the primary key: a re-`enqueue` of an in-flight recording is a no-op that preserves progress.

## Crash recovery (on app startup)
For each `resumable()` row, resume from its status: re-upload/re-poll the AWS job (idempotent by job name),
re-run transcription, or re-file via corti-vagus. Idempotency: keep the AWS job name so re-poll attaches to
the existing job instead of starting a new one; keep `note_path` so a re-file doesn't duplicate.

## Notes
- DB lives **outside** any vault (guardrail 5): `~/.local/share/corti/queue.db` (WAL; override via
  `$CORTI_DATA_DIR`). Verified durable across reopen (the crash-recovery premise) in a unit test.
- `prune_done(cutoff)` deletes old `Done` rows and **returns** their cached WAV paths; the caller unlinks
  the files (keeps this crate DB-only and file deletion explicit/loggable). App policy e.g. keep 30 days.

## Depends on
`corti-core` (RecordingMeta, JobStatus), `rusqlite` (bundled), `chrono`.
