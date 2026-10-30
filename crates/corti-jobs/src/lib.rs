//! A deliberately small durable background-job layer: a `jobs` table living in corti's queue.db,
//! drained by the pipeline worker between (and after) its channel messages.
//!
//! Shape: kinds are strings, payloads are JSON; the app defines kind constants and handlers. The narrow
//! recording APIs additionally preserve typed quality evidence and immutable origin. One-shot jobs retry with exponential backoff up to
//! `max_attempts`, then park as `failed` (row kept for inspection/GC). Periodic jobs are per-kind
//! singletons that reschedule themselves on completion *and* on failure — a broken sweep must tick at
//! its period, not backoff-spam.
//!
//! Threading: [`Jobs`] only ever **borrows** a [`rusqlite::Connection`] (in corti, the pipeline
//! thread's — the queue DB's single owner, see `corti-queue`). It is a temporary view, never stored.
//!
//! Crash recovery: a claim marks the row `running` and bumps `attempts` *before* the handler runs, so
//! [`recover_running`](Jobs::recover_running) at startup can simply flip `running → pending, due now`
//! — a crash-looping job still exhausts its attempt budget instead of retrying forever.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OptionalExtension, params};

/// Exponential backoff policy for one-shot jobs: `delay(n) = min(cap, base · 2^(n-1))`.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
}

impl Backoff {
    /// Delay after the `attempts`-th failure (1-based: first failure ⇒ `base`).
    pub fn delay(&self, attempts: u32) -> Duration {
        let factor = 2u64.saturating_pow(attempts.saturating_sub(1));
        self.cap.min(Duration::from_secs(
            self.base.as_secs().saturating_mul(factor),
        ))
    }
}

/// A job handed out by [`Jobs::claim_due`] — already marked `running`, `attempts` already bumped.
/// Pass it back to exactly one of [`Jobs::complete`] / [`Jobs::fail`].
#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    /// Attempts *including* this run (first run ⇒ 1).
    pub attempts: u32,
    pub max_attempts: u32,
    /// `Some` ⇒ periodic singleton; `None` ⇒ one-shot.
    pub period_secs: Option<u64>,
}

/// What [`Jobs::fail`] did with the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    /// Will run again at the contained time (backoff for one-shots, next period for periodics).
    Rescheduled { next_run_at: DateTime<Utc> },
    /// One-shot out of attempts: parked as `failed` (row kept; see
    /// [`delete_terminal_older_than`](Jobs::delete_terminal_older_than)).
    Exhausted,
}

/// Borrowed view over the jobs table. Construct per use via [`Jobs::new`]; never stored.
pub struct Jobs<'c> {
    conn: &'c Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs(
  id           INTEGER PRIMARY KEY,
  kind         TEXT NOT NULL,
  payload      TEXT NOT NULL DEFAULT '{}',
  state        TEXT NOT NULL DEFAULT 'pending',
  attempts     INTEGER NOT NULL DEFAULT 0,
  max_attempts INTEGER NOT NULL DEFAULT 5,
  next_run_at  TEXT NOT NULL,
  period_secs  INTEGER,
  last_error   TEXT,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_active_dedupe
  ON jobs(kind, payload) WHERE state IN ('pending','running');
CREATE INDEX IF NOT EXISTS idx_jobs_due ON jobs(state, next_run_at);
";

/// Optional settled live quality in a recording payload. Errors never include untrusted JSON values.
pub fn settled_quality(payload: &serde_json::Value) -> Result<Option<corti_core::QualityRecord>> {
    match payload.get("settled_quality") {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|_| anyhow::anyhow!("invalid_recording_quality")),
    }
}

/// Add facts using the shared bounded record, without mutating the payload on validation/conflict.
pub fn merge_settled_quality(
    payload: &mut serde_json::Value,
    incoming: &corti_core::QualityRecord,
) -> Result<()> {
    ensure!(payload.is_object(), "invalid_recording_job_payload");
    incoming
        .validate()
        .map_err(|_| anyhow::anyhow!("invalid_recording_quality"))?;
    let merged = match settled_quality(payload)? {
        Some(mut previous) => {
            previous
                .merge_snapshot(incoming)
                .map_err(|_| anyhow::anyhow!("conflicting_recording_quality"))?;
            previous
        }
        None => incoming.clone(),
    };
    let value =
        serde_json::to_value(merged).map_err(|_| anyhow::anyhow!("invalid_recording_quality"))?;
    payload["settled_quality"] = value;
    Ok(())
}

fn merge_recording_payload(
    existing: &serde_json::Value,
    incoming: &serde_json::Value,
) -> Result<serde_json::Value> {
    let id = existing
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .context("invalid_recording_job_payload")?;
    ensure!(
        incoming.get("id").and_then(serde_json::Value::as_str) == Some(id),
        "conflicting_recording_job_identity"
    );
    settled_quality(existing)?;
    let mut merged = existing.clone();
    if let Some(quality) = settled_quality(incoming)? {
        merge_settled_quality(&mut merged, &quality)?;
    }
    let promote = incoming
        .get("preferred_note")
        .and_then(serde_json::Value::as_str)
        .is_some()
        && (existing
            .get("preferred_note")
            .and_then(serde_json::Value::as_str)
            .is_none()
            || incoming["note_canonical"].as_bool() == Some(true)
                && existing["note_canonical"].as_bool() != Some(true));
    if promote {
        merged["preferred_note"] = incoming["preferred_note"].clone();
        merged["note_canonical"] =
            serde_json::Value::Bool(incoming["note_canonical"].as_bool().unwrap_or(false));
    }
    Ok(merged)
}

