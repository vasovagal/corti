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
use corti_postprocess::{
    BillingBasis, CallId, CurrencyCode, ErrorCode, Lane, LatencyFields, ModelId, NormalizedUsage,
    ProviderId, RequestGroupId, SupportTier, TargetId, TransportId,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};

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
    /// Additive hosted projection; the existing `status` remains the downgrade authority.
    pub postprocess_state: Option<PostprocessState>,
    pub postprocess_updated_at: Option<DateTime<Local>>,
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

/// Small downgrade-safe projection beside the existing recording status. These values never enter the
/// `JobStatus` column, so an older Corti binary can continue to parse every recording row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostprocessState {
    AwaitingAuth,
    Dispatching,
    Finalizing,
    Fallback,
    Complete,
}

/// Sanitized terminal disposition for a hosted call. There is intentionally no free-form error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostprocessOutcome {
    Applied,
    Completed,
    Canceled,
    Failed,
    Superseded,
    Timeout,
    Fallback,
    Ambiguous,
}

/// Where an accepted terminal result came from. Failed/pre-dispatch calls use `none`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostprocessCacheSource {
    None,
    Local,
    Provider,
    Network,
    Mixed,
}

/// Nullable truthful cost metadata. Non-metered and unknown variants cannot carry a dollar amount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostprocessCost {
    billing_basis: BillingBasis,
    cost_micros: Option<u64>,
    currency: Option<CurrencyCode>,
    pricing_catalog_version: Option<String>,
    tariff_id: Option<String>,
    tariff_effective_at: Option<DateTime<Utc>>,
}

impl PostprocessCost {
    pub fn unknown() -> Self {
        Self::without_cost(BillingBasis::Unknown)
    }

    pub fn included_subscription() -> Self {
        Self::without_cost(BillingBasis::IncludedSubscription)
    }

    pub fn no_provider_request() -> Self {
        Self::without_cost(BillingBasis::NoProviderRequest)
    }

    pub fn metered_estimate(
        cost_micros: u64,
        currency: CurrencyCode,
        pricing_catalog_version: impl Into<String>,
        tariff_id: impl Into<String>,
        tariff_effective_at: DateTime<Utc>,
    ) -> Self {
        Self {
            billing_basis: BillingBasis::MeteredEstimate,
            cost_micros: Some(cost_micros),
            currency: Some(currency),
            pricing_catalog_version: Some(pricing_catalog_version.into()),
            tariff_id: Some(tariff_id.into()),
            tariff_effective_at: Some(tariff_effective_at),
        }
    }

    fn without_cost(billing_basis: BillingBasis) -> Self {
        Self {
            billing_basis,
            cost_micros: None,
            currency: None,
            pricing_catalog_version: None,
            tariff_id: None,
            tariff_effective_at: None,
        }
    }

    pub const fn billing_basis(&self) -> BillingBasis {
        self.billing_basis
    }

    pub const fn cost_micros(&self) -> Option<u64> {
        self.cost_micros
    }

    pub fn currency(&self) -> Option<&CurrencyCode> {
        self.currency.as_ref()
    }

    pub fn pricing_catalog_version(&self) -> Option<&str> {
        self.pricing_catalog_version.as_deref()
    }

    pub fn tariff_id(&self) -> Option<&str> {
        self.tariff_id.as_deref()
    }

    pub fn tariff_effective_at(&self) -> Option<&DateTime<Utc>> {
        self.tariff_effective_at.as_ref()
    }

    fn validate(&self) -> Result<()> {
        match self.billing_basis {
            BillingBasis::MeteredEstimate => ensure_cost_fields(self, true),
            BillingBasis::IncludedSubscription
            | BillingBasis::NoProviderRequest
            | BillingBasis::Unknown => ensure_cost_fields(self, false),
        }
    }
}

fn ensure_cost_fields(cost: &PostprocessCost, expected: bool) -> Result<()> {
    let present = [
        cost.cost_micros.is_some(),
        cost.currency.is_some(),
        cost.pricing_catalog_version.is_some(),
        cost.tariff_id.is_some(),
        cost.tariff_effective_at.is_some(),
    ];
    if present.into_iter().all(|actual| actual == expected) {
        Ok(())
    } else if expected {
        bail!("metered postprocess cost is missing tariff provenance")
    } else {
        bail!("unknown/subscription/local postprocess cost must remain null")
    }
}

/// One content-free terminal provider-call record. The type has no prompt, transcript, replacement, diff,
/// steering text, word-bank entry, question, answer, credential, account/project id, response body, or
/// free-form provider-error field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostprocessCallRecord {
    pub call_id: CallId,
    pub recording_id: String,
    pub request_group_id: RequestGroupId,
    pub target_id: Option<TargetId>,
    pub lane: Lane,
    pub attempt_no: u64,
    pub provider_id: ProviderId,
    pub transport_id: TransportId,
    pub support_tier: SupportTier,
    pub model_id: ModelId,
    pub adapter_version: u32,
    pub prompt_version: u32,
    pub output_schema_version: u32,
    pub session_generation: u64,
    pub transcript_revision: u64,
    pub control_revision: u64,
    pub steering_revision: u64,
    pub bank_revision: u64,
    pub question_revision: Option<u64>,
    pub outcome: PostprocessOutcome,
    pub error_code: Option<ErrorCode>,
    pub cache_source: PostprocessCacheSource,
    pub provider_request_sent: bool,
    pub usage: NormalizedUsage,
    pub cost: PostprocessCost,
    pub queued_at: DateTime<Utc>,
    pub dispatched_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub latency: LatencyFields,
    pub created_at: DateTime<Utc>,
}

