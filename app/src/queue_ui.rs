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
use corti_postprocess::BillingBasis;
use corti_queue::{PostprocessCallRecord, Queue};
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
    /// Additive hosted projection; ordinary `status` remains the downgrade authority.
    pub postprocess_state: Option<String>,
    pub postprocess_updated_at: Option<String>,
    pub note_path: Option<String>,
    /// Whether the note still exists at `note_path` — `false` after vagus reorganizes it out of the
    /// inbox, which the UI shows as "Filed in brain" (not an error).
    pub note_exists: bool,
    pub audio_exists: bool,
    /// Raw audio or a validated post-ASR checkpoint can drive a manual retry.
    pub recovery_exists: bool,
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
            let recovery_exists = audio_meta.is_some()
                || crate::checkpoint::FilingCheckpoint::load(&job.audio_path).is_ok();
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
                postprocess_state: job.postprocess_state.and_then(enum_wire),
                postprocess_updated_at: job.postprocess_updated_at.map(|time| time.to_rfc3339()),
                note_path: job.note_path.map(|p| p.to_string_lossy().into_owned()),
                note_exists,
                audio_exists: audio_meta.is_some(),
                recovery_exists,
                audio_bytes: audio_meta.map(|m| m.len()),
                retry_pending: retry_attempts.is_some(),
                retry_attempts,
                id: job.id,
            }
        })
        .collect())
}

