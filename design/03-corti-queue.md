# 03 — corti-queue

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

## API (target)
```rust
pub struct Queue { /* rusqlite::Connection */ }
impl Queue {
    pub fn open() -> Result<Self>;                                   // creates dir + schema
    pub fn enqueue(&self, meta: &RecordingMeta) -> Result<String>;   // returns id, status=PendingTranscription
    pub fn set_status(&self, id: &str, status: JobStatus) -> Result<()>;
    pub fn set_fields(&self, id: &str, /* s3_uri, transcribe_job, note_path, error */) -> Result<()>;
    pub fn resumable(&self) -> Result<Vec<Job>>;                     // rows in non-terminal states
}
```
`JobStatus` and its `is_terminal()` live in `corti-core` already.

## Crash recovery (on app startup)
For each `resumable()` row, resume from its status: re-upload/re-poll the AWS job (idempotent by job name),
re-run transcription, or re-file via corti-vagus. Idempotency: keep the AWS job name so re-poll attaches to
the existing job instead of starting a new one; keep `note_path` so a re-file doesn't duplicate.

## Notes
- DB lives **outside** any vault (guardrail 5).
- Prune `Done` rows + their cached WAVs on a retention policy (e.g. keep 30 days) — the recordings cache can
  get large.

## Depends on
`corti-core` (RecordingMeta, JobStatus), `rusqlite` (bundled), `chrono`.