impl PostprocessCallRecord {
    fn validate(&self) -> Result<()> {
        if self.recording_id.is_empty()
            || self.recording_id.len() > 512
            || self.recording_id.chars().any(char::is_control)
        {
            bail!("invalid postprocess recording id");
        }
        if self.attempt_no == 0 {
            bail!("postprocess attempt number must be positive");
        }
        self.cost.validate()?;
        Ok(())
    }
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
        conn.pragma_update(None, "foreign_keys", true)
            .context("enabling foreign keys")?;
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
        conn.pragma_update(None, "foreign_keys", true)
            .context("enabling foreign keys")?;
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

    /// Atomically persist the filed note path and transition the recording to `Done` in one SQL update.
    /// A failure leaves both fields unchanged, so callers must not report success or delete recovery inputs.
    pub fn complete_with_note(&self, id: &str, note_path: &Path) -> Result<()> {
        let done = status_to_string(JobStatus::Done);
        let note = note_path.to_string_lossy().into_owned();
        let n = self
            .conn
            .execute(
                "UPDATE recordings
                 SET note_path = ?2, status = ?3, error = NULL, updated_at = ?4
                 WHERE id = ?1",
                params![id, note, done, fmt_dt(Local::now())],
            )
            .context("completing recording with note")?;
        if n == 0 {
            bail!("no recording with id {id}");
        }
        tracing::debug!(
            target: "corti::queue",
            job_id = %id,
            note_path = %note_path.display(),
            "complete_with_note"
        );
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

    /// Reset a `Failed` recording for a manual retry, clearing its error. `PendingNote` is permitted when
    /// a durable filing checkpoint survived; all other retries restart at `PendingTranscription`.
    pub fn retry_reset_to(&self, id: &str, status: JobStatus) -> Result<()> {
        if !matches!(
            status,
            JobStatus::PendingTranscription | JobStatus::PendingNote
        ) {
            bail!("manual retry cannot reset to {status:?}");
        }
        let n = self
            .conn
            .execute(
                "UPDATE recordings SET status = ?2, error = NULL, updated_at = ?3
                 WHERE id = ?1 AND status = ?4",
                params![
                    id,
                    status_to_string(status),
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

    /// Backward-compatible transcription retry reset.
    pub fn retry_reset(&self, id: &str) -> Result<()> {
        self.retry_reset_to(id, JobStatus::PendingTranscription)
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

    /// Set or clear the downgrade-safe hosted projection without changing the existing pipeline status.
    pub fn set_postprocess_state(
        &self,
        recording_id: &str,
        state: Option<PostprocessState>,
    ) -> Result<()> {
        let state = state.map(enum_token).transpose()?;
        let updated_at = state.as_ref().map(|_| fmt_utc(Utc::now()));
        let changed = self
            .conn
            .execute(
                "UPDATE recordings
                 SET postprocess_state = ?2, postprocess_updated_at = ?3
                 WHERE id = ?1",
                params![recording_id, state, updated_at],
            )
            .context("updating postprocess recording projection")?;
        if changed == 0 {
            bail!("no recording with id {recording_id}");
        }
        Ok(())
    }

    /// Insert or idempotently refresh one terminal content-free call row. The pipeline owner is the only
    /// production caller; read-only Queue handles fail at SQLite if this method is accidentally invoked.
    pub fn upsert_postprocess_call(&self, call: &PostprocessCallRecord) -> Result<()> {
        call.validate()?;
        let attempt_no = sqlite_u64(call.attempt_no, "attempt_no")?;
        let session_generation = sqlite_u64(call.session_generation, "session_generation")?;
        let transcript_revision = sqlite_u64(call.transcript_revision, "transcript_revision")?;
        let control_revision = sqlite_u64(call.control_revision, "control_revision")?;
        let steering_revision = sqlite_u64(call.steering_revision, "steering_revision")?;
        let bank_revision = sqlite_u64(call.bank_revision, "bank_revision")?;
        let question_revision = sqlite_optional_u64(call.question_revision, "question_revision")?;
        let usage = call.usage;
        let input_tokens = sqlite_optional_u64(usage.input_tokens, "input_tokens")?;
        let output_tokens = sqlite_optional_u64(usage.output_tokens, "output_tokens")?;
        let cached_read_tokens =
            sqlite_optional_u64(usage.cached_read_tokens, "cached_read_tokens")?;
        let cached_write_tokens =
            sqlite_optional_u64(usage.cached_write_tokens, "cached_write_tokens")?;
        let reasoning_tokens = sqlite_optional_u64(usage.reasoning_tokens, "reasoning_tokens")?;
        let cost_micros = sqlite_optional_u64(call.cost.cost_micros, "cost_micros")?;
        let latency = call.latency;
        let queue_us = sqlite_optional_u64(latency.queue_us, "queue_us")?;
        let auth_us = sqlite_optional_u64(latency.auth_us, "auth_us")?;
        let cache_lookup_us = sqlite_optional_u64(latency.cache_lookup_us, "cache_lookup_us")?;
        let connect_us = sqlite_optional_u64(latency.connect_us, "connect_us")?;
        let ttfb_us = sqlite_optional_u64(latency.ttfb_us, "ttfb_us")?;
        let ttft_us = sqlite_optional_u64(latency.ttft_us, "ttft_us")?;
        let stream_us = sqlite_optional_u64(latency.stream_us, "stream_us")?;
        let parse_us = sqlite_optional_u64(latency.parse_us, "parse_us")?;
        let cache_commit_us = sqlite_optional_u64(latency.cache_commit_us, "cache_commit_us")?;
        let total_us = sqlite_optional_u64(latency.total_us, "total_us")?;

        self.conn
            .execute(
                UPSERT_POSTPROCESS_CALL,
                params![
                    call.call_id.as_str(),
                    call.recording_id,
                    call.request_group_id.as_str(),
                    call.target_id.as_ref().map(TargetId::as_str),
                    enum_token(call.lane)?,
                    attempt_no,
                    call.provider_id.as_str(),
                    call.transport_id.as_str(),
                    enum_token(call.support_tier)?,
                    call.model_id.as_str(),
                    i64::from(call.adapter_version),
                    i64::from(call.prompt_version),
                    i64::from(call.output_schema_version),
                    session_generation,
                    transcript_revision,
                    control_revision,
                    steering_revision,
                    bank_revision,
                    question_revision,
                    enum_token(call.outcome)?,
                    call.error_code.map(enum_token).transpose()?,
                    enum_token(call.cache_source)?,
                    call.provider_request_sent,
                    usage.usage_complete,
                    input_tokens,
                    output_tokens,
                    cached_read_tokens,
                    cached_write_tokens,
                    reasoning_tokens,
                    cost_micros,
                    call.cost.currency.as_ref().map(CurrencyCode::as_str),
                    enum_token(call.cost.billing_basis)?,
                    call.cost.pricing_catalog_version.as_deref(),
                    call.cost.tariff_id.as_deref(),
                    call.cost.tariff_effective_at.map(fmt_utc),
                    fmt_utc(call.queued_at),
                    call.dispatched_at.map(fmt_utc),
                    call.completed_at.map(fmt_utc),
                    queue_us,
                    auth_us,
                    cache_lookup_us,
                    connect_us,
                    ttfb_us,
                    ttft_us,
                    stream_us,
                    parse_us,
                    cache_commit_us,
                    total_us,
                    fmt_utc(call.created_at),
                ],
            )
            .context("upserting content-free postprocess call")?;
        Ok(())
    }

    /// Content-free call history for one recording, in deterministic schedule/call-id order.
    pub fn postprocess_history(&self, recording_id: &str) -> Result<Vec<PostprocessCallRecord>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {POSTPROCESS_CALL_COLS} FROM postprocess_calls
             WHERE recording_id = ?1 ORDER BY queued_at, call_id"
        ))?;
        let rows = statement
            .query_map(params![recording_id], read_postprocess_call)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(raw_to_postprocess_call).collect()
    }

    /// Fetch one content-free call by its globally unique id.
    pub fn postprocess_call(&self, call_id: &CallId) -> Result<Option<PostprocessCallRecord>> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {POSTPROCESS_CALL_COLS} FROM postprocess_calls WHERE call_id = ?1"
        ))?;
        let raw = statement
            .query_row(params![call_id.as_str()], read_postprocess_call)
            .optional()?;
        raw.map(raw_to_postprocess_call).transpose()
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