#[derive(Debug, Clone, Serialize)]
pub struct PostprocessHistoryCallDto {
    pub call_id: String,
    pub lane: String,
    pub provider: String,
    pub transport: String,
    pub support_tier: String,
    pub model: String,
    pub outcome: String,
    pub error_code: Option<String>,
    pub cache_source: String,
    pub provider_request_sent: bool,
    pub usage_complete: bool,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cached_read_tokens: Option<u64>,
    pub cached_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub cost_label: String,
    pub queued_at: String,
    pub dispatched_at: Option<String>,
    pub completed_at: Option<String>,
    pub total_us: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PostprocessHistoryDto {
    pub known_estimate_micros: u64,
    pub known_call_count: u64,
    pub included_call_count: u64,
    pub no_provider_call_count: u64,
    pub unknown_call_count: u64,
    pub calls: Vec<PostprocessHistoryCallDto>,
}

/// Content-free hosted call history. This opens the same read-only queue handle as the recording list and
/// cannot expose prompts/transcripts/questions/answers because those columns and DTO fields do not exist.
#[tauri::command]
pub fn get_recording_postprocess_history(
    id: String,
    hosted: State<'_, crate::postprocess_app::HostedState>,
) -> Result<PostprocessHistoryDto, String> {
    let queue = Queue::open_read_only().map_err(|e| format!("{e:#}"))?;
    let calls = queue
        .postprocess_history(&id)
        .map_err(|e| format!("{e:#}"))?;
    let mut history = history_dto(calls);
    if !hosted.handle().snapshot().show_history_diagnostics {
        history.calls.clear();
    }
    Ok(history)
}

fn history_dto(calls: Vec<PostprocessCallRecord>) -> PostprocessHistoryDto {
    let mut known_estimate_micros = 0u64;
    let mut known_call_count = 0u64;
    let mut included_call_count = 0u64;
    let mut no_provider_call_count = 0u64;
    let mut unknown_call_count = 0u64;
    let calls = calls
        .into_iter()
        .map(|call| {
            match call.cost.billing_basis() {
                BillingBasis::MeteredEstimate => {
                    if let Some(value) = call.cost.cost_micros() {
                        known_estimate_micros = known_estimate_micros.saturating_add(value);
                        known_call_count = known_call_count.saturating_add(1);
                    } else {
                        unknown_call_count = unknown_call_count.saturating_add(1);
                    }
                }
                BillingBasis::IncludedSubscription => {
                    included_call_count = included_call_count.saturating_add(1)
                }
                BillingBasis::NoProviderRequest => {
                    no_provider_call_count = no_provider_call_count.saturating_add(1)
                }
                BillingBasis::Unknown => unknown_call_count = unknown_call_count.saturating_add(1),
            }
            PostprocessHistoryCallDto {
                call_id: call.call_id.as_str().to_owned(),
                lane: enum_wire(call.lane).unwrap_or_default(),
                provider: call.provider_id.as_str().to_owned(),
                transport: call.transport_id.as_str().to_owned(),
                support_tier: enum_wire(call.support_tier).unwrap_or_default(),
                model: call.model_id.as_str().to_owned(),
                outcome: enum_wire(call.outcome).unwrap_or_default(),
                error_code: call.error_code.and_then(enum_wire),
                cache_source: enum_wire(call.cache_source).unwrap_or_default(),
                provider_request_sent: call.provider_request_sent,
                usage_complete: call.usage.usage_complete,
                input_tokens: call.usage.input_tokens,
                output_tokens: call.usage.output_tokens,
                cached_read_tokens: call.usage.cached_read_tokens,
                cached_write_tokens: call.usage.cached_write_tokens,
                reasoning_tokens: call.usage.reasoning_tokens,
                cost_label: queue_cost_label(&call),
                queued_at: call.queued_at.to_rfc3339(),
                dispatched_at: call.dispatched_at.map(|time| time.to_rfc3339()),
                completed_at: call.completed_at.map(|time| time.to_rfc3339()),
                total_us: call.latency.total_us,
            }
        })
        .collect();
    PostprocessHistoryDto {
        known_estimate_micros,
        known_call_count,
        included_call_count,
        no_provider_call_count,
        unknown_call_count,
        calls,
    }
}

fn queue_cost_label(call: &PostprocessCallRecord) -> String {
    match call.cost.billing_basis() {
        BillingBasis::IncludedSubscription => {
            "Included subscription · cost unavailable".to_string()
        }
        BillingBasis::NoProviderRequest => "Local cache · no provider request".to_string(),
        BillingBasis::Unknown => "Cost unavailable".to_string(),
        BillingBasis::MeteredEstimate => match (call.cost.currency(), call.cost.cost_micros()) {
            (Some(currency), Some(micros)) if currency.as_str() == "USD" => {
                format!("Estimated ${}", format_micros(micros))
            }
            (Some(currency), Some(micros)) => {
                format!("Estimated {} {}", currency.as_str(), format_micros(micros))
            }
            _ => "Cost unavailable".to_string(),
        },
    }
}

fn format_micros(micros: u64) -> String {
    let whole = micros / 1_000_000;
    let fractional = micros % 1_000_000;
    let mut value = format!("{whole}.{fractional:06}");
    while value.ends_with('0') && value.len() - value.find('.').unwrap_or(value.len()) - 1 > 4 {
        value.pop();
    }
    value
}

fn enum_wire<T: Serialize>(value: T) -> Option<String> {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use corti_postprocess::{
        CacheObservation, CallId, CurrencyCode, Lane, LatencyFields, ModelId, NormalizedUsage,
        ProviderId, RequestGroupId, SupportTier, TransportId,
    };
    use corti_queue::{PostprocessCacheSource, PostprocessCost, PostprocessOutcome};

    use super::*;

    fn call(id: &str, cost: PostprocessCost) -> PostprocessCallRecord {
        let now = Utc::now();
        PostprocessCallRecord {
            call_id: CallId::new(id).unwrap(),
            recording_id: "fixture-recording".into(),
            request_group_id: RequestGroupId::new(format!("group-{id}")).unwrap(),
            target_id: None,
            lane: Lane::Final,
            attempt_no: 1,
            provider_id: ProviderId::new("fixture-provider").unwrap(),
            transport_id: TransportId::new("fixture-transport").unwrap(),
            support_tier: SupportTier::Documented,
            model_id: ModelId::new("fixture-model").unwrap(),
            adapter_version: 1,
            prompt_version: 1,
            output_schema_version: 1,
            session_generation: 1,
            transcript_revision: 2,
            control_revision: 3,
            steering_revision: 4,
            bank_revision: 5,
            question_revision: None,
            outcome: PostprocessOutcome::Completed,
            error_code: None,
            cache_source: PostprocessCacheSource::Network,
            provider_request_sent: true,
            usage: NormalizedUsage::unknown(),
            cost,
            queued_at: now,
            dispatched_at: Some(now),
            completed_at: Some(now),
            latency: LatencyFields::default(),
            created_at: now,
        }
    }

    #[test]
    fn history_cost_warning_labels_are_exact_and_null_is_never_zero() {
        let now = Utc::now();
        let report = history_dto(vec![
            call(
                "metered",
                PostprocessCost::metered_estimate(
                    18_400,
                    CurrencyCode::usd(),
                    "fixture-catalog",
                    "fixture-tariff",
                    now,
                ),
            ),
            call("included", PostprocessCost::included_subscription()),
            call("local", PostprocessCost::no_provider_request()),
            call("unknown", PostprocessCost::unknown()),
        ]);
        assert_eq!(report.known_estimate_micros, 18_400);
        assert_eq!(report.known_call_count, 1);
        assert_eq!(report.included_call_count, 1);
        assert_eq!(report.no_provider_call_count, 1);
        assert_eq!(report.unknown_call_count, 1);
        let labels: Vec<&str> = report
            .calls
            .iter()
            .map(|call| call.cost_label.as_str())
            .collect();
        assert_eq!(labels[0], "Estimated $0.0184");
        assert_eq!(labels[1], "Included subscription · cost unavailable");
        assert_eq!(labels[2], "Local cache · no provider request");
        assert_eq!(labels[3], "Cost unavailable");
        assert!(labels.iter().all(|label| !label.contains("$0.00")));

        let serialized = serde_json::to_string(&report).unwrap();
        for forbidden in [
            "fixture transcript text",
            "fixture question",
            "fixture answer",
            "fixture-secret-value",
            "bearer ",
        ] {
            assert!(!serialized.to_ascii_lowercase().contains(forbidden));
        }
        let _ = CacheObservation::None; // keep the domain allowlist visible in this integration test.
    }
}
