//! A deliberately small durable background-job layer: a `jobs` table living in corti's queue.db,
//! drained by the pipeline worker between (and after) its channel messages.
//!
//! Shape: kinds are strings, payloads are JSON — this crate knows nothing about corti's job semantics;
//! the app defines kind constants and handlers. One-shot jobs retry with exponential backoff up to
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

use anyhow::{Context, Result};
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
                                     last_error = ?2, next_run_at = ?3, updated_at = ?5
                     WHERE id = ?1",
                    params![job.id, error, fmt_utc(next), attempts, fmt_utc(now)],
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
                        "UPDATE jobs SET state = 'failed', last_error = ?2, updated_at = ?3 WHERE id = ?1",
                        params![job.id, error, fmt_utc(now)],
                    )
                    .context("parking exhausted job")?;
                Ok(FailOutcome::Exhausted)
            }
        }
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