    /// Delete one terminal (`Done` | `Failed`) row after its caller has confirmed that every associated
    /// artifact is absent. Non-terminal rows are never touched. Returns whether a row was deleted.
    pub fn delete_terminal(&self, id: &str) -> Result<bool> {
        let deleted = self
            .conn
            .execute(
                "DELETE FROM recordings WHERE id = ?1 AND status IN (?2, ?3)",
                params![
                    id,
                    status_to_string(JobStatus::Done),
                    status_to_string(JobStatus::Failed),
                ],
            )
            .context("deleting terminal recording")?;
        Ok(deleted > 0)
    }

    /// Row GC: delete terminal (`Done` | `Failed`) rows last updated before `older_than`. Bounds
    /// queue.db and the Recording Queue's history list on a much longer horizon than the audio
    /// retention (90 days vs ~7), so "Filed in brain" history stays useful for a quarter. Returns how
    /// many rows went; non-terminal rows are never touched.
    ///
    /// Retention code that owns artifacts should prefer [`Queue::delete_terminal`] after confirming those
    /// paths are absent. This bulk helper remains available to queue-only callers with no artifact contract.
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
                    transcribe_secs, postprocess_state, postprocess_updated_at";

/// Bumped whenever [`migrate`] gains a step; stamped into `PRAGMA user_version` on every open.
const SCHEMA_VERSION: i64 = 2;

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
  error                   TEXT,
  updated_at              TEXT NOT NULL,
  transcribe_secs         REAL,
  postprocess_state       TEXT,
  postprocess_updated_at  TEXT
);
CREATE INDEX IF NOT EXISTS idx_recordings_status ON recordings(status);