fn recording_claim(conn: &Connection, job: &ClaimedJob) -> Result<ClaimedJob> {
    let (raw, state, attempts, max_attempts): (String, String, u32, u32) = conn
        .query_row(
            "SELECT payload,state,attempts,max_attempts FROM jobs WHERE id=?1 AND kind=?2",
            params![job.id, job.kind],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .context("reading recording claim")?;
    ensure!(
        job.period_secs.is_none() && state == "running" && attempts == job.attempts,
        "stale_recording_claim"
    );
    let existing =
        serde_json::from_str(&raw).map_err(|_| anyhow::anyhow!("invalid_recording_job_payload"))?;
    let mut merged = job.clone();
    merged.payload = merge_recording_payload(&existing, &job.payload)?;
    merged.max_attempts = max_attempts;
    Ok(merged)
}

impl<'c> Jobs<'c> {
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Create the jobs table + indexes (idempotent). Called by `corti-queue` on every open.
    pub fn ensure_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(SCHEMA).context("creating jobs schema")
    }

    /// Enqueue a one-shot job, returning its id — or `None` if an identical `(kind, payload)` job is
    /// already pending/running (the partial unique index makes re-enqueueing crash-safe and free).
    pub fn enqueue(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        max_attempts: u32,
        run_at: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        let now = fmt_utc(Utc::now());
        let n = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO jobs (kind, payload, max_attempts, next_run_at, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![kind, payload.to_string(), max_attempts, fmt_utc(run_at), now],
            )
            .context("enqueueing job")?;
        Ok((n == 1).then(|| self.conn.last_insert_rowid()))
    }

    /// Arm existing recording work before its first side effect. Unlike generic [`Self::enqueue`],
    /// identity is `(kind, payload.id)`, so adding origin/note metadata never creates a second active
    /// retry. An existing origin (including its absence), first-attempt flag, claim, budget and due time
    /// are immutable here. Note ownership and settled quality merge monotonically, including into a
    /// parked failed job; incidental delivery never restarts exhausted work.
    pub fn enqueue_recording(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        max_attempts: u32,
        run_at: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        self.enqueue_recording_inner(kind, payload, max_attempts, run_at, false)
    }

    /// Explicit manual retry: atomically preserve the recording payload and rearm a parked failure.
    /// Active claims/backoff remain untouched; only an already failed job gets the authorized new budget.
    /// First-attempt permission is consumed even if the historical job was originally fresh.
    pub fn rearm_failed_recording(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        max_attempts: u32,
        run_at: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        self.enqueue_recording_inner(kind, payload, max_attempts, run_at, true)
    }

    /// Queue-owned manual retry, on the same transaction as its recording-row reset. Requiring an
    /// existing transaction prevents callers from accidentally splitting those durability authorities.
    pub fn rearm_recording_in_transaction(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        max_attempts: u32,
        run_at: DateTime<Utc>,
    ) -> Result<Option<i64>> {
        ensure!(
            !self.conn.is_autocommit(),
            "recording_rearm_requires_transaction"
        );
        self.enqueue_recording_in_transaction(kind, payload, max_attempts, run_at, true)
    }

    fn enqueue_recording_inner(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        max_attempts: u32,
        run_at: DateTime<Utc>,
        rearm_failed: bool,
    ) -> Result<Option<i64>> {
        let transaction = self
            .conn
            .unchecked_transaction()
            .context("starting recording enqueue")?;
        let inserted = Jobs::new(&transaction).enqueue_recording_in_transaction(
            kind,
            payload,
            max_attempts,
            run_at,
            rearm_failed,
        )?;
        transaction
            .commit()
            .context("persisting recording job origin")?;
        Ok(inserted)
    }

    fn enqueue_recording_in_transaction(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        max_attempts: u32,
        run_at: DateTime<Utc>,
        rearm_failed: bool,
    ) -> Result<Option<i64>> {
        settled_quality(payload)?;
        let recording_id = payload
            .get("id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .context("recording job requires an identity")?;
        let existing = {
            let mut statement = self.conn.prepare("SELECT id, payload, state FROM jobs WHERE kind = ?1 AND state IN ('pending','running','failed') ORDER BY CASE WHEN state = 'failed' THEN 1 ELSE 0 END, id DESC")?;
            let mut rows = statement.query(params![kind])?;
            let mut found = None;
            while let Some(row) = rows.next()? {
                let raw: String = row.get(1)?;
                let candidate: serde_json::Value = serde_json::from_str(&raw)
                    .map_err(|_| anyhow::anyhow!("invalid_recording_job_payload"))?;
                if candidate.get("id").and_then(serde_json::Value::as_str) == Some(recording_id) {
                    let state: String = row.get(2)?;
                    if found.is_some() {
                        ensure!(state == "failed", "conflicting_active_recording_jobs");
                    } else {
                        found = Some((row.get::<_, i64>(0)?, candidate, state));
                    }
                }
            }
            found
        };
        let inserted = if let Some((job_id, existing, state)) = existing {
            let mut merged = merge_recording_payload(&existing, payload)?;
            if rearm_failed {
                merged["first_batch"] = serde_json::Value::Bool(false);
            }
            if rearm_failed && state == "failed" {
                self.conn.execute(
                    "UPDATE jobs SET payload=?2,state='pending',attempts=0,max_attempts=?3,next_run_at=?4,last_error=NULL,updated_at=?5 WHERE id=?1",
                    params![job_id, merged.to_string(), max_attempts, fmt_utc(run_at), fmt_utc(Utc::now())],
                ).context("rearming recording retry")?;
            } else if merged != existing {
                self.conn.execute(
                    "UPDATE jobs SET payload = ?2 WHERE id = ?1",
                    params![job_id, merged.to_string()],
                )?;
            }
            None
        } else {
            let now = fmt_utc(Utc::now());
            let mut payload = payload.clone();
            if rearm_failed {
                payload["first_batch"] = serde_json::Value::Bool(false);
            }
            self.conn.execute(
                "INSERT INTO jobs (kind, payload, max_attempts, next_run_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![kind, payload.to_string(), max_attempts, fmt_utc(run_at), now],
            ).context("arming recording job")?;
            Some(self.conn.last_insert_rowid())
        };
        Ok(inserted)
    }

    /// Read the durable recording payload without changing its claim/backoff. Active ownership takes
    /// precedence over failed history; completed one-shot jobs retain their normal deletion behavior.
    pub fn recording_payload(
        &self,
        kind: &str,
        recording_id: &str,
    ) -> Result<Option<serde_json::Value>> {
        let mut statement = self.conn.prepare("SELECT payload FROM jobs WHERE kind = ?1 AND state IN ('pending','running','failed') ORDER BY CASE WHEN state = 'failed' THEN 1 ELSE 0 END, id DESC")?;
        let mut rows = statement.query(params![kind])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let payload: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|_| anyhow::anyhow!("invalid_recording_job_payload"))?;
            if payload.get("id").and_then(serde_json::Value::as_str) == Some(recording_id) {
                return Ok(Some(payload));
            }
        }
        Ok(None)
    }

    /// Ensure the per-kind periodic singleton exists, due **now** (so it also fires at startup), with
    /// the given period. Re-registering updates the period of an existing pending row; a currently
    /// `running` instance is left alone (it reschedules itself on completion).
    pub fn enqueue_periodic(&self, kind: &str, period: Duration) -> Result<()> {
        let now = fmt_utc(Utc::now());
        self.conn
            .execute(
                "INSERT OR IGNORE INTO jobs (kind, payload, max_attempts, next_run_at, period_secs, created_at, updated_at)
                 VALUES (?1, '{}', 1, ?2, ?3, ?2, ?2)",
                params![kind, now, period.as_secs() as i64],
            )
            .context("enqueueing periodic job")?;
        self.conn
            .execute(
                "UPDATE jobs SET next_run_at = ?2, period_secs = ?3, updated_at = ?2
                 WHERE kind = ?1 AND payload = '{}' AND state = 'pending'",
                params![kind, now, period.as_secs() as i64],
            )
            .context("re-arming periodic job")?;
        Ok(())
    }

    /// Claim the earliest due pending job (if any): marks it `running` and bumps `attempts`.
    pub fn claim_due(&self, now: DateTime<Utc>) -> Result<Option<ClaimedJob>> {
        let row = self
            .conn
            .query_row(
                "SELECT id, kind, payload, attempts, max_attempts, period_secs FROM jobs
                 WHERE state = 'pending' AND next_run_at <= ?1
                 ORDER BY next_run_at LIMIT 1",
                params![fmt_utc(now)],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, u32>(3)?,
                        r.get::<_, u32>(4)?,
                        // SQLite integers are i64; period is always small and non-negative.
                        r.get::<_, Option<i64>>(5)?.map(|s| s as u64),
                    ))
                },
            )
            .optional()
            .context("selecting due job")?;
        let Some((id, kind, payload, attempts, max_attempts, period_secs)) = row else {
            return Ok(None);
        };
        self.conn
            .execute(
                "UPDATE jobs SET state = 'running', attempts = attempts + 1, updated_at = ?2 WHERE id = ?1",
                params![id, fmt_utc(Utc::now())],
            )
            .context("claiming job")?;
        Ok(Some(ClaimedJob {
            id,
            kind,
            payload: serde_json::from_str(&payload)
                .with_context(|| format!("parsing payload of job {id}"))?,
            attempts: attempts + 1,
            max_attempts,
            period_secs,
        }))
    }

    /// Mark a claimed job done: one-shots are deleted; periodics re-arm at `now + period` with a
    /// fresh attempt budget.
    pub fn complete(&self, job: &ClaimedJob) -> Result<()> {
        match job.period_secs {
            None => {
                self.conn
                    .execute("DELETE FROM jobs WHERE id = ?1", params![job.id])
                    .context("deleting completed job")?;
            }
            Some(period) => {
                let next = Utc::now() + chrono::Duration::seconds(period as i64);
                self.conn
                    .execute(
                        "UPDATE jobs SET state = 'pending', attempts = 0, last_error = NULL,
                                         next_run_at = ?2, updated_at = ?3
                         WHERE id = ?1",
                        params![job.id, fmt_utc(next), fmt_utc(Utc::now())],
                    )
                    .context("re-arming completed periodic job")?;
            }
        }
        Ok(())
    }

    /// Record a failure. One-shots with attempts left reschedule on `backoff`, else park as `failed`.
    /// Periodics always re-arm at their period (never backoff, never exhaust).
    pub fn fail(&self, job: &ClaimedJob, error: &str, backoff: &Backoff) -> Result<FailOutcome> {
        let now = Utc::now();
        let reschedule = |next: DateTime<Utc>, attempts: Option<u32>| -> Result<()> {
            self.conn
                .execute(
                    "UPDATE jobs SET state = 'pending', attempts = COALESCE(?4, attempts),
                                     last_error = ?2, next_run_at = ?3, updated_at = ?5,
                                     payload = ?6
                     WHERE id = ?1",
                    params![
                        job.id,
                        error,
                        fmt_utc(next),
                        attempts,
                        fmt_utc(now),
                        job.payload.to_string()
                    ],
                )
                .context("rescheduling failed job")?;
            Ok(())
        };
        match job.period_secs {
            Some(period) => {
                let next = now + chrono::Duration::seconds(period as i64);
                reschedule(next, Some(0))?;
                Ok(FailOutcome::Rescheduled { next_run_at: next })
            }
            None if job.attempts < job.max_attempts => {
                let next = now
                    + chrono::Duration::from_std(backoff.delay(job.attempts))
                        .unwrap_or(chrono::Duration::MAX);
                reschedule(next, None)?;
                Ok(FailOutcome::Rescheduled { next_run_at: next })
            }
            None => {
                self.conn
                    .execute(
                        "UPDATE jobs SET state = 'failed', last_error = ?2, updated_at = ?3,
                                         payload = ?4 WHERE id = ?1",
                        params![job.id, error, fmt_utc(now), job.payload.to_string()],
                    )
                    .context("parking exhausted job")?;
                Ok(FailOutcome::Exhausted)
            }
        }
    }

    /// Settle recording work without letting a stale claim erase newly merged quality or ownership.
    pub fn fail_recording(
        &self,
        job: &ClaimedJob,
        error: &str,
        backoff: &Backoff,
    ) -> Result<FailOutcome> {
        let transaction = self.conn.unchecked_transaction()?;
        let merged = recording_claim(&transaction, job)?;
        let outcome = self.fail(&merged, error, backoff)?;
        transaction
            .commit()
            .context("persisting recording failure")?;
        Ok(outcome)
    }

    /// Requeue a claimed one-shot even after its nominal attempt budget. Used when the artifact's terminal
    /// state could not be persisted: parking the only active job first would violate durability.
    pub fn reschedule(
        &self,
        job: &ClaimedJob,
        error: &str,
        backoff: &Backoff,
    ) -> Result<DateTime<Utc>> {
        let now = Utc::now();
        let next = now
            + chrono::Duration::from_std(backoff.delay(job.attempts))
                .unwrap_or(chrono::Duration::MAX);
        self.conn
            .execute(
                "UPDATE jobs SET state = 'pending', last_error = ?2, next_run_at = ?3,
                                 updated_at = ?4, payload = ?5 WHERE id = ?1",
                params![
                    job.id,
                    error,
                    fmt_utc(next),
                    fmt_utc(now),
                    job.payload.to_string()
                ],
            )
            .context("rescheduling unsettled job")?;
        Ok(next)
    }

    /// Recording variant of [`Self::reschedule`], preserving evidence merged after the claim.
    pub fn reschedule_recording(
        &self,
        job: &ClaimedJob,
        error: &str,
        backoff: &Backoff,
    ) -> Result<DateTime<Utc>> {
        let transaction = self.conn.unchecked_transaction()?;
        let merged = recording_claim(&transaction, job)?;
        let next = self.reschedule(&merged, error, backoff)?;
        transaction
            .commit()
            .context("persisting recording reschedule")?;
        Ok(next)
    }

    /// Startup crash recovery: anything still `running` was orphaned by a crash — make it due now.
    /// (Its claim already bumped `attempts`, so the budget still depletes.) Returns how many.
    pub fn recover_running(&self) -> Result<usize> {
        let now = fmt_utc(Utc::now());
        self.conn
            .execute(
                "UPDATE jobs SET state = 'pending', next_run_at = ?1, updated_at = ?1
                 WHERE state = 'running'",
                params![now],
            )
            .context("recovering running jobs")
    }

    /// When the next pending job is due — the pipeline loop's wake-up bound.
    pub fn next_due_at(&self) -> Result<Option<DateTime<Utc>>> {
        let next: Option<String> = self
            .conn
            .query_row(
                "SELECT MIN(next_run_at) FROM jobs WHERE state = 'pending'",
                [],
                |r| r.get(0),
            )
            .context("reading next due time")?;
        next.as_deref().map(parse_utc).transpose()
    }

    /// Drop any parked `failed` row for this exact `(kind, payload)`. The dedupe index only covers
    /// active rows, so failed debris would otherwise sit next to a fresh enqueue of the same work —
    /// call this when something (e.g. a manual retry) supersedes the old failure. Returns how many.
    pub fn delete_failed(&self, kind: &str, payload: &serde_json::Value) -> Result<usize> {
        self.conn
            .execute(
                "DELETE FROM jobs WHERE kind = ?1 AND payload = ?2 AND state = 'failed'",
                params![kind, payload.to_string()],
            )
            .context("deleting failed job row")
    }

    /// GC `failed` rows older than `cutoff`. Returns how many were deleted.
    pub fn delete_terminal_older_than(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        self.conn
            .execute(
                "DELETE FROM jobs WHERE state = 'failed' AND updated_at < ?1",
                params![fmt_utc(cutoff)],
            )
            .context("deleting failed job rows")
    }

    /// Payload + attempts of every active (pending/running) job of `kind` — UI surface (e.g. "will
    /// retry, attempt 2/5").
    pub fn active_for(&self, kind: &str) -> Result<Vec<(serde_json::Value, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload, attempts FROM jobs WHERE kind = ?1 AND state IN ('pending','running')",
        )?;
        let rows = stmt
            .query_map(params![kind], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, u32>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("reading active jobs")?;
        rows.into_iter()
            .map(|(p, a)| Ok((serde_json::from_str(&p).context("parsing payload")?, a)))
            .collect()
    }
}