CREATE TABLE IF NOT EXISTS postprocess_calls(
  call_id                  TEXT PRIMARY KEY,
  recording_id             TEXT NOT NULL REFERENCES recordings(id) ON DELETE CASCADE,
  request_group_id         TEXT NOT NULL,
  target_id                TEXT,
  lane                     TEXT NOT NULL,
  attempt_no               INTEGER NOT NULL,
  provider_id              TEXT NOT NULL,
  transport_id             TEXT NOT NULL,
  support_tier             TEXT NOT NULL,
  model_id                 TEXT NOT NULL,
  adapter_version          INTEGER NOT NULL,
  prompt_version           INTEGER NOT NULL,
  output_schema_version    INTEGER NOT NULL,
  session_generation       INTEGER NOT NULL,
  transcript_revision      INTEGER NOT NULL,
  control_revision         INTEGER NOT NULL,
  steering_revision        INTEGER NOT NULL,
  bank_revision            INTEGER NOT NULL,
  question_revision        INTEGER,
  outcome                  TEXT NOT NULL,
  error_code               TEXT,
  cache_source             TEXT NOT NULL,
  provider_request_sent    INTEGER NOT NULL,
  usage_complete           INTEGER NOT NULL,
  input_tokens             INTEGER,
  output_tokens            INTEGER,
  cached_read_tokens       INTEGER,
  cached_write_tokens      INTEGER,
  reasoning_tokens         INTEGER,
  cost_micros              INTEGER,
  currency                 TEXT,
  billing_basis            TEXT NOT NULL,
  pricing_catalog_version  TEXT,
  tariff_id                TEXT,
  tariff_effective_at      TEXT,
  queued_at                TEXT NOT NULL,
  dispatched_at            TEXT,
  completed_at             TEXT,
  queue_us                 INTEGER,
  auth_us                  INTEGER,
  cache_lookup_us          INTEGER,
  connect_us               INTEGER,
  ttfb_us                  INTEGER,
  ttft_us                  INTEGER,
  stream_us                INTEGER,
  parse_us                 INTEGER,
  cache_commit_us          INTEGER,
  total_us                 INTEGER,
  created_at               TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_postprocess_recording_lane_time
  ON postprocess_calls(recording_id, lane, queued_at);
CREATE INDEX IF NOT EXISTS idx_postprocess_time ON postprocess_calls(queued_at);
";

/// Bring a pre-existing DB up to [`SCHEMA_VERSION`]. Runs before `execute_batch(SCHEMA)`, so a fresh DB
/// (no `recordings` table yet) skips straight through and is created in the current shape. Every step is
/// safe to re-run: a crash between a step committing and the `user_version` stamp just replays it.
fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .context("reading user_version")?;
    if version > SCHEMA_VERSION {
        bail!("queue schema version {version} is newer than supported version {SCHEMA_VERSION}");
    }
    if version == SCHEMA_VERSION {
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
    if version < 2 && has_table {
        migrate_v1_to_v2(conn).context("migrating queue schema v1 → v2")?;
    }
    Ok(())
}

/// v1 → v2 is wholly additive. Probe before each `ALTER TABLE`, create call history/indexes, and stamp the
/// version in one transaction. The probes make replay safe for a fixture interrupted by an older migration
/// implementation even though ordinary SQLite transaction rollback is already atomic.
fn migrate_v1_to_v2(conn: &mut Connection) -> Result<()> {
    let tx = conn
        .transaction()
        .context("starting v1 → v2 migration tx")?;
    for (column, declaration) in [
        ("postprocess_state", "TEXT"),
        ("postprocess_updated_at", "TEXT"),
    ] {
        let exists: bool = tx
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM pragma_table_info('recordings') WHERE name = ?1
                 )",
                params![column],
                |row| row.get(0),
            )
            .with_context(|| format!("probing for recordings.{column}"))?;
        if !exists {
            tx.execute(
                &format!("ALTER TABLE recordings ADD COLUMN {column} {declaration}"),
                [],
            )
            .with_context(|| format!("adding recordings.{column}"))?;
        }
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS postprocess_calls(
           call_id                  TEXT PRIMARY KEY,
           recording_id             TEXT NOT NULL REFERENCES recordings(id) ON DELETE CASCADE,
           request_group_id         TEXT NOT NULL,
           target_id                TEXT,
           lane                     TEXT NOT NULL,
           attempt_no               INTEGER NOT NULL,
           provider_id              TEXT NOT NULL,
           transport_id             TEXT NOT NULL,
           support_tier             TEXT NOT NULL,
           model_id                 TEXT NOT NULL,
           adapter_version          INTEGER NOT NULL,
           prompt_version           INTEGER NOT NULL,
           output_schema_version    INTEGER NOT NULL,
           session_generation       INTEGER NOT NULL,
           transcript_revision      INTEGER NOT NULL,
           control_revision         INTEGER NOT NULL,
           steering_revision        INTEGER NOT NULL,
           bank_revision            INTEGER NOT NULL,
           question_revision        INTEGER,
           outcome                  TEXT NOT NULL,
           error_code               TEXT,
           cache_source             TEXT NOT NULL,
           provider_request_sent    INTEGER NOT NULL,
           usage_complete           INTEGER NOT NULL,
           input_tokens             INTEGER,
           output_tokens            INTEGER,
           cached_read_tokens       INTEGER,
           cached_write_tokens      INTEGER,
           reasoning_tokens         INTEGER,
           cost_micros              INTEGER,
           currency                 TEXT,
           billing_basis            TEXT NOT NULL,
           pricing_catalog_version  TEXT,
           tariff_id                TEXT,
           tariff_effective_at      TEXT,
           queued_at                TEXT NOT NULL,
           dispatched_at            TEXT,
           completed_at             TEXT,
           queue_us                 INTEGER,
           auth_us                  INTEGER,
           cache_lookup_us          INTEGER,
           connect_us               INTEGER,
           ttfb_us                  INTEGER,
           ttft_us                  INTEGER,
           stream_us                INTEGER,
           parse_us                 INTEGER,
           cache_commit_us          INTEGER,
           total_us                 INTEGER,
           created_at               TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_postprocess_recording_lane_time
           ON postprocess_calls(recording_id, lane, queued_at);
         CREATE INDEX IF NOT EXISTS idx_postprocess_time ON postprocess_calls(queued_at);
         PRAGMA user_version = 2;",
    )
    .context("creating v2 postprocess call history")?;
    tx.commit().context("committing v1 → v2 migration")
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
    postprocess_state: Option<String>,
    postprocess_updated_at: Option<String>,
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
        postprocess_state: row.get(13)?,
        postprocess_updated_at: row.get(14)?,
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
        postprocess_state: r
            .postprocess_state
            .as_deref()
            .map(enum_from_token)
            .transpose()?,
        postprocess_updated_at: r
            .postprocess_updated_at
            .as_deref()
            .map(parse_dt)
            .transpose()?,
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

const POSTPROCESS_CALL_COLS: &str = "call_id, recording_id, request_group_id, target_id, lane, \
    attempt_no, provider_id, transport_id, support_tier, model_id, adapter_version, prompt_version, \
    output_schema_version, session_generation, transcript_revision, control_revision, steering_revision, \
    bank_revision, question_revision, outcome, error_code, cache_source, provider_request_sent, \
    usage_complete, input_tokens, output_tokens, cached_read_tokens, cached_write_tokens, reasoning_tokens, \
    cost_micros, currency, billing_basis, pricing_catalog_version, tariff_id, tariff_effective_at, queued_at, \
    dispatched_at, completed_at, queue_us, auth_us, cache_lookup_us, connect_us, ttfb_us, ttft_us, stream_us, \
    parse_us, cache_commit_us, total_us, created_at";

const UPSERT_POSTPROCESS_CALL: &str = "
INSERT INTO postprocess_calls(
  call_id, recording_id, request_group_id, target_id, lane, attempt_no, provider_id, transport_id,
  support_tier, model_id, adapter_version, prompt_version, output_schema_version, session_generation,
  transcript_revision, control_revision, steering_revision, bank_revision, question_revision, outcome,
  error_code, cache_source, provider_request_sent, usage_complete, input_tokens, output_tokens,
  cached_read_tokens, cached_write_tokens, reasoning_tokens, cost_micros, currency, billing_basis,
  pricing_catalog_version, tariff_id, tariff_effective_at, queued_at, dispatched_at, completed_at, queue_us,
  auth_us, cache_lookup_us, connect_us, ttfb_us, ttft_us, stream_us, parse_us, cache_commit_us, total_us,
  created_at
) VALUES (
  ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
  ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37,
  ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46, ?47, ?48, ?49
)
ON CONFLICT(call_id) DO UPDATE SET
  recording_id = excluded.recording_id,
  request_group_id = excluded.request_group_id,
  target_id = excluded.target_id,
  lane = excluded.lane,
  attempt_no = excluded.attempt_no,
  provider_id = excluded.provider_id,
  transport_id = excluded.transport_id,
  support_tier = excluded.support_tier,
  model_id = excluded.model_id,
  adapter_version = excluded.adapter_version,
  prompt_version = excluded.prompt_version,
  output_schema_version = excluded.output_schema_version,
  session_generation = excluded.session_generation,
  transcript_revision = excluded.transcript_revision,
  control_revision = excluded.control_revision,
  steering_revision = excluded.steering_revision,
  bank_revision = excluded.bank_revision,
  question_revision = excluded.question_revision,
  outcome = excluded.outcome,
  error_code = excluded.error_code,
  cache_source = excluded.cache_source,
  provider_request_sent = excluded.provider_request_sent,
  usage_complete = excluded.usage_complete,
  input_tokens = excluded.input_tokens,
  output_tokens = excluded.output_tokens,
  cached_read_tokens = excluded.cached_read_tokens,
  cached_write_tokens = excluded.cached_write_tokens,
  reasoning_tokens = excluded.reasoning_tokens,
  cost_micros = excluded.cost_micros,
  currency = excluded.currency,
  billing_basis = excluded.billing_basis,
  pricing_catalog_version = excluded.pricing_catalog_version,
  tariff_id = excluded.tariff_id,
  tariff_effective_at = excluded.tariff_effective_at,
  queued_at = excluded.queued_at,
  dispatched_at = excluded.dispatched_at,
  completed_at = excluded.completed_at,
  queue_us = excluded.queue_us,
  auth_us = excluded.auth_us,
  cache_lookup_us = excluded.cache_lookup_us,
  connect_us = excluded.connect_us,
  ttfb_us = excluded.ttfb_us,
  ttft_us = excluded.ttft_us,
  stream_us = excluded.stream_us,
  parse_us = excluded.parse_us,
  cache_commit_us = excluded.cache_commit_us,
  total_us = excluded.total_us,
  created_at = excluded.created_at
";

struct RawPostprocessCall {
    call_id: String,
    recording_id: String,
    request_group_id: String,
    target_id: Option<String>,
    lane: String,
    attempt_no: i64,
    provider_id: String,
    transport_id: String,
    support_tier: String,
    model_id: String,
    adapter_version: i64,
    prompt_version: i64,
    output_schema_version: i64,
    session_generation: i64,
    transcript_revision: i64,
    control_revision: i64,
    steering_revision: i64,
    bank_revision: i64,
    question_revision: Option<i64>,
    outcome: String,
    error_code: Option<String>,
    cache_source: String,
    provider_request_sent: i64,
    usage_complete: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_read_tokens: Option<i64>,
    cached_write_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    cost_micros: Option<i64>,
    currency: Option<String>,
    billing_basis: String,
    pricing_catalog_version: Option<String>,
    tariff_id: Option<String>,
    tariff_effective_at: Option<String>,
    queued_at: String,
    dispatched_at: Option<String>,
    completed_at: Option<String>,
    queue_us: Option<i64>,
    auth_us: Option<i64>,
    cache_lookup_us: Option<i64>,
    connect_us: Option<i64>,
    ttfb_us: Option<i64>,
    ttft_us: Option<i64>,
    stream_us: Option<i64>,
    parse_us: Option<i64>,
    cache_commit_us: Option<i64>,
    total_us: Option<i64>,
    created_at: String,
}

fn read_postprocess_call(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawPostprocessCall> {
    Ok(RawPostprocessCall {
        call_id: row.get(0)?,
        recording_id: row.get(1)?,
        request_group_id: row.get(2)?,
        target_id: row.get(3)?,
        lane: row.get(4)?,
        attempt_no: row.get(5)?,
        provider_id: row.get(6)?,
        transport_id: row.get(7)?,
        support_tier: row.get(8)?,
        model_id: row.get(9)?,
        adapter_version: row.get(10)?,
        prompt_version: row.get(11)?,
        output_schema_version: row.get(12)?,
        session_generation: row.get(13)?,
        transcript_revision: row.get(14)?,
        control_revision: row.get(15)?,
        steering_revision: row.get(16)?,
        bank_revision: row.get(17)?,
        question_revision: row.get(18)?,
        outcome: row.get(19)?,
        error_code: row.get(20)?,
        cache_source: row.get(21)?,
        provider_request_sent: row.get(22)?,
        usage_complete: row.get(23)?,
        input_tokens: row.get(24)?,
        output_tokens: row.get(25)?,
        cached_read_tokens: row.get(26)?,
        cached_write_tokens: row.get(27)?,
        reasoning_tokens: row.get(28)?,
        cost_micros: row.get(29)?,
        currency: row.get(30)?,
        billing_basis: row.get(31)?,
        pricing_catalog_version: row.get(32)?,
        tariff_id: row.get(33)?,
        tariff_effective_at: row.get(34)?,
        queued_at: row.get(35)?,
        dispatched_at: row.get(36)?,
        completed_at: row.get(37)?,
        queue_us: row.get(38)?,
        auth_us: row.get(39)?,
        cache_lookup_us: row.get(40)?,
        connect_us: row.get(41)?,
        ttfb_us: row.get(42)?,
        ttft_us: row.get(43)?,
        stream_us: row.get(44)?,
        parse_us: row.get(45)?,
        cache_commit_us: row.get(46)?,
        total_us: row.get(47)?,
        created_at: row.get(48)?,
    })
}

fn raw_to_postprocess_call(raw: RawPostprocessCall) -> Result<PostprocessCallRecord> {
    let billing_basis = enum_from_token(&raw.billing_basis)?;
    let cost = PostprocessCost {
        billing_basis,
        cost_micros: raw_optional_u64(raw.cost_micros, "cost_micros")?,
        currency: raw
            .currency
            .map(CurrencyCode::new)
            .transpose()
            .context("reading postprocess currency")?,
        pricing_catalog_version: raw.pricing_catalog_version,
        tariff_id: raw.tariff_id,
        tariff_effective_at: raw
            .tariff_effective_at
            .as_deref()
            .map(parse_utc)
            .transpose()?,
    };
    let call = PostprocessCallRecord {
        call_id: CallId::new(raw.call_id).context("reading postprocess call id")?,
        recording_id: raw.recording_id,
        request_group_id: RequestGroupId::new(raw.request_group_id)
            .context("reading postprocess request group id")?,
        target_id: raw
            .target_id
            .map(TargetId::new)
            .transpose()
            .context("reading postprocess target id")?,
        lane: enum_from_token(&raw.lane)?,
        attempt_no: raw_u64(raw.attempt_no, "attempt_no")?,
        provider_id: ProviderId::new(raw.provider_id).context("reading postprocess provider id")?,
        transport_id: TransportId::new(raw.transport_id)
            .context("reading postprocess transport id")?,
        support_tier: enum_from_token(&raw.support_tier)?,
        model_id: ModelId::new(raw.model_id).context("reading postprocess model id")?,
        adapter_version: raw_u32(raw.adapter_version, "adapter_version")?,
        prompt_version: raw_u32(raw.prompt_version, "prompt_version")?,
        output_schema_version: raw_u32(raw.output_schema_version, "output_schema_version")?,
        session_generation: raw_u64(raw.session_generation, "session_generation")?,
        transcript_revision: raw_u64(raw.transcript_revision, "transcript_revision")?,
        control_revision: raw_u64(raw.control_revision, "control_revision")?,
        steering_revision: raw_u64(raw.steering_revision, "steering_revision")?,
        bank_revision: raw_u64(raw.bank_revision, "bank_revision")?,
        question_revision: raw_optional_u64(raw.question_revision, "question_revision")?,
        outcome: enum_from_token(&raw.outcome)?,
        error_code: raw.error_code.as_deref().map(enum_from_token).transpose()?,
        cache_source: enum_from_token(&raw.cache_source)?,
        provider_request_sent: raw_bool(raw.provider_request_sent, "provider_request_sent")?,
        usage: NormalizedUsage {
            input_tokens: raw_optional_u64(raw.input_tokens, "input_tokens")?,
            output_tokens: raw_optional_u64(raw.output_tokens, "output_tokens")?,
            cached_read_tokens: raw_optional_u64(raw.cached_read_tokens, "cached_read_tokens")?,
            cached_write_tokens: raw_optional_u64(raw.cached_write_tokens, "cached_write_tokens")?,
            reasoning_tokens: raw_optional_u64(raw.reasoning_tokens, "reasoning_tokens")?,
            usage_complete: raw_bool(raw.usage_complete, "usage_complete")?,
        },
        cost,
        queued_at: parse_utc(&raw.queued_at)?,
        dispatched_at: raw.dispatched_at.as_deref().map(parse_utc).transpose()?,
        completed_at: raw.completed_at.as_deref().map(parse_utc).transpose()?,
        latency: LatencyFields {
            queue_us: raw_optional_u64(raw.queue_us, "queue_us")?,
            auth_us: raw_optional_u64(raw.auth_us, "auth_us")?,
            cache_lookup_us: raw_optional_u64(raw.cache_lookup_us, "cache_lookup_us")?,
            connect_us: raw_optional_u64(raw.connect_us, "connect_us")?,
            ttfb_us: raw_optional_u64(raw.ttfb_us, "ttfb_us")?,
            ttft_us: raw_optional_u64(raw.ttft_us, "ttft_us")?,
            stream_us: raw_optional_u64(raw.stream_us, "stream_us")?,
            parse_us: raw_optional_u64(raw.parse_us, "parse_us")?,
            cache_commit_us: raw_optional_u64(raw.cache_commit_us, "cache_commit_us")?,
            total_us: raw_optional_u64(raw.total_us, "total_us")?,
        },
        created_at: parse_utc(&raw.created_at)?,
    };
    call.validate()?;
    Ok(call)
}

fn enum_token<T: Serialize>(value: T) -> Result<String> {
    match serde_json::to_value(value).context("serializing content-free enum token")? {
        serde_json::Value::String(token) => Ok(token),
        _ => bail!("content-free enum did not serialize as a string"),
    }
}

fn enum_from_token<T: DeserializeOwned>(token: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(token.to_owned()))
        .context("unrecognized content-free enum token")
}

fn sqlite_u64(value: u64, label: &str) -> Result<i64> {
    i64::try_from(value).with_context(|| format!("{label} exceeds SQLite INTEGER range"))
}

fn sqlite_optional_u64(value: Option<u64>, label: &str) -> Result<Option<i64>> {
    value.map(|value| sqlite_u64(value, label)).transpose()
}

fn raw_u64(value: i64, label: &str) -> Result<u64> {
    u64::try_from(value).with_context(|| format!("{label} is negative"))
}

fn raw_optional_u64(value: Option<i64>, label: &str) -> Result<Option<u64>> {
    value.map(|value| raw_u64(value, label)).transpose()
}

fn raw_u32(value: i64, label: &str) -> Result<u32> {
    u32::try_from(value).with_context(|| format!("{label} is outside the u32 range"))
}

fn raw_bool(value: i64, label: &str) -> Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => bail!("{label} is not a SQLite boolean"),
    }
}

fn fmt_utc(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_utc(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .context("parsing postprocess UTC timestamp")?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    impl Queue {
        /// In-memory DB for tests (no filesystem, no path collisions).
        fn memory() -> Self {
            let conn = Connection::open_in_memory().unwrap();
            conn.pragma_update(None, "foreign_keys", true).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            corti_jobs::Jobs::ensure_schema(&conn).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)
                .unwrap();
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

    fn content_free_call(recording_id: &str) -> PostprocessCallRecord {
        let queued_at = Utc.with_ymd_and_hms(2026, 8, 21, 12, 0, 0).unwrap();
        PostprocessCallRecord {
            call_id: CallId::new("call-fixture-1").unwrap(),
            recording_id: recording_id.into(),
            request_group_id: RequestGroupId::new("group-fixture-1").unwrap(),
            target_id: Some(TargetId::new("target-fixture-1").unwrap()),
            lane: Lane::Final,
            attempt_no: 1,
            provider_id: ProviderId::new("fixture-provider").unwrap(),
            transport_id: TransportId::new("fixture-transport").unwrap(),
            support_tier: SupportTier::Documented,
            model_id: ModelId::new("fixture-model").unwrap(),
            adapter_version: 1,
            prompt_version: 1,
            output_schema_version: 1,
            session_generation: 2,
            transcript_revision: 3,
            control_revision: 4,
            steering_revision: 5,
            bank_revision: 6,
            question_revision: None,
            outcome: PostprocessOutcome::Failed,
            error_code: Some(ErrorCode::Timeout),
            cache_source: PostprocessCacheSource::None,
            provider_request_sent: true,
            usage: NormalizedUsage::unknown(),
            cost: PostprocessCost::unknown(),
            queued_at,
            dispatched_at: Some(queued_at + chrono::Duration::milliseconds(10)),
            completed_at: Some(queued_at + chrono::Duration::milliseconds(250)),
            latency: LatencyFields {
                queue_us: Some(10_000),
                auth_us: None,
                cache_lookup_us: Some(500),
                connect_us: Some(20_000),
                ttfb_us: Some(30_000),
                ttft_us: None,
                stream_us: None,
                parse_us: None,
                cache_commit_us: None,
                total_us: Some(250_000),
            },
            created_at: queued_at + chrono::Duration::milliseconds(250),
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
    fn complete_with_note_is_one_atomic_transition() {
        let q = Queue::memory();
        let id = q.enqueue(&meta("/cache/a.wav", "us.zoom.xos")).unwrap();
        q.set_status(&id, JobStatus::PendingNote).unwrap();

        // Force the one completion statement to abort. Neither note_path nor status may leak through.
        q.conn
            .execute_batch(
                "CREATE TRIGGER reject_done BEFORE UPDATE ON recordings
                 WHEN NEW.status = 'done'
                 BEGIN SELECT RAISE(ABORT, 'injected completion failure'); END;",
            )
            .unwrap();
        assert!(
            q.complete_with_note(&id, Path::new("/vault/note.md"))
                .is_err()
        );
        let unchanged = q.get(&id).unwrap().unwrap();
        assert_eq!(unchanged.status, JobStatus::PendingNote);
        assert_eq!(unchanged.note_path, None);

        // The live pipeline records a just-returned note path before this completion statement. If Done is
        // the write that fails, that recovery reference survives for the batch rewrite.
        q.update(
            &id,
            JobUpdate {
                note_path: Some(PathBuf::from("/vault/note.md")),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            q.complete_with_note(&id, Path::new("/vault/note.md"))
                .is_err()
        );
        let recoverable = q.get(&id).unwrap().unwrap();
        assert_eq!(recoverable.status, JobStatus::PendingNote);
        assert_eq!(recoverable.note_path, Some(PathBuf::from("/vault/note.md")));

        q.conn.execute_batch("DROP TRIGGER reject_done").unwrap();
        q.complete_with_note(&id, Path::new("/vault/note.md"))
            .unwrap();
        let completed = q.get(&id).unwrap().unwrap();
        assert_eq!(completed.status, JobStatus::Done);
        assert_eq!(completed.note_path, Some(PathBuf::from("/vault/note.md")));
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
    fn delete_terminal_removes_only_terminal_rows() {
        let q = Queue::memory();
        let done = q.enqueue(&meta("/cache/done.ogg", "us.zoom.xos")).unwrap();
        let pending = q
            .enqueue(&meta("/cache/pending.ogg", "us.zoom.xos"))
            .unwrap();
        q.set_status(&done, JobStatus::Done).unwrap();

        assert!(q.delete_terminal(&done).unwrap());
        assert!(q.get(&done).unwrap().is_none());
        assert!(!q.delete_terminal(&pending).unwrap());
        assert!(q.get(&pending).unwrap().is_some());
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

    #[test]
    fn content_free_history_round_trips_nullable_unknown_cost_and_latency() {
        let queue = Queue::memory();
        let recording_id = queue
            .enqueue(&meta("/cache/hosted-fixture.wav", "us.zoom.xos"))
            .unwrap();
        let call = content_free_call(&recording_id);
        queue.upsert_postprocess_call(&call).unwrap();

        let stored = queue.postprocess_call(&call.call_id).unwrap().unwrap();
        assert_eq!(stored, call);
        assert_eq!(
            queue.postprocess_history(&recording_id).unwrap(),
            vec![call.clone()]
        );
        assert_eq!(stored.cost.billing_basis(), BillingBasis::Unknown);
        assert_eq!(stored.cost.cost_micros(), None);
        assert_eq!(stored.usage, NormalizedUsage::unknown());
        assert_eq!(stored.latency.ttft_us, None);
        assert_eq!(stored.latency.total_us, Some(250_000));

        let sql_values: (Option<i64>, Option<String>, String) = queue
            .conn
            .query_row(
                "SELECT cost_micros, currency, billing_basis
                 FROM postprocess_calls WHERE call_id = ?1",
                params![stored.call_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(sql_values, (None, None, "unknown".into()));

        // Outbox replay/import is idempotent: one call id is refreshed, never duplicated.
        let mut refreshed = call;
        refreshed.outcome = PostprocessOutcome::Canceled;
        refreshed.error_code = Some(ErrorCode::Canceled);
        queue.upsert_postprocess_call(&refreshed).unwrap();
        assert_eq!(
            queue.postprocess_history(&recording_id).unwrap(),
            vec![refreshed]
        );
    }

    #[test]
    fn postprocess_call_schema_is_an_exact_content_free_allowlist() {
        let queue = Queue::memory();
        let columns = {
            let mut statement = queue
                .conn
                .prepare("SELECT name FROM pragma_table_info('postprocess_calls') ORDER BY cid")
                .unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let expected = POSTPROCESS_CALL_COLS
            .split(',')
            .map(str::trim)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(columns, expected);
        for forbidden in [
            "transcript",
            "prompt",
            "replacement",
            "diff",
            "steering",
            "word_bank_entry",
            "question",
            "answer",
            "credential",
            "account_id",
            "project_id",
            "provider_body",
            "error_body",
            "error_message",
        ] {
            assert!(
                !columns.iter().any(|column| column == forbidden),
                "content column {forbidden} must not exist"
            );
        }
        // The only similarly named fields are numeric revisions/versions required for fencing.
        assert!(columns.contains(&"transcript_revision".into()));
        assert!(columns.contains(&"prompt_version".into()));
        assert!(columns.contains(&"steering_revision".into()));
        assert!(columns.contains(&"question_revision".into()));
    }

    #[test]
    fn call_history_cascades_with_recording_and_projection_preserves_job_status() {
        let queue = Queue::memory();
        let recording_id = queue
            .enqueue(&meta("/cache/cascade-fixture.wav", "us.zoom.xos"))
            .unwrap();
        let call = content_free_call(&recording_id);
        queue.upsert_postprocess_call(&call).unwrap();
        let old_status = queue.get(&recording_id).unwrap().unwrap().status;

        queue
            .set_postprocess_state(&recording_id, Some(PostprocessState::Finalizing))
            .unwrap();
        let projected = queue.get(&recording_id).unwrap().unwrap();
        assert_eq!(projected.status, old_status);
        assert_eq!(
            projected.postprocess_state,
            Some(PostprocessState::Finalizing)
        );
        assert!(projected.postprocess_updated_at.is_some());
        queue.set_postprocess_state(&recording_id, None).unwrap();
        let cleared = queue.get(&recording_id).unwrap().unwrap();
        assert_eq!(cleared.postprocess_state, None);
        assert_eq!(cleared.postprocess_updated_at, None);

        queue.set_status(&recording_id, JobStatus::Done).unwrap();
        assert!(queue.delete_terminal(&recording_id).unwrap());
        assert!(queue.postprocess_history(&recording_id).unwrap().is_empty());
    }

    /// The v1 schema verbatim (schema version 1, before hosted projection/history). Keep frozen so the
    /// migration test does not accidentally begin from the current schema.
    const V1_SCHEMA: &str = "
CREATE TABLE recordings(
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
CREATE INDEX idx_recordings_status ON recordings(status);
PRAGMA user_version = 1;
";

    #[test]
    fn migrates_v1_to_v2_transactionally_and_is_reopen_safe() {
        let dir = std::env::temp_dir().join(format!(
            "corti-queue-test-migrate-v1-v2-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.db");
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(V1_SCHEMA).unwrap();
            connection
                .execute(
                    "INSERT INTO recordings(
                       id, started_at, owning_app, audio_path, status, updated_at, transcribe_secs
                     ) VALUES (
                       'v1-fixture', '2026-08-21T12:00:00.000Z', 'Fixture App',
                       '/cache/v1-fixture.wav', 'done', '2026-08-21T12:01:00.000Z', NULL
                     )",
                    [],
                )
                .unwrap();
            // Simulate a prior rerunnable implementation that added only the first column before stamp.
            connection
                .execute(
                    "ALTER TABLE recordings ADD COLUMN postprocess_state TEXT",
                    [],
                )
                .unwrap();
        }

        let queue = Queue::open_at(&path).unwrap();
        let version: i64 = queue
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let migrated = queue.get("v1-fixture").unwrap().unwrap();
        assert_eq!(migrated.postprocess_state, None);
        assert_eq!(migrated.postprocess_updated_at, None);
        let call_table: bool = queue
            .conn
            .query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'postprocess_calls'
                 )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(call_table);
        drop(queue);

        let reopened = Queue::open_at(&path).unwrap();
        assert_eq!(
            reopened.get("v1-fixture").unwrap().unwrap().status,
            JobStatus::Done
        );
        assert_eq!(
            reopened
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        std::fs::remove_dir_all(&dir).ok();
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
    fn retry_reset_requires_failed_clears_error_and_accepts_filing_state() {
        let q = Queue::memory();
        let id = q.enqueue(&meta("/cache/a.wav", "us.zoom.xos")).unwrap();
        // Not Failed yet ⇒ refuses (the UI can race the pipeline).
        assert!(q.retry_reset(&id).is_err());

        q.fail(&id, "boom").unwrap();
        q.retry_reset_to(&id, JobStatus::PendingNote).unwrap();
        let job = q.get(&id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::PendingNote);
        assert_eq!(job.error, None);

        q.fail(&id, "again").unwrap();
        assert!(q.retry_reset_to(&id, JobStatus::Done).is_err());
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