/// Fixed-width UTC `…Z` millisecond form — lexical order equals chronological order (same canonical
/// form as corti-queue's timestamps).
fn fmt_utc(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_utc(s: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)
        .with_context(|| format!("parsing timestamp {s:?}"))?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn jobs_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        Jobs::ensure_schema(&conn).unwrap();
        conn
    }

    const BACKOFF: Backoff = Backoff {
        base: Duration::from_secs(30),
        cap: Duration::from_secs(3600),
    };

    fn recording_quality(start: u64, source_loss: bool) -> serde_json::Value {
        json!({
            "schema_version": 1, "coverage": "observed",
            "epochs": [{"id": 0, "source_start_ns": 0, "source_rate_hz": 48000,
                "source_frame_base": 0, "retained_start_frame": 0, "retained_rate_hz": 48000,
                "mic_channels": 1, "tap_channels": 1, "reason": "start"}],
            "spans": [{"epoch": 0, "start_frame": start, "end_frame": start + 2,
                "track": "both", "cause": if source_loss { "ring_overflow" } else { "mic_tap_mismatch" },
                "class": if source_loss { "source_loss" } else { "processing_loss" },
                "stage": "live", "resolution": "outstanding"}],
            "overflow": {"omitted_spans": [0,0,0], "outstanding_omitted_spans": [0,0,0],
                "omitted_epochs": 0, "counter_saturated": false}
        })
    }

    #[test]
    fn recording_quality_merge_survives_duplicate_stale_claim_and_reopen_without_reset() {
        let path =
            std::env::temp_dir().join(format!("corti-job-quality-{}.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        Jobs::ensure_schema(&conn).unwrap();
        let jobs = Jobs::new(&conn);
        let now = Utc::now();
        let initial = json!({"id":"recording", "original_backend":"local", "first_batch":false,
            "capture_finished":true, "settled_quality": recording_quality(0, true)});
        jobs.enqueue_recording("retry", &initial, 3, now).unwrap();
        let mut claimed = jobs.claim_due(now).unwrap().unwrap();
        let state = || {
            conn.query_row(
                "SELECT state,attempts,max_attempts,next_run_at FROM jobs WHERE id=?1",
                params![claimed.id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, u32>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                },
            )
            .unwrap()
        };
        let before = state();
        let duplicate = json!({"id":"recording", "original_backend":"aws", "first_batch":true,
            "settled_quality": recording_quality(8, false)});
        for _ in 0..2 {
            assert_eq!(
                jobs.enqueue_recording("retry", &duplicate, 99, now + chrono::Duration::days(1))
                    .unwrap(),
                None
            );
        }
        assert_eq!(state(), before);
        let merged = jobs
            .recording_payload("retry", "recording")
            .unwrap()
            .unwrap();
        assert_eq!(
            merged["settled_quality"]["spans"].as_array().unwrap().len(),
            2
        );
        assert_eq!(merged["original_backend"], "local");
        assert_eq!(merged["first_batch"], false);
        // The app may add note ownership using an older claimed payload after a newer quality merge.
        claimed.payload = json!({"id":"recording", "preferred_note":"/synthetic/owned.md", "note_canonical":true});
        let FailOutcome::Rescheduled { next_run_at } = jobs
            .fail_recording(&claimed, "synthetic_retry", &BACKOFF)
            .unwrap()
        else {
            panic!("retry remains available")
        };
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        let jobs = Jobs::new(&conn);
        assert!(jobs.claim_due(now).unwrap().is_none());
        let reopened = jobs
            .claim_due(next_run_at + chrono::Duration::seconds(1))
            .unwrap()
            .unwrap();
        assert_eq!((reopened.attempts, reopened.max_attempts), (2, 3));
        assert_eq!(
            reopened.payload["settled_quality"],
            merged["settled_quality"]
        );
        assert_eq!(reopened.payload["original_backend"], "local");
        assert_eq!(reopened.payload["first_batch"], false);
        assert_eq!(reopened.payload["capture_finished"], true);
        assert_eq!(reopened.payload["preferred_note"], "/synthetic/owned.md");
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn malformed_recording_quality_fails_before_mutation_without_payload_diagnostics() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        let payload = json!({"id":"recording", "settled_quality": {
            "coverage":"SYNTHETIC-PRIVATE-QUALITY-SENTINEL"}});
        let error = jobs
            .enqueue_recording("retry", &payload, 5, Utc::now())
            .unwrap_err();
        assert!(!format!("{error:#} {error:?}").contains("SYNTHETIC-PRIVATE-QUALITY-SENTINEL"));
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        let valid = json!({"id":"recording", "settled_quality":recording_quality(0, false)});
        jobs.enqueue_recording("retry", &valid, 5, Utc::now())
            .unwrap();
        let prior = jobs
            .recording_payload("retry", "recording")
            .unwrap()
            .unwrap();
        let error = jobs
            .enqueue_recording("retry", &payload, 99, Utc::now())
            .unwrap_err();
        assert!(!format!("{error:#} {error:?}").contains("SYNTHETIC-PRIVATE-QUALITY-SENTINEL"));
        assert_eq!(
            jobs.recording_payload("retry", "recording")
                .unwrap()
                .unwrap(),
            prior
        );
        let mut conflict = valid.clone();
        conflict["settled_quality"]["epochs"][0]["mic_channels"] = json!(2);
        assert!(
            jobs.enqueue_recording("retry", &conflict, 99, Utc::now())
                .is_err()
        );
        assert_eq!(
            jobs.recording_payload("retry", "recording")
                .unwrap()
                .unwrap(),
            prior
        );
        conn.execute("UPDATE jobs SET payload=?1", params![payload.to_string()])
            .unwrap();
        let error = jobs
            .enqueue_recording("retry", &valid, 5, Utc::now())
            .unwrap_err();
        assert!(!format!("{error:#} {error:?}").contains("SYNTHETIC-PRIVATE-QUALITY-SENTINEL"));
    }

    #[test]
    fn failed_recording_quality_merge_and_manual_rearm_are_atomic_without_implicit_restart() {
        for original in [None, Some("local")] {
            let conn = jobs_conn();
            let jobs = Jobs::new(&conn);
            let now = Utc::now();
            let mut initial = json!({"id":"recording", "first_batch":false,
                "preferred_note":"/synthetic/owned.md", "note_canonical":true,
                "settled_quality":recording_quality(0,true)});
            if let Some(original) = original {
                initial["original_backend"] = json!(original);
            }
            assert!(
                jobs.rearm_recording_in_transaction("retry", &initial, 5, now)
                    .is_err(),
                "recording resets must share an explicit queue transaction"
            );
            jobs.enqueue_recording("retry", &initial, 1, now).unwrap();
            let claimed = jobs.claim_due(now).unwrap().unwrap();
            assert_eq!(
                jobs.fail_recording(&claimed, "synthetic_exhausted", &BACKOFF)
                    .unwrap(),
                FailOutcome::Exhausted
            );
            let state = || {
                conn.query_row(
                    "SELECT state,attempts,max_attempts,next_run_at FROM jobs WHERE id=?1",
                    params![claimed.id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, u32>(1)?,
                            r.get::<_, u32>(2)?,
                            r.get::<_, String>(3)?,
                        ))
                    },
                )
                .unwrap()
            };
            let parked = state();
            let duplicate = json!({"id":"recording","original_backend":"aws","first_batch":true,
                "settled_quality":recording_quality(8,false)});
            assert_eq!(
                jobs.enqueue_recording("retry", &duplicate, 99, now)
                    .unwrap(),
                None
            );
            assert_eq!(state(), parked);
            assert!(
                jobs.claim_due(now + chrono::Duration::days(1))
                    .unwrap()
                    .is_none()
            );
            let evidence = jobs
                .recording_payload("retry", "recording")
                .unwrap()
                .unwrap();
            assert_eq!(
                evidence["settled_quality"]["spans"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
            conn.execute_batch("CREATE TEMP TRIGGER reject_rearm BEFORE UPDATE OF state ON jobs WHEN NEW.state='pending' BEGIN SELECT RAISE(ABORT,'synthetic_rearm_fault'); END;").unwrap();
            assert!(
                jobs.rearm_failed_recording("retry", &duplicate, 5, now)
                    .is_err()
            );
            assert_eq!(state(), parked);
            assert_eq!(
                jobs.recording_payload("retry", "recording")
                    .unwrap()
                    .unwrap(),
                evidence
            );
            conn.execute_batch("DROP TRIGGER reject_rearm;").unwrap();
            jobs.rearm_failed_recording("retry", &duplicate, 5, now)
                .unwrap();
            let resumed = jobs.claim_due(now).unwrap().unwrap();
            assert_eq!(
                (resumed.id, resumed.attempts, resumed.max_attempts),
                (claimed.id, 1, 5)
            );
            assert_eq!(resumed.payload, evidence);
            assert_eq!(
                resumed
                    .payload
                    .get("original_backend")
                    .and_then(serde_json::Value::as_str),
                original
            );
            assert_eq!(resumed.payload["first_batch"], false);
            let running = state();
            jobs.rearm_failed_recording("retry", &duplicate, 88, now + chrono::Duration::days(1))
                .unwrap();
            assert_eq!(state(), running);
        }
    }

    #[test]
    fn recording_reschedule_merges_new_quality_and_rejects_stale_claims() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        let now = Utc::now();
        jobs.enqueue_recording(
            "retry",
            &json!({"id":"recording", "settled_quality":recording_quality(0,true)}),
            1,
            now,
        )
        .unwrap();
        let claimed = jobs.claim_due(now).unwrap().unwrap();
        jobs.enqueue_recording(
            "retry",
            &json!({"id":"recording", "settled_quality":recording_quality(8,false)}),
            5,
            now,
        )
        .unwrap();
        let next = jobs
            .reschedule_recording(&claimed, "synthetic_terminal_write_failed", &BACKOFF)
            .unwrap();
        let persisted = jobs
            .recording_payload("retry", "recording")
            .unwrap()
            .unwrap();
        assert_eq!(
            persisted["settled_quality"]["spans"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert!(jobs.fail_recording(&claimed, "stale", &BACKOFF).is_err());
        assert_eq!(
            jobs.recording_payload("retry", "recording")
                .unwrap()
                .unwrap(),
            persisted
        );
        let resumed = jobs
            .claim_due(next + chrono::Duration::seconds(1))
            .unwrap()
            .unwrap();
        assert_eq!((resumed.attempts, resumed.max_attempts), (2, 1));
        assert_eq!(resumed.payload, persisted);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let delays: Vec<u64> = (1..=5).map(|n| BACKOFF.delay(n).as_secs()).collect();
        assert_eq!(delays, vec![30, 60, 120, 240, 480]);
        assert_eq!(BACKOFF.delay(10).as_secs(), 3600); // capped
        assert_eq!(BACKOFF.delay(u32::MAX).as_secs(), 3600); // no overflow
    }

    #[test]
    fn enqueue_dedupes_active_then_allows_after_complete() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        let payload = json!({"id": "20260530-140500-zoom"});

        let first = jobs.enqueue("retry", &payload, 5, Utc::now()).unwrap();
        assert!(first.is_some());
        assert!(
            jobs.enqueue("retry", &payload, 5, Utc::now())
                .unwrap()
                .is_none()
        );
        // A different payload is a different job.
        assert!(
            jobs.enqueue("retry", &json!({"id": "other"}), 5, Utc::now())
                .unwrap()
                .is_some()
        );

        let claimed = jobs.claim_due(Utc::now()).unwrap().unwrap();
        // Still active while running.
        assert!(
            jobs.enqueue(&claimed.kind, &claimed.payload, 5, Utc::now())
                .unwrap()
                .is_none()
        );
        jobs.complete(&claimed).unwrap();
        assert!(
            jobs.enqueue(&claimed.kind, &claimed.payload, 5, Utc::now())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn recording_enqueue_preserves_origin_claim_budget_and_backoff() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        let now = Utc::now();
        let original = json!({"id":"recording","original_backend":"local","first_batch":true,"preferred_note":"partial.md","note_canonical":false});
        jobs.enqueue_recording("retry", &original, 6, now).unwrap();
        let changed = json!({"id":"recording","original_backend":"aws","first_batch":false,"preferred_note":"canonical.md","note_canonical":true});
        assert!(
            jobs.enqueue_recording("retry", &changed, 99, now)
                .unwrap()
                .is_none()
        );
        assert_eq!(jobs.active_for("retry").unwrap().len(), 1);
        let claimed = jobs
            .claim_due(now + chrono::Duration::seconds(1))
            .unwrap()
            .unwrap();
        assert_eq!(claimed.payload["original_backend"], "local");
        assert_eq!(claimed.payload["first_batch"], true);
        assert_eq!(claimed.payload["preferred_note"], "canonical.md");
        assert_eq!(claimed.attempts, 1);
        assert_eq!(claimed.max_attempts, 6);
        assert!(
            jobs.enqueue_recording("retry", &changed, 100, now)
                .unwrap()
                .is_none()
        );
        assert!(
            jobs.claim_due(now + chrono::Duration::hours(1))
                .unwrap()
                .is_none(),
            "claimed ownership must not reset"
        );
        let FailOutcome::Rescheduled { next_run_at } =
            jobs.fail(&claimed, "synthetic failure", &BACKOFF).unwrap()
        else {
            panic!("first attempt should back off")
        };
        jobs.enqueue_recording("retry", &changed, 101, now).unwrap();
        assert_eq!(
            jobs.next_due_at().unwrap(),
            Some(
                DateTime::parse_from_rfc3339(&fmt_utc(next_run_at))
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
        let retried = jobs
            .claim_due(next_run_at + chrono::Duration::seconds(1))
            .unwrap()
            .unwrap();
        assert_eq!(retried.id, claimed.id);
        assert_eq!(retried.attempts, 2);
        assert_eq!(retried.max_attempts, 6);
        assert_eq!(retried.payload["original_backend"], "local");
    }

    #[test]
    fn recording_enqueue_cannot_authorize_an_unknown_existing_origin() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        jobs.enqueue("retry", &json!({"id":"legacy"}), 5, Utc::now())
            .unwrap();
        jobs.enqueue_recording(
            "retry",
            &json!({"id":"legacy","original_backend":"aws","first_batch":true}),
            6,
            Utc::now(),
        )
        .unwrap();
        let payload = jobs.recording_payload("retry", "legacy").unwrap().unwrap();
        assert!(payload.get("original_backend").is_none());
        assert!(payload.get("first_batch").is_none());
        assert_eq!(jobs.active_for("retry").unwrap().len(), 1);
    }

    #[test]
    fn recording_origin_survives_reopening_before_and_after_claim() {
        let dir = std::env::temp_dir().join(format!("corti-jobs-origin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jobs.db");
        let conn = Connection::open(&path).unwrap();
        Jobs::ensure_schema(&conn).unwrap();
        Jobs::new(&conn)
            .enqueue_recording(
                "retry",
                &json!({"id":"recording","original_backend":"aws","first_batch":true}),
                6,
                Utc::now(),
            )
            .unwrap();
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        let first = Jobs::new(&conn)
            .claim_due(Utc::now() + chrono::Duration::seconds(1))
            .unwrap()
            .unwrap();
        assert_eq!(first.payload["original_backend"], "aws");
        assert_eq!(first.attempts, 1);
        drop(conn);
        let conn = Connection::open(&path).unwrap();
        let jobs = Jobs::new(&conn);
        assert_eq!(jobs.recover_running().unwrap(), 1);
        let resumed = jobs
            .claim_due(Utc::now() + chrono::Duration::seconds(1))
            .unwrap()
            .unwrap();
        assert_eq!(resumed.id, first.id);
        assert_eq!(resumed.attempts, 2);
        assert_eq!(resumed.max_attempts, 6);
        assert_eq!(resumed.payload["original_backend"], "aws");
        drop(conn);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recording_budgets_allow_exactly_first_plus_five_or_existing_five_attempts() {
        for budget in [5_u32, 6] {
            let conn = jobs_conn();
            let jobs = Jobs::new(&conn);
            let payload = json!({"id":"recording","original_backend":"local"});
            jobs.enqueue_recording("retry", &payload, budget, Utc::now())
                .unwrap();
            for expected in 1..=budget {
                let claimed = jobs
                    .claim_due(Utc::now() + chrono::Duration::days(1))
                    .unwrap()
                    .unwrap();
                assert_eq!(claimed.attempts, expected);
                jobs.enqueue_recording("retry", &payload, 99, Utc::now())
                    .unwrap();
                let outcome = jobs.fail(&claimed, "synthetic failure", &BACKOFF).unwrap();
                assert_eq!(
                    matches!(outcome, FailOutcome::Exhausted),
                    expected == budget
                );
            }
            assert!(
                jobs.claim_due(Utc::now() + chrono::Duration::days(2))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                jobs.recording_payload("retry", "recording")
                    .unwrap()
                    .unwrap()["original_backend"],
                "local"
            );
        }
    }

    #[test]
    fn claims_earliest_due_only() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        let now = Utc::now();
        jobs.enqueue(
            "a",
            &json!({"n": 1}),
            5,
            now - chrono::Duration::seconds(10),
        )
        .unwrap();
        jobs.enqueue(
            "b",
            &json!({"n": 2}),
            5,
            now - chrono::Duration::seconds(20),
        )
        .unwrap();
        jobs.enqueue("c", &json!({"n": 3}), 5, now + chrono::Duration::hours(1))
            .unwrap();

        assert_eq!(jobs.claim_due(now).unwrap().unwrap().kind, "b"); // oldest due first
        assert_eq!(jobs.claim_due(now).unwrap().unwrap().kind, "a");
        assert!(jobs.claim_due(now).unwrap().is_none()); // c not due yet
        assert_eq!(
            jobs.claim_due(now + chrono::Duration::hours(2))
                .unwrap()
                .unwrap()
                .kind,
            "c"
        );
    }

    #[test]
    fn one_shot_fails_with_backoff_then_exhausts() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        let now = Utc::now();
        jobs.enqueue("retry", &json!({"id": "x"}), 2, now).unwrap();

        let j = jobs.claim_due(now).unwrap().unwrap();
        assert_eq!(j.attempts, 1);
        let outcome = jobs.fail(&j, "boom", &BACKOFF).unwrap();
        let FailOutcome::Rescheduled { next_run_at } = outcome else {
            panic!("expected reschedule, got {outcome:?}");
        };
        assert!(next_run_at > now);
        assert!(jobs.claim_due(now).unwrap().is_none()); // backoff holds it

        let j = jobs
            .claim_due(next_run_at + chrono::Duration::seconds(1))
            .unwrap()
            .unwrap();
        assert_eq!(j.attempts, 2);
        assert_eq!(
            jobs.fail(&j, "boom again", &BACKOFF).unwrap(),
            FailOutcome::Exhausted
        );
        // Parked, not deleted; no longer claimable.
        assert!(
            jobs.claim_due(now + chrono::Duration::days(1))
                .unwrap()
                .is_none()
        );
        let state: String = conn
            .query_row("SELECT state FROM jobs WHERE id = ?1", params![j.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "failed");
    }

    #[test]
    fn failure_settlement_persists_replacement_payload_and_can_reschedule_exhaustion() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        jobs.enqueue("retry", &json!({"id": "x"}), 1, Utc::now())
            .unwrap();
        let mut claimed = jobs.claim_due(Utc::now()).unwrap().unwrap();
        claimed.payload = json!({"id": "x", "preferred_note": "/brain/note.md"});

        let next = jobs
            .reschedule(&claimed, "terminal write failed", &BACKOFF)
            .unwrap();
        let recovered = jobs
            .claim_due(next + chrono::Duration::seconds(1))
            .unwrap()
            .unwrap();
        assert_eq!(recovered.payload, claimed.payload);
        assert_eq!(
            jobs.fail(&recovered, "still broken", &BACKOFF).unwrap(),
            FailOutcome::Exhausted
        );
        let payload: String = conn
            .query_row(
                "SELECT payload FROM jobs WHERE id = ?1",
                params![recovered.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
            claimed.payload
        );
    }

    #[test]
    fn periodic_fires_at_startup_reschedules_on_complete_and_fail() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        let period = Duration::from_secs(3600);
        jobs.enqueue_periodic("sweep", period).unwrap();
        jobs.enqueue_periodic("sweep", period).unwrap(); // idempotent singleton
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Due immediately (startup fire).
        let j = jobs.claim_due(Utc::now()).unwrap().unwrap();
        assert_eq!(j.period_secs, Some(3600));
        jobs.complete(&j).unwrap();
        assert!(jobs.claim_due(Utc::now()).unwrap().is_none()); // re-armed an hour out
        let j = jobs
            .claim_due(Utc::now() + chrono::Duration::seconds(3601))
            .unwrap()
            .unwrap();
        assert_eq!(j.attempts, 1); // complete() reset the budget

        // Failure also re-arms at the period — periodics never exhaust.
        let outcome = jobs.fail(&j, "sweep broke", &BACKOFF).unwrap();
        assert!(matches!(outcome, FailOutcome::Rescheduled { .. }));
        let j = jobs
            .claim_due(Utc::now() + chrono::Duration::seconds(7202))
            .unwrap()
            .unwrap();
        assert_eq!(j.attempts, 1); // attempts reset on periodic fail too
    }

    #[test]
    fn recover_running_makes_orphans_due_and_preserves_attempts() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        jobs.enqueue("retry", &json!({"id": "x"}), 5, Utc::now())
            .unwrap();
        let j = jobs.claim_due(Utc::now()).unwrap().unwrap();
        assert_eq!(j.attempts, 1);
        // "Crash": the running row is orphaned. Recover → due now, attempts kept.
        assert_eq!(jobs.recover_running().unwrap(), 1);
        assert_eq!(jobs.recover_running().unwrap(), 0); // idempotent
        let j = jobs.claim_due(Utc::now()).unwrap().unwrap();
        assert_eq!(j.attempts, 2); // claim bumped again — crash-loops still exhaust
    }

    #[test]
    fn next_due_at_tracks_earliest_pending() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        assert!(jobs.next_due_at().unwrap().is_none());
        let near = Utc::now() + chrono::Duration::seconds(30);
        let far = Utc::now() + chrono::Duration::hours(2);
        jobs.enqueue("a", &json!({"n": 1}), 5, far).unwrap();
        jobs.enqueue("b", &json!({"n": 2}), 5, near).unwrap();
        let due = jobs.next_due_at().unwrap().unwrap();
        assert_eq!(due.timestamp(), near.timestamp());
    }

    #[test]
    fn gc_deletes_only_old_failed_rows() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        jobs.enqueue("retry", &json!({"id": "dead"}), 1, Utc::now())
            .unwrap();
        jobs.enqueue("retry", &json!({"id": "alive"}), 5, Utc::now())
            .unwrap();
        let j = jobs.claim_due(Utc::now()).unwrap().unwrap();
        assert_eq!(
            jobs.fail(&j, "x", &BACKOFF).unwrap(),
            FailOutcome::Exhausted
        );

        // Pending rows are never GC'd; the failed one goes once the cutoff passes it.
        assert_eq!(
            jobs.delete_terminal_older_than(Utc::now() - chrono::Duration::hours(1))
                .unwrap(),
            0
        );
        assert_eq!(
            jobs.delete_terminal_older_than(Utc::now() + chrono::Duration::hours(1))
                .unwrap(),
            1
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn delete_failed_clears_exact_match_only() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        jobs.enqueue("retry", &json!({"id": "x"}), 1, Utc::now())
            .unwrap();
        let j = jobs.claim_due(Utc::now()).unwrap().unwrap();
        assert_eq!(
            jobs.fail(&j, "x", &BACKOFF).unwrap(),
            FailOutcome::Exhausted
        );

        assert_eq!(
            jobs.delete_failed("retry", &json!({"id": "other"}))
                .unwrap(),
            0
        );
        assert_eq!(jobs.delete_failed("retry", &json!({"id": "x"})).unwrap(), 1);
        // Slate is clean: the same work can be enqueued fresh.
        assert!(
            jobs.enqueue("retry", &json!({"id": "x"}), 1, Utc::now())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn active_for_reports_payload_and_attempts() {
        let conn = jobs_conn();
        let jobs = Jobs::new(&conn);
        jobs.enqueue("retry", &json!({"id": "x"}), 5, Utc::now())
            .unwrap();
        let j = jobs.claim_due(Utc::now()).unwrap().unwrap();
        jobs.fail(&j, "transient", &BACKOFF).unwrap();

        let active = jobs.active_for("retry").unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0["id"], "x");
        assert_eq!(active[0].1, 1);
        assert!(jobs.active_for("sweep").unwrap().is_empty());
    }
}
