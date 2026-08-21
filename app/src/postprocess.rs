//! Managed hosted post-processing control and coordination.
//!
//! This module is deliberately an app-owned boundary rather than production wiring. It owns monotonic
//! request fences, lane scheduling, cancellation, exact-cache ordering, Vertex catch-up, and the encrypted
//! store journal protocol. Provider execution, credential resolution, the encrypted SQLite implementation,
//! and call-site integration are injected. In particular, no type in this module discovers ambient
//! credentials or opens a network connection.
//!
//! Content-bearing request/result types intentionally implement neither `Serialize` nor content-revealing
//! `Debug`. DTOs that can cross the Tauri/event boundary contain only fixed state, typed identifiers,
//! normalized accounting, and sanitized error codes.

// Production call sites land in a later integration slice. Keeping this module compiled now lets its pure,
// injected state machines be tested without weakening the capture/ASR hot paths.
#![allow(dead_code)]

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{
        Arc,
        mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel},
    },
};

use corti_postprocess::{
    CacheObservation, CachePolicy, CallId, CancellationReason, CancellationToken, CostEstimate,
    CredentialState, ErrorCode, EventContext, HostedRequest, Lane, LatencyFields, LocalCacheMode,
    ModelCatalog, ModelDescriptor, ModelId, MonotonicDeadline, NormalizedUsage, PostprocessError,
    PricingCatalog, PricingQuery, ProcessEpoch, ProviderAdapter, ProviderCacheMode,
    ProviderDescriptor, ProviderEvent, ProviderEventKind, ProviderEventSink, ProviderId,
    ProviderOutput, ProviderScope, RequestFence, RequestKey, RewriteValidationLimits, SupportTier,
    TranscriptRow, TransportId, ValidatedQuestion, ValidatedRewrite, parse_and_validate_question,
    parse_and_validate_rewrite,
};
use corti_postprocess_providers::{
    CODEX_APP_SERVER_COMPILED, Clock as ProviderClock, VertexAutoPending, VertexCredentialResolver,
    VertexCredentialState, VertexDispatchDisposition, VertexResolutionAttempt,
    VertexResolutionOutcome, VertexResolverError,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation;

pub(crate) const COORDINATOR_COMMAND_CAPACITY: usize = 256;
pub(crate) const PROVIDER_EVENT_CAPACITY: usize = 256;
pub(crate) const MAX_GLOBAL_PROVIDER_CALLS: usize = 4;
pub(crate) const MAX_PROVIDER_CALLS: usize = 2;
pub(crate) const MAX_FINAL_CALLS: usize = 2;
pub(crate) const MAX_QUEUED_AD_HOC_QUESTIONS: usize = 8;
pub(crate) const MAX_VISIBLE_AD_HOC_EXCHANGES: usize = 20;
pub(crate) const MAX_VISIBLE_ASSISTANT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_QUESTION_TEXT_BYTES: usize = 32 * 1024;
pub(crate) const LIVE_DEBOUNCE_MICROS: u64 = 150_000;
pub(crate) const LIVE_FIRST_TEXT_DEADLINE_MICROS: u64 = 2_000_000;
pub(crate) const LIVE_TERMINAL_DEADLINE_MICROS: u64 = 5_000_000;
pub(crate) const QUESTION_DEADLINE_MICROS: u64 = 30_000_000;
pub(crate) const FINAL_PROMOTION_MICROS: u64 = 2_000_000;
pub(crate) const PINNED_QUIET_DEBOUNCE_MICROS: u64 = 750_000;
pub(crate) const PINNED_WORD_THRESHOLD: u64 = 40;
pub(crate) const PINNED_SPEECH_THRESHOLD_MS: u64 = 30_000;
const MAX_COORDINATOR_EVENTS: usize = 256;

/// App clock used for monotonic scheduling and truthful tariff timestamps.
///
/// Tests inject a manually advanced implementation. The provider crate's clock is a supertrait so the same
/// epoch drives the Vertex resolver and the coordinator without sleeping.
pub(crate) trait CoordinatorClock: ProviderClock {
    fn unix_millis(&self) -> i64;
}

struct VertexClock(Arc<dyn CoordinatorClock>);

impl ProviderClock for VertexClock {
    fn monotonic_micros(&self) -> u64 {
        self.0.monotonic_micros()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaneFamily {
    Live,
    Final,
    Question,
}

impl LaneFamily {
    const fn contains(self, lane: Lane) -> bool {
        matches!(
            (self, lane),
            (Self::Live, Lane::Live)
                | (Self::Final, Lane::Final)
                | (Self::Question, Lane::AdHocQuestion | Lane::PinnedQuestion)
        )
    }

    pub(crate) const fn of(lane: Lane) -> Self {
        match lane {
            Lane::Live => Self::Live,
            Lane::Final => Self::Final,
            Lane::AdHocQuestion | Lane::PinnedQuestion => Self::Question,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LaneSelectionDto {
    pub(crate) provider: Option<ProviderId>,
    pub(crate) transport: Option<TransportId>,
    pub(crate) model: Option<ModelId>,
    pub(crate) cache_policy: CachePolicy,
}

impl Default for LaneSelectionDto {
    fn default() -> Self {
        Self {
            provider: None,
            transport: None,
            model: None,
            cache_policy: CachePolicy {
                local: LocalCacheMode::Reusable,
                provider: corti_postprocess::ProviderCacheMode::Off,
            },
        }
    }
}

impl LaneSelectionDto {
    fn is_complete(&self) -> bool {
        self.provider.is_some() && self.transport.is_some() && self.model.is_some()
    }

    fn is_all_none(&self) -> bool {
        self.provider.is_none() && self.transport.is_none() && self.model.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LaneControlDto {
    pub(crate) enabled: bool,
    pub(crate) revision: u64,
    pub(crate) selection: LaneSelectionDto,
}

impl Default for LaneControlDto {
    fn default() -> Self {
        Self {
            enabled: false,
            revision: 1,
            selection: LaneSelectionDto::default(),
        }
    }
}

/// Complete secret-free runtime control projection. Credential presence is projected separately and no
/// secret reference/value can be represented by this DTO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ControlSnapshotDto {
    pub(crate) process_epoch: ProcessEpoch,
    pub(crate) session_generation: u64,
    pub(crate) control_revision: u64,
    pub(crate) steering_revision: u64,
    pub(crate) bank_revision: u64,
    pub(crate) pinned_question_revision: u64,
    pub(crate) master_enabled: bool,
    pub(crate) egress_acknowledged: bool,
    pub(crate) pinned_auto_enabled: bool,
    pub(crate) codex_experimental_approved: bool,
    pub(crate) live: LaneControlDto,
    pub(crate) final_lane: LaneControlDto,
    pub(crate) questions: LaneControlDto,
}

impl ControlSnapshotDto {
    fn defaults(process_epoch: ProcessEpoch) -> Self {
        Self {
            process_epoch,
            session_generation: 1,
            control_revision: 1,
            steering_revision: 1,
            bank_revision: 1,
            pinned_question_revision: 0,
            master_enabled: false,
            egress_acknowledged: false,
            pinned_auto_enabled: false,
            codex_experimental_approved: false,
            live: LaneControlDto::default(),
            final_lane: LaneControlDto::default(),
            questions: LaneControlDto::default(),
        }
    }

    fn lane(&self, family: LaneFamily) -> &LaneControlDto {
        match family {
            LaneFamily::Live => &self.live,
            LaneFamily::Final => &self.final_lane,
            LaneFamily::Question => &self.questions,
        }
    }

    fn lane_mut(&mut self, family: LaneFamily) -> &mut LaneControlDto {
        match family {
            LaneFamily::Live => &mut self.live,
            LaneFamily::Final => &mut self.final_lane,
            LaneFamily::Question => &mut self.questions,
        }
    }

    fn enabled_for(&self, lane: Lane) -> bool {
        self.master_enabled && self.lane(LaneFamily::of(lane)).enabled
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ControlPatch {
    SetEgressAcknowledged(bool),
    SetMaster(bool),
    SetLaneEnabled {
        lane: LaneFamily,
        enabled: bool,
    },
    SetLaneSelection {
        lane: LaneFamily,
        selection: LaneSelectionDto,
    },
    SetPinnedAuto(bool),
    SetCodexExperimentalApproved(bool),
    /// Non-secret provider scope (account alias/project/region) changed. It fences every lane because more
    /// than one lane may select the same transport.
    ProviderScopeChanged,
    SteeringChanged,
    BankChanged,
    /// Session-only steering never enters the persisted hosted document, but still fences the next request.
    SessionSteeringChanged,
    /// Display-only changes deliberately do not alter any request generation.
    DisplayOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancellationScope {
    None,
    All(CancellationReason),
    Family(LaneFamily, CancellationReason),
    Pinned(CancellationReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum ControlError {
    #[error("control generation overflow")]
    GenerationOverflow,
    #[error("hosted egress acknowledgement is required")]
    EgressNotAcknowledged,
    #[error("an enabled lane requires an exact provider, transport, and model")]
    IncompleteLaneSelection,
    #[error("lane selection must be either complete or empty")]
    PartialLaneSelection,
    #[error("the selected provider transport is unavailable")]
    ProviderUnavailable,
    #[error("the selected provider transport is blocked by policy")]
    ProviderBlocked,
    #[error("the selected provider transport remains experimental and off")]
    ExperimentalProviderOff,
    #[error("persisting hosted controls failed")]
    Persistence,
}

pub(crate) trait ControlPersistence: Send {
    /// Persist only this secret-free projection. Implementations must durably replace the hosted document.
    fn persist(&mut self, snapshot: &ControlSnapshotDto) -> Result<(), ErrorCode>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PatchOutcome {
    Unchanged(ControlSnapshotDto),
    Applied(ControlSnapshotDto),
    /// Disable is fail-safe: runtime is already off even though persistence failed.
    DisabledForSession {
        snapshot: ControlSnapshotDto,
        error: ErrorCode,
    },
}

struct PostprocessControl {
    snapshot: ControlSnapshotDto,
}

impl PostprocessControl {
    fn new(process_epoch: ProcessEpoch) -> Self {
        Self {
            snapshot: ControlSnapshotDto::defaults(process_epoch),
        }
    }

    fn snapshot(&self) -> &ControlSnapshotDto {
        &self.snapshot
    }

    fn preview(
        &self,
        patch: &ControlPatch,
    ) -> Result<(ControlSnapshotDto, CancellationScope), ControlError> {
        let mut next = self.snapshot.clone();
        let scope = match patch {
            ControlPatch::SetEgressAcknowledged(value) => {
                next.egress_acknowledged = *value;
                CancellationScope::None
            }
            ControlPatch::SetMaster(enabled) => {
                if next.master_enabled == *enabled {
                    return Ok((next, CancellationScope::None));
                }
                if *enabled && !next.egress_acknowledged {
                    return Err(ControlError::EgressNotAcknowledged);
                }
                next.master_enabled = *enabled;
                bump(&mut next.control_revision)?;
                if *enabled {
                    CancellationScope::None
                } else {
                    CancellationScope::All(CancellationReason::MasterDisabled)
                }
            }
            ControlPatch::SetLaneEnabled { lane, enabled } => {
                if next.lane(*lane).enabled == *enabled {
                    return Ok((next, CancellationScope::None));
                }
                if *enabled && !next.lane(*lane).selection.is_complete() {
                    return Err(ControlError::IncompleteLaneSelection);
                }
                let lane_state = next.lane_mut(*lane);
                lane_state.enabled = *enabled;
                bump(&mut lane_state.revision)?;
                if *enabled {
                    CancellationScope::None
                } else {
                    CancellationScope::Family(*lane, CancellationReason::LaneDisabled)
                }
            }
            ControlPatch::SetLaneSelection { lane, selection } => {
                if !selection.is_complete() && !selection.is_all_none() {
                    return Err(ControlError::PartialLaneSelection);
                }
                if next.lane(*lane).selection == *selection {
                    return Ok((next, CancellationScope::None));
                }
                if next.lane(*lane).enabled && !selection.is_complete() {
                    return Err(ControlError::IncompleteLaneSelection);
                }
                let lane_state = next.lane_mut(*lane);
                lane_state.selection = selection.clone();
                bump(&mut lane_state.revision)?;
                CancellationScope::Family(*lane, CancellationReason::ModelChanged)
            }
            ControlPatch::SetPinnedAuto(enabled) => {
                if next.pinned_auto_enabled == *enabled {
                    return Ok((next, CancellationScope::None));
                }
                next.pinned_auto_enabled = *enabled;
                // Pinned automation is independent of explicit ad-hoc FIFO work. Fence it with the pinned
                // question generation rather than invalidating the shared question-lane revision.
                bump(&mut next.pinned_question_revision)?;
                if *enabled {
                    CancellationScope::None
                } else {
                    CancellationScope::Pinned(CancellationReason::LaneDisabled)
                }
            }
            ControlPatch::SetCodexExperimentalApproved(approved) => {
                if next.codex_experimental_approved == *approved {
                    return Ok((next, CancellationScope::None));
                }
                next.codex_experimental_approved = *approved;
                bump(&mut next.control_revision)?;
                CancellationScope::All(CancellationReason::ModelChanged)
            }
            ControlPatch::ProviderScopeChanged => {
                bump(&mut next.control_revision)?;
                CancellationScope::All(CancellationReason::ModelChanged)
            }
            ControlPatch::SteeringChanged | ControlPatch::SessionSteeringChanged => {
                bump(&mut next.steering_revision)?;
                CancellationScope::All(CancellationReason::SteeringChanged)
            }
            ControlPatch::BankChanged => {
                bump(&mut next.bank_revision)?;
                CancellationScope::All(CancellationReason::WordBankChanged)
            }
            ControlPatch::DisplayOnly => CancellationScope::None,
        };
        Ok((next, scope))
    }

    fn commit(&mut self, snapshot: ControlSnapshotDto) {
        self.snapshot = snapshot;
    }

    fn begin_session(&mut self) -> Result<ControlSnapshotDto, ControlError> {
        bump(&mut self.snapshot.session_generation)?;
        Ok(self.snapshot.clone())
    }

    fn commit_pinned_question_revision(&mut self) -> Result<u64, ControlError> {
        bump(&mut self.snapshot.pinned_question_revision)?;
        Ok(self.snapshot.pinned_question_revision)
    }

    fn fence(
        &self,
        lane: Lane,
        watermark: TranscriptWatermark,
        question_revision: Option<u64>,
    ) -> RequestFence {
        RequestFence {
            process_epoch: self.snapshot.process_epoch,
            session_generation: self.snapshot.session_generation,
            transcript_revision: watermark.transcript_revision,
            control_revision: self.snapshot.control_revision,
            lane_revision: self.snapshot.lane(LaneFamily::of(lane)).revision,
            steering_revision: self.snapshot.steering_revision,
            bank_revision: self.snapshot.bank_revision,
            question_revision,
        }
    }

    fn fence_controls_are_current(&self, lane: Lane, fence: &RequestFence) -> bool {
        let current = &self.snapshot;
        current.enabled_for(lane)
            && fence.process_epoch == current.process_epoch
            && fence.session_generation == current.session_generation
            && fence.control_revision == current.control_revision
            && fence.lane_revision == current.lane(LaneFamily::of(lane)).revision
            && fence.steering_revision == current.steering_revision
            && fence.bank_revision == current.bank_revision
            && (lane != Lane::PinnedQuestion
                || fence.question_revision == Some(current.pinned_question_revision))
    }
}

fn bump(value: &mut u64) -> Result<(), ControlError> {
    *value = value
        .checked_add(1)
        .ok_or(ControlError::GenerationOverflow)?;
    Ok(())
}

/// Monotonic transcript watermark minted only by the coordinator after newly finalized rows arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct TranscriptWatermark {
    pub(crate) session_generation: u64,
    pub(crate) transcript_revision: u64,
    pub(crate) finalized_word_tokens: u64,
    pub(crate) covered_speech_ms: u64,
    pub(crate) finalized_rows: u64,
}

impl TranscriptWatermark {
    fn initial(session_generation: u64) -> Self {
        Self {
            session_generation,
            transcript_revision: 0,
            finalized_word_tokens: 0,
            covered_speech_ms: 0,
            finalized_rows: 0,
        }
    }
}

/// A request plus encrypted-store metadata. Its debug representation delegates to the domain request, whose
/// prompt and rows are redacted; this type deliberately has no serialization implementation.
pub(crate) struct RequestSubmission {
    pub(crate) recording_id: String,
    pub(crate) request: HostedRequest,
    pub(crate) request_key: RequestKey,
    pub(crate) scope: ProviderScope,
    pub(crate) adapter_version: u32,
    pub(crate) request_max_output_bytes: usize,
    pub(crate) catalog_max_output_bytes: usize,
    pub(crate) expected_context_truncated: bool,
}

impl fmt::Debug for RequestSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestSubmission")
            .field("recording_id", &self.recording_id)
            .field("request", &self.request)
            .field("request_key", &self.request_key)
            .field("scope", &self.scope)
            .field("adapter_version", &self.adapter_version)
            .field("request_max_output_bytes", &self.request_max_output_bytes)
            .field("catalog_max_output_bytes", &self.catalog_max_output_bytes)
            .field(
                "expected_context_truncated",
                &self.expected_context_truncated,
            )
            .finish()
    }
}

impl RequestSubmission {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        recording_id: impl Into<String>,
        request: HostedRequest,
        request_key: RequestKey,
        scope: ProviderScope,
        adapter_version: u32,
        request_max_output_bytes: usize,
        catalog_max_output_bytes: usize,
        expected_context_truncated: bool,
    ) -> Result<Self, SubmitError> {
        let recording_id = recording_id.into();
        if recording_id.is_empty()
            || recording_id.len() > 512
            || recording_id.chars().any(char::is_control)
        {
            return Err(SubmitError::InvalidRecordingId);
        }
        if request_max_output_bytes == 0 || catalog_max_output_bytes == 0 {
            return Err(SubmitError::InvalidOutputLimit);
        }
        Ok(Self {
            recording_id,
            request,
            request_key,
            scope,
            adapter_version,
            request_max_output_bytes,
            catalog_max_output_bytes,
            expected_context_truncated,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum SubmitError {
    #[error("the hosted lane is disabled")]
    Disabled,
    #[error("the request lane does not match the submission method")]
    WrongLane,
    #[error("the transcript watermark belongs to another session")]
    StaleWatermark,
    #[error("the request provider/model does not match the exact lane selection")]
    SelectionChanged,
    #[error("request deadline has expired")]
    Deadline,
    #[error("duplicate call id")]
    DuplicateCall,
    #[error("the ad-hoc question FIFO is full")]
    AdHocQueueFull,
    #[error("question text is empty or too large")]
    InvalidQuestion,
    #[error("no committed pinned question template exists")]
    NoPinnedTemplate,
    #[error("invalid recording id")]
    InvalidRecordingId,
    #[error("invalid output limit")]
    InvalidOutputLimit,
    #[error("coordinator generation overflow")]
    GenerationOverflow,
    #[error("provider is unavailable or blocked")]
    ProviderBlocked,
}

/// Trusted plaintext returned by the encrypted exact store. The coordinator revalidates it against the
/// current immutable request before any application.
#[derive(Clone)]
pub(crate) enum ExactLookup {
    Miss,
    Hit(ProviderOutput),
    Corrupt,
}

impl fmt::Debug for ExactLookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Miss => f.write_str("Miss"),
            Self::Hit(output) => f.debug_tuple("Hit").field(output).finish(),
            Self::Corrupt => f.write_str("Corrupt"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FinalJournalState {
    Prepared,
    Dispatched,
    ResultCached,
    Applied,
    Checkpointed,
    Abandoned,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct FinalJournalBoundary {
    pub(crate) recording_id: String,
    pub(crate) request_group_id: corti_postprocess::RequestGroupId,
    pub(crate) call_id: CallId,
    pub(crate) request_key: RequestKey,
    pub(crate) fence: RequestFence,
}

impl fmt::Debug for FinalJournalBoundary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FinalJournalBoundary")
            .field("recording_id", &self.recording_id)
            .field("request_group_id", &self.request_group_id)
            .field("call_id", &self.call_id)
            .field("request_key", &self.request_key)
            .field("fence", &self.fence)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalRecoveryRecord {
    pub(crate) call_id: CallId,
    pub(crate) state: FinalJournalState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum FinalRecoveryDirective {
    None,
    ResumePrepared {
        call_id: CallId,
    },
    ResumeEncryptedResult {
        call_id: CallId,
    },
    ResumeCheckpoint {
        call_id: CallId,
    },
    Fallback {
        call_id: CallId,
        code: ErrorCode,
        explicit_retry_required: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalOutcomeDto {
    Completed,
    Failed,
    Canceled,
    Superseded,
    Timeout,
}

/// Content-free terminal telemetry suitable for the encrypted outbox and eventual queue import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TerminalTelemetryDto {
    pub(crate) call_id: CallId,
    pub(crate) recording_id: String,
    pub(crate) request_group_id: corti_postprocess::RequestGroupId,
    pub(crate) target_id: Option<corti_postprocess::TargetId>,
    pub(crate) lane: Lane,
    pub(crate) attempt_no: u64,
    pub(crate) fence: RequestFence,
    pub(crate) provider: ProviderId,
    pub(crate) transport: TransportId,
    pub(crate) model: ModelId,
    pub(crate) support_tier: SupportTier,
    pub(crate) adapter_version: u32,
    pub(crate) prompt_version: u32,
    pub(crate) output_schema_version: u32,
    pub(crate) outcome: TerminalOutcomeDto,
    pub(crate) error: Option<ErrorCode>,
    pub(crate) provider_request_sent: bool,
    pub(crate) late_content_discarded: bool,
    pub(crate) cache: CacheObservation,
    pub(crate) usage: NormalizedUsage,
    pub(crate) cost: CostEstimate,
    pub(crate) latency: LatencyFields,
    /// Wall time captured when the call entered the coordinator. Monotonic time remains the scheduling
    /// authority, but durable history needs a process-independent UTC timestamp.
    pub(crate) queued_at_unix_ms: i64,
    pub(crate) dispatched_at_unix_ms: Option<i64>,
    pub(crate) completed_at_unix_ms: i64,
}

/// Validated content remains private to app integration and the encrypted store. Debug output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ValidatedOutput {
    Rewrite {
        rows: Vec<TranscriptRow>,
    },
    Question {
        answer: String,
        cited_row_ids: Vec<corti_postprocess::RowId>,
        context_truncated: bool,
    },
}

impl fmt::Debug for ValidatedOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rewrite { rows } => f
                .debug_struct("Rewrite")
                .field("row_count", &rows.len())
                .finish(),
            Self::Question {
                answer,
                cited_row_ids,
                context_truncated,
            } => f
                .debug_struct("Question")
                .field("answer_bytes", &answer.len())
                .field("citation_count", &cited_row_ids.len())
                .field("context_truncated", context_truncated)
                .finish(),
        }
    }
}

impl ValidatedOutput {
    pub(crate) fn rewritten_rows(&self) -> Option<&[TranscriptRow]> {
        match self {
            Self::Rewrite { rows } => Some(rows),
            Self::Question { .. } => None,
        }
    }

    pub(crate) fn answer(&self) -> Option<&str> {
        match self {
            Self::Rewrite { .. } => None,
            Self::Question { answer, .. } => Some(answer),
        }
    }
}

pub(crate) struct StoreCommit<'a> {
    pub(crate) request_key: RequestKey,
    pub(crate) lane: Lane,
    pub(crate) local_cache_mode: LocalCacheMode,
    /// Raw typed provider output is persisted only inside the encrypted store and revalidated on every hit.
    pub(crate) cache_output: &'a ProviderOutput,
    pub(crate) output: &'a ValidatedOutput,
    pub(crate) final_boundary: Option<&'a FinalJournalBoundary>,
    pub(crate) telemetry: &'a TerminalTelemetryDto,
}

/// One-owner encrypted store boundary. Implementations must keep result/ledger/recovery content encrypted;
/// only `TerminalTelemetryDto` is permitted in its plaintext outbox.
pub(crate) trait EncryptedPostprocessStore: Send {
    fn lookup_exact(&mut self, key: RequestKey) -> Result<ExactLookup, ErrorCode>;
    fn evict_corrupt(&mut self, key: RequestKey) -> Result<(), ErrorCode>;
    fn prepare_final(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode>;
    /// Persist the conservative ambiguous-dispatch state before a ticket may reach any egress transport.
    fn mark_final_dispatched(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode>;
    /// For final calls this must atomically persist encrypted output and move the journal to ResultCached.
    fn commit_validated(&mut self, commit: StoreCommit<'_>) -> Result<(), ErrorCode>;
    fn abandon_final(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode>;
    /// Move an entire final chunk group in one durable transaction; partial group publication is forbidden.
    fn mark_final_group_applied(
        &mut self,
        boundaries: &[FinalJournalBoundary],
    ) -> Result<(), ErrorCode>;
    fn mark_final_group_checkpointed(
        &mut self,
        boundaries: &[FinalJournalBoundary],
    ) -> Result<(), ErrorCode>;
    fn recover_final(
        &mut self,
        recording_id: &str,
    ) -> Result<Option<FinalRecoveryRecord>, ErrorCode>;
    fn record_terminal(&mut self, telemetry: &TerminalTelemetryDto) -> Result<(), ErrorCode>;
}

/// Injected, secret-free provider/auth/catalog directory. Implementations may communicate with dedicated
/// auth/provider workers, but the coordinator never receives a key/token and never performs discovery.
pub(crate) fn provider_support_catalog() -> Vec<ProviderDescriptor> {
    [
        corti_postprocess::KnownTransport::VertexDirect,
        corti_postprocess::KnownTransport::OpenAiDirect,
        corti_postprocess::KnownTransport::CodexAppServer,
        corti_postprocess::KnownTransport::AnthropicDirect,
        corti_postprocess::KnownTransport::ClaudeSubscription,
        corti_postprocess::KnownTransport::BedrockRuntime,
    ]
    .into_iter()
    .map(corti_postprocess::KnownTransport::descriptor)
    .collect()
}

pub(crate) trait ProviderAccess: Send {
    fn descriptor(
        &mut self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> Option<ProviderDescriptor>;
    fn credential_state(
        &mut self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> CredentialState;
    fn catalog(
        &mut self,
        provider: &ProviderId,
        transport: &TransportId,
        scope: &ProviderScope,
    ) -> Result<ModelCatalog, PostprocessError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ProviderStateDto {
    pub(crate) descriptor: ProviderDescriptor,
    pub(crate) credential: CredentialState,
    pub(crate) models: Vec<ModelDescriptor>,
    /// Service/IAM/quota/model failure remains distinct from token readiness.
    pub(crate) service_error: Option<ErrorCode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LaneStateDto {
    Disabled,
    WaitingForPhrase,
    Debouncing,
    Queued,
    Arming,
    CatchingUp,
    Rewriting,
    Finalizing,
    Clean,
    UsingRaw,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UiFenceDto {
    pub(crate) process_epoch: ProcessEpoch,
    pub(crate) session_generation: u64,
    pub(crate) control_revision: u64,
    pub(crate) lane_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LaneStateEventDto {
    pub(crate) lane: Lane,
    pub(crate) state: LaneStateDto,
    pub(crate) code: Option<ErrorCode>,
    pub(crate) fence: UiFenceDto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct NoticeDto {
    pub(crate) role: &'static str,
    pub(crate) visible_message: &'static str,
    pub(crate) episode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccountingFinalityDto {
    Provisional,
    Final,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AccountingEventDto {
    pub(crate) call_id: CallId,
    pub(crate) recording_id: String,
    pub(crate) lane: Lane,
    pub(crate) fence: RequestFence,
    pub(crate) finality: AccountingFinalityDto,
    pub(crate) usage: NormalizedUsage,
    pub(crate) cost: CostEstimate,
    pub(crate) late: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum CoordinatorEventDto {
    ControlChanged(Box<ControlSnapshotDto>),
    LaneState(LaneStateEventDto),
    ProviderState(Box<ProviderStateDto>),
    Notice(NoticeDto),
    Accounting(Box<AccountingEventDto>),
    Terminal(Box<TerminalTelemetryDto>),
    PersistenceWarning { code: ErrorCode },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuestionStatusDto {
    Queued,
    WaitingForCredential,
    Running,
    Completed,
    Canceled,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct QuestionSummaryDto {
    pub(crate) call_id: CallId,
    pub(crate) as_of_revision: u64,
    pub(crate) status: QuestionStatusDto,
    pub(crate) error: Option<ErrorCode>,
    pub(crate) usage: Option<NormalizedUsage>,
    pub(crate) cost: Option<CostEstimate>,
}

struct SensitiveText(String);

impl SensitiveText {
    fn new(value: String) -> Self {
        Self(value)
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

impl fmt::Debug for SensitiveText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SensitiveText")
            .field("bytes", &self.0.len())
            .finish()
    }
}

#[derive(Debug)]
struct AdHocEntry {
    call_id: CallId,
    as_of_revision: u64,
    question: SensitiveText,
    answer: Option<SensitiveText>,
    status: QuestionStatusDto,
    error: Option<ErrorCode>,
    usage: Option<NormalizedUsage>,
    cost: Option<CostEstimate>,
}

/// Borrowed content view for the assistant UI. It is intentionally non-serializable and redacted in Debug;
/// an eventual Tauri command must copy it only into the intended live window response.
pub(crate) struct QuestionContent<'a> {
    pub(crate) question: &'a str,
    pub(crate) answer: Option<&'a str>,
}

impl fmt::Debug for QuestionContent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionContent")
            .field("question_bytes", &self.question.len())
            .field("answer_bytes", &self.answer.map(str::len))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueStage {
    NeedsCache,
    NeedsAuth,
    WaitingCredential,
    WaitingVertex,
    ReadyForCatalog,
}

struct QueuedCall {
    sequence: u64,
    queued_at_micros: u64,
    queued_at_unix_ms: i64,
    eligible_at_micros: u64,
    watermark: TranscriptWatermark,
    descriptor: ProviderDescriptor,
    submission: RequestSubmission,
    stage: QueueStage,
    final_prepared: bool,
}

impl fmt::Debug for QueuedCall {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueuedCall")
            .field("sequence", &self.sequence)
            .field("queued_at_micros", &self.queued_at_micros)
            .field("queued_at_unix_ms", &self.queued_at_unix_ms)
            .field("eligible_at_micros", &self.eligible_at_micros)
            .field("watermark", &self.watermark)
            .field("descriptor", &self.descriptor)
            .field("submission", &self.submission)
            .field("stage", &self.stage)
            .field("final_prepared", &self.final_prepared)
            .finish()
    }
}

#[derive(Debug)]
struct PinnedCandidate {
    submission: RequestSubmission,
    watermark: TranscriptWatermark,
}

#[derive(Debug)]
struct PinnedProgress {
    request_watermark: TranscriptWatermark,
    candidate: Option<PinnedCandidate>,
    quiet_due_at_micros: Option<u64>,
    dirty_while_running: bool,
    run_count: u64,
}

impl PinnedProgress {
    fn new(session_generation: u64) -> Self {
        Self {
            request_watermark: TranscriptWatermark::initial(session_generation),
            candidate: None,
            quiet_due_at_micros: None,
            dirty_while_running: false,
            run_count: 0,
        }
    }
}

#[derive(Debug)]
struct ActiveCall {
    sequence: u64,
    recording_id: String,
    context: EventContext,
    cancel: CancellationToken,
    descriptor: ProviderDescriptor,
    final_boundary: Option<FinalJournalBoundary>,
    provider_request_sent: bool,
    dispatch_started_at_micros: Option<u64>,
    dispatched_at_unix_ms: Option<i64>,
    first_text_seen: bool,
    observed_terminal_usage: Option<NormalizedUsage>,
    model: ModelId,
    region: Option<String>,
    deadline: MonotonicDeadline,
}

/// Provider worker ticket. It owns the immutable request until completion, allowing late completions to
/// retain all content-free telemetry even after the request was canceled. It has no serialization surface.
pub(crate) struct DispatchTicket {
    sequence: u64,
    call: Arc<RequestSubmission>,
    descriptor: ProviderDescriptor,
    cancel: CancellationToken,
    queued_at_micros: u64,
    queued_at_unix_ms: i64,
}

impl fmt::Debug for DispatchTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DispatchTicket")
            .field("sequence", &self.sequence)
            .field("call_id", &self.call.request.call_id)
            .field("lane", &self.call.request.lane)
            .field("descriptor", &self.descriptor)
            .field("cancel", &self.cancel)
            .field("queued_at_micros", &self.queued_at_micros)
            .field("queued_at_unix_ms", &self.queued_at_unix_ms)
            .finish()
    }
}

impl DispatchTicket {
    pub(crate) fn request(&self) -> &HostedRequest {
        &self.call.request
    }

    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }

    /// Execute only on a dedicated provider worker. The adapter is injected; this helper creates no
    /// transport and refuses descriptor substitution.
    pub(crate) fn execute_with(
        &self,
        adapter: &mut dyn ProviderAdapter,
        sink: &dyn ProviderEventSink,
    ) -> Result<corti_postprocess::ProviderTerminal, PostprocessError> {
        if adapter.descriptor() != self.descriptor {
            return Err(ErrorCode::PolicyBlocked.into());
        }
        adapter.execute(&self.call.request, &self.cancel, sink)
    }
}

/// Content-bearing result ready for the narrow live/pipeline integration boundary. Final output is always
/// recovery-committed before this value exists. Debug output cannot reveal replacement/answer text.
pub(crate) struct ApplyReady {
    pub(crate) call_id: CallId,
    pub(crate) lane: Lane,
    pub(crate) fence: RequestFence,
    pub(crate) output: ValidatedOutput,
    pub(crate) recovery_committed: bool,
}

impl fmt::Debug for ApplyReady {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplyReady")
            .field("call_id", &self.call_id)
            .field("lane", &self.lane)
            .field("fence", &self.fence)
            .field("output", &self.output)
            .field("recovery_committed", &self.recovery_committed)
            .finish()
    }
}

#[derive(Debug)]
pub(crate) enum DispatchOutcome {
    Ticket(DispatchTicket),
    CacheApply(ApplyReady),
    Waiting,
    Backpressured,
    Failed { call_id: CallId, code: ErrorCode },
    Empty,
}

#[derive(Debug)]
pub(crate) enum CompletionOutcome {
    Apply(ApplyReady),
    Discarded { call_id: CallId, code: ErrorCode },
    Failed { call_id: CallId, code: ErrorCode },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum CoordinatorError {
    #[error("provider event does not match an active request")]
    UnknownCall,
    #[error("provider event context does not match the immutable request")]
    EventFenceMismatch,
    #[error("Vertex resolver completion is stale")]
    StaleVertexAttempt,
    #[error("encrypted post-processing store failed")]
    Store,
    #[error("final application fence is stale")]
    StaleApplication,
}

impl From<VertexResolverError> for CoordinatorError {
    fn from(_: VertexResolverError) -> Self {
        Self::StaleVertexAttempt
    }
}

/// Pure/injected coordinator. Drive it from its dedicated app thread; provider calls execute from tickets on
/// separate bounded workers. None of its submission methods are intended for HAL/capture threads—those use
/// [`CoordinatorIngress::try_send`] below.
pub(crate) struct PostprocessCoordinator {
    clock: Arc<dyn CoordinatorClock>,
    control: PostprocessControl,
    persistence: Box<dyn ControlPersistence>,
    store: Box<dyn EncryptedPostprocessStore>,
    providers: Box<dyn ProviderAccess>,
    pricing: Arc<dyn PricingCatalog>,
    vertex: VertexCredentialResolver,
    provider_states: HashMap<(ProviderId, TransportId), ProviderStateDto>,
    queue: VecDeque<QueuedCall>,
    active: HashMap<CallId, ActiveCall>,
    next_sequence: u64,
    next_question_revision: u64,
    watermark: TranscriptWatermark,
    last_progress_at_micros: u64,
    pinned: PinnedProgress,
    pinned_template: Option<SensitiveText>,
    ad_hoc: VecDeque<AdHocEntry>,
    awaiting_final_apply: HashMap<CallId, FinalJournalBoundary>,
    events: VecDeque<CoordinatorEventDto>,
}

impl fmt::Debug for PostprocessCoordinator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostprocessCoordinator")
            .field("control", &self.control.snapshot())
            .field("provider_state_count", &self.provider_states.len())
            .field("queued", &self.queue.len())
            .field("active", &self.active.len())
            .field("watermark", &self.watermark)
            .field("pinned", &self.pinned)
            .field(
                "pinned_template_bytes",
                &self.pinned_template.as_ref().map(SensitiveText::len),
            )
            .field("ad_hoc_count", &self.ad_hoc.len())
            .field("awaiting_final_apply", &self.awaiting_final_apply.len())
            .field("event_count", &self.events.len())
            .finish()
    }
}

impl PostprocessCoordinator {
    pub(crate) fn new(
        process_epoch: ProcessEpoch,
        clock: Arc<dyn CoordinatorClock>,
        persistence: Box<dyn ControlPersistence>,
        store: Box<dyn EncryptedPostprocessStore>,
        providers: Box<dyn ProviderAccess>,
        pricing: Arc<dyn PricingCatalog>,
    ) -> Self {
        Self::new_with_snapshot(
            ControlSnapshotDto::defaults(process_epoch),
            None,
            clock,
            persistence,
            store,
            providers,
            pricing,
        )
    }

    /// Restore the already-validated, secret-free hosted document at process startup. This avoids replaying
    /// Settings patches (and rewriting the file) merely to seed runtime state. Callers must always supply
    /// the current process epoch; stale persisted epochs are never accepted.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_snapshot(
        snapshot: ControlSnapshotDto,
        pinned_template: Option<String>,
        clock: Arc<dyn CoordinatorClock>,
        persistence: Box<dyn ControlPersistence>,
        store: Box<dyn EncryptedPostprocessStore>,
        providers: Box<dyn ProviderAccess>,
        pricing: Arc<dyn PricingCatalog>,
    ) -> Self {
        let session_generation = snapshot.session_generation;
        let control = PostprocessControl { snapshot };
        let vertex = VertexCredentialResolver::new(Box::new(VertexClock(clock.clone())));
        let provider_states = provider_support_catalog()
            .into_iter()
            .map(|descriptor| {
                let credential = match descriptor.support_tier {
                    SupportTier::Blocked | SupportTier::Experimental => {
                        CredentialState::Unsupported {
                            code: ErrorCode::PolicyBlocked,
                        }
                    }
                    SupportTier::Documented => CredentialState::Absent,
                };
                (
                    (descriptor.provider.clone(), descriptor.transport.clone()),
                    ProviderStateDto {
                        descriptor,
                        credential,
                        models: Vec::new(),
                        service_error: None,
                    },
                )
            })
            .collect();
        Self {
            clock,
            control,
            persistence,
            store,
            providers,
            pricing,
            vertex,
            provider_states,
            queue: VecDeque::new(),
            active: HashMap::new(),
            next_sequence: 1,
            next_question_revision: 1,
            watermark: TranscriptWatermark::initial(session_generation),
            last_progress_at_micros: 0,
            pinned: PinnedProgress::new(session_generation),
            pinned_template: pinned_template
                .filter(|text| !text.trim().is_empty())
                .map(SensitiveText::new),
            ad_hoc: VecDeque::new(),
            awaiting_final_apply: HashMap::new(),
            events: VecDeque::new(),
        }
    }

    pub(crate) fn control_snapshot(&self) -> &ControlSnapshotDto {
        self.control.snapshot()
    }

    pub(crate) fn provider_states(&self) -> impl Iterator<Item = &ProviderStateDto> {
        self.provider_states.values()
    }

    pub(crate) fn watermark(&self) -> TranscriptWatermark {
        self.watermark
    }

    /// Explicit Settings refresh. Catalog discovery remains injected and returns only typed descriptors;
    /// credential bytes and provider response bodies can never cross this boundary.
    pub(crate) fn refresh_provider(
        &mut self,
        provider: &ProviderId,
        transport: &TransportId,
        scope: &ProviderScope,
    ) -> Result<ProviderStateDto, ErrorCode> {
        let descriptor = self
            .providers
            .descriptor(provider, transport)
            .ok_or(ErrorCode::PolicyBlocked)?;
        if descriptor.support_tier == SupportTier::Blocked
            || (descriptor.support_tier == SupportTier::Experimental
                && !(CODEX_APP_SERVER_COMPILED
                    && self.control.snapshot().codex_experimental_approved))
        {
            return Err(ErrorCode::PolicyBlocked);
        }
        let credential = if descriptor.transport
            == corti_postprocess::KnownTransport::VertexDirect
                .descriptor()
                .transport
        {
            self.vertex.state().credential_state()
        } else {
            self.providers.credential_state(provider, transport)
        };
        let models = match &credential {
            CredentialState::Ready { .. } => {
                self.providers
                    .catalog(provider, transport, scope)
                    .map_err(|error| error.code)?
                    .models
            }
            _ => Vec::new(),
        };
        let state = ProviderStateDto {
            descriptor: descriptor.clone(),
            credential,
            models,
            service_error: None,
        };
        self.provider_states.insert(
            (descriptor.provider.clone(), descriptor.transport.clone()),
            state.clone(),
        );
        self.push_event(CoordinatorEventDto::ProviderState(Box::new(state.clone())));
        Ok(state)
    }

    pub(crate) fn take_events(&mut self) -> Vec<CoordinatorEventDto> {
        self.events.drain(..).collect()
    }

    pub(crate) fn apply_patch(
        &mut self,
        patch: ControlPatch,
    ) -> Result<PatchOutcome, ControlError> {
        let (proposed, scope) = self.control.preview(&patch)?;
        if proposed == *self.control.snapshot() {
            return Ok(PatchOutcome::Unchanged(proposed));
        }
        self.validate_control(&proposed)?;

        let disable_first = matches!(
            patch,
            ControlPatch::SetMaster(false)
                | ControlPatch::SetLaneEnabled { enabled: false, .. }
                | ControlPatch::SetPinnedAuto(false)
        );
        // These payloads are persisted by their narrow owner before this fence-only patch reaches the
        // coordinator (hosted preferences for scope/default steering, private word-bank document for bank).
        // Session steering is intentionally memory-only. Rewriting hosted.toml here adds no durability and
        // could leave newly persisted semantics active under an old fence if that unrelated write failed.
        let externally_persisted = matches!(
            patch,
            ControlPatch::ProviderScopeChanged
                | ControlPatch::SteeringChanged
                | ControlPatch::BankChanged
                | ControlPatch::SessionSteeringChanged
        );

        if disable_first {
            self.control.commit(proposed.clone());
            self.cancel_scope(scope);
            self.push_event(CoordinatorEventDto::ControlChanged(Box::new(
                proposed.clone(),
            )));
            if let Err(error) = self.persistence.persist(&proposed) {
                self.push_event(CoordinatorEventDto::PersistenceWarning { code: error });
                return Ok(PatchOutcome::DisabledForSession {
                    snapshot: proposed,
                    error,
                });
            }
        } else {
            if !externally_persisted {
                self.persistence
                    .persist(&proposed)
                    .map_err(|_| ControlError::Persistence)?;
            }
            self.control.commit(proposed.clone());
            self.cancel_scope(scope);
            self.push_event(CoordinatorEventDto::ControlChanged(Box::new(
                proposed.clone(),
            )));
        }
        Ok(PatchOutcome::Applied(proposed))
    }

    fn validate_control(&mut self, snapshot: &ControlSnapshotDto) -> Result<(), ControlError> {
        if snapshot.master_enabled && !snapshot.egress_acknowledged {
            return Err(ControlError::EgressNotAcknowledged);
        }
        for lane in [&snapshot.live, &snapshot.final_lane, &snapshot.questions] {
            if !lane.selection.is_complete() && !lane.selection.is_all_none() {
                return Err(ControlError::PartialLaneSelection);
            }
            if lane.enabled && !lane.selection.is_complete() {
                return Err(ControlError::IncompleteLaneSelection);
            }
            let (Some(provider), Some(transport)) = (
                lane.selection.provider.as_ref(),
                lane.selection.transport.as_ref(),
            ) else {
                continue;
            };
            let Some(descriptor) = self.providers.descriptor(provider, transport) else {
                return Err(ControlError::ProviderUnavailable);
            };
            match descriptor.support_tier {
                SupportTier::Blocked => return Err(ControlError::ProviderBlocked),
                SupportTier::Experimental
                    if !(CODEX_APP_SERVER_COMPILED && snapshot.codex_experimental_approved) =>
                {
                    return Err(ControlError::ExperimentalProviderOff);
                }
                SupportTier::Documented | SupportTier::Experimental => {}
            }
        }
        Ok(())
    }

    pub(crate) fn begin_session(&mut self) -> Result<ControlSnapshotDto, ControlError> {
        self.cancel_scope(CancellationScope::All(CancellationReason::SessionEnded));
        let snapshot = self.control.begin_session()?;
        self.queue.clear();
        self.ad_hoc.clear();
        for boundary in self.awaiting_final_apply.values() {
            let _ = self.store.abandon_final(boundary);
        }
        self.awaiting_final_apply.clear();
        self.vertex.clear_pending();
        self.watermark = TranscriptWatermark::initial(snapshot.session_generation);
        self.pinned = PinnedProgress::new(snapshot.session_generation);
        self.last_progress_at_micros = self.clock.monotonic_micros();
        self.push_event(CoordinatorEventDto::ControlChanged(Box::new(
            snapshot.clone(),
        )));
        Ok(snapshot)
    }

    /// Assign the next transcript revision and compute meaningful pinned progress from newly finalized rows.
    pub(crate) fn observe_finalized_rows(
        &mut self,
        rows: &[TranscriptRow],
    ) -> Result<TranscriptWatermark, SubmitError> {
        if rows.is_empty() {
            return Ok(self.watermark);
        }
        self.watermark.transcript_revision = self
            .watermark
            .transcript_revision
            .checked_add(1)
            .ok_or(SubmitError::GenerationOverflow)?;
        let words = rows.iter().try_fold(0u64, |sum, row| {
            let count = u64::try_from(row.text.unicode_words().count())
                .map_err(|_| SubmitError::GenerationOverflow)?;
            sum.checked_add(count)
                .ok_or(SubmitError::GenerationOverflow)
        })?;
        let speech_ms = rows.iter().try_fold(0u64, |sum, row| {
            let duration = row.end_ms.saturating_sub(row.start_ms);
            sum.checked_add(duration)
                .ok_or(SubmitError::GenerationOverflow)
        })?;
        self.watermark.finalized_word_tokens = self
            .watermark
            .finalized_word_tokens
            .checked_add(words)
            .ok_or(SubmitError::GenerationOverflow)?;
        self.watermark.covered_speech_ms = self
            .watermark
            .covered_speech_ms
            .checked_add(speech_ms)
            .ok_or(SubmitError::GenerationOverflow)?;
        self.watermark.finalized_rows = self
            .watermark
            .finalized_rows
            .checked_add(u64::try_from(rows.len()).map_err(|_| SubmitError::GenerationOverflow)?)
            .ok_or(SubmitError::GenerationOverflow)?;
        self.last_progress_at_micros = self.clock.monotonic_micros();
        if self.meaningful_pinned_progress(self.watermark) {
            self.pinned.quiet_due_at_micros = Some(
                self.last_progress_at_micros
                    .saturating_add(PINNED_QUIET_DEBOUNCE_MICROS),
            );
            if self.active_lane_count(Lane::PinnedQuestion) > 0 {
                self.pinned.dirty_while_running = true;
            }
        }
        Ok(self.watermark)
    }

    pub(crate) fn edit_pinned_template(&mut self, text: String) -> Result<(), SubmitError> {
        if text.len() > MAX_QUESTION_TEXT_BYTES || text.chars().any(char::is_control) {
            return Err(SubmitError::InvalidQuestion);
        }
        self.cancel_scope(CancellationScope::Pinned(CancellationReason::Superseded));
        self.remove_queued_lane(Lane::PinnedQuestion, CancellationReason::Superseded);
        self.pinned.candidate = None;
        self.pinned_template = (!text.trim().is_empty()).then(|| SensitiveText::new(text));
        self.control
            .commit_pinned_question_revision()
            .map_err(|_| SubmitError::GenerationOverflow)?;
        self.pinned.request_watermark = self.watermark;
        self.pinned.quiet_due_at_micros = None;
        self.pinned.dirty_while_running = false;
        self.push_event(CoordinatorEventDto::ControlChanged(Box::new(
            self.control.snapshot().clone(),
        )));
        Ok(())
    }

    pub(crate) fn submit_live(
        &mut self,
        submission: RequestSubmission,
        watermark: TranscriptWatermark,
    ) -> Result<(), SubmitError> {
        self.submit_auto(
            submission,
            watermark,
            Lane::Live,
            LIVE_DEBOUNCE_MICROS,
            None,
        )
    }

    pub(crate) fn submit_final(
        &mut self,
        submission: RequestSubmission,
        watermark: TranscriptWatermark,
    ) -> Result<(), SubmitError> {
        self.submit_auto(submission, watermark, Lane::Final, 0, None)
    }

    pub(crate) fn submit_pinned_snapshot(
        &mut self,
        submission: RequestSubmission,
        watermark: TranscriptWatermark,
    ) -> Result<(), SubmitError> {
        if submission.request.lane != Lane::PinnedQuestion {
            return Err(SubmitError::WrongLane);
        }
        if self.pinned_template.is_none() || self.control.snapshot().pinned_question_revision == 0 {
            return Err(SubmitError::NoPinnedTemplate);
        }
        self.validate_submission(&submission, watermark, Lane::PinnedQuestion)?;
        // A due-but-not-yet-dispatched pinned request is still pending state: replace it immediately with
        // this newest transcript snapshot. An in-flight request is left alone and sets the dirty rerun bit.
        self.remove_queued_lane(Lane::PinnedQuestion, CancellationReason::Superseded);
        self.pinned.candidate = Some(PinnedCandidate {
            submission,
            watermark,
        });
        if self.meaningful_pinned_progress(watermark) {
            self.pinned.quiet_due_at_micros = Some(
                self.last_progress_at_micros
                    .saturating_add(PINNED_QUIET_DEBOUNCE_MICROS),
            );
        }
        Ok(())
    }

    pub(crate) fn submit_ad_hoc(
        &mut self,
        submission: RequestSubmission,
        watermark: TranscriptWatermark,
        question: String,
    ) -> Result<(), SubmitError> {
        if submission.request.lane != Lane::AdHocQuestion {
            return Err(SubmitError::WrongLane);
        }
        if question.trim().is_empty()
            || question.len() > MAX_QUESTION_TEXT_BYTES
            || question.chars().any(char::is_control)
        {
            return Err(SubmitError::InvalidQuestion);
        }
        let queued = self
            .ad_hoc
            .iter()
            .filter(|item| {
                matches!(
                    item.status,
                    QuestionStatusDto::Queued
                        | QuestionStatusDto::WaitingForCredential
                        | QuestionStatusDto::Running
                )
            })
            .count();
        if queued >= MAX_QUEUED_AD_HOC_QUESTIONS {
            return Err(SubmitError::AdHocQueueFull);
        }
        self.validate_submission(&submission, watermark, Lane::AdHocQuestion)?;
        let revision = self.next_question_revision()?;
        let call_id = submission.request.call_id.clone();
        self.ad_hoc.push_back(AdHocEntry {
            call_id: call_id.clone(),
            as_of_revision: watermark.transcript_revision,
            question: SensitiveText::new(question),
            answer: None,
            status: QuestionStatusDto::Queued,
            error: None,
            usage: None,
            cost: None,
        });
        self.enqueue(submission, watermark, Some(revision), 0)?;
        self.trim_ad_hoc();
        Ok(())
    }

    fn submit_auto(
        &mut self,
        submission: RequestSubmission,
        watermark: TranscriptWatermark,
        lane: Lane,
        debounce_micros: u64,
        question_revision: Option<u64>,
    ) -> Result<(), SubmitError> {
        if submission.request.lane != lane {
            return Err(SubmitError::WrongLane);
        }
        self.validate_submission(&submission, watermark, lane)?;
        if matches!(lane, Lane::Live | Lane::PinnedQuestion) {
            self.remove_queued_lane(lane, CancellationReason::Superseded);
        }
        self.enqueue(submission, watermark, question_revision, debounce_micros)
    }

    fn validate_submission(
        &mut self,
        submission: &RequestSubmission,
        watermark: TranscriptWatermark,
        lane: Lane,
    ) -> Result<(), SubmitError> {
        if !self.control.snapshot().enabled_for(lane) {
            return Err(SubmitError::Disabled);
        }
        if watermark.session_generation != self.control.snapshot().session_generation
            || watermark.transcript_revision > self.watermark.transcript_revision
        {
            return Err(SubmitError::StaleWatermark);
        }
        if submission
            .request
            .deadline
            .is_expired_at(self.clock.monotonic_micros())
        {
            return Err(SubmitError::Deadline);
        }
        let selection = &self.control.snapshot().lane(LaneFamily::of(lane)).selection;
        if selection.provider.as_ref() != Some(&submission.request.provider)
            || selection.transport.as_ref() != Some(&submission.request.transport)
            || selection.model.as_ref() != Some(&submission.request.model)
            || selection.cache_policy != submission.request.cache_policy
        {
            return Err(SubmitError::SelectionChanged);
        }
        if self.call_exists(&submission.request.call_id) {
            return Err(SubmitError::DuplicateCall);
        }
        let Some(descriptor) = self
            .providers
            .descriptor(&submission.request.provider, &submission.request.transport)
        else {
            return Err(SubmitError::ProviderBlocked);
        };
        if descriptor.support_tier == SupportTier::Blocked
            || (descriptor.support_tier == SupportTier::Experimental
                && !(CODEX_APP_SERVER_COMPILED
                    && self.control.snapshot().codex_experimental_approved))
        {
            return Err(SubmitError::ProviderBlocked);
        }
        Ok(())
    }

    fn enqueue(
        &mut self,
        mut submission: RequestSubmission,
        watermark: TranscriptWatermark,
        question_revision: Option<u64>,
        debounce_micros: u64,
    ) -> Result<(), SubmitError> {
        let lane = submission.request.lane;
        let descriptor = self
            .providers
            .descriptor(&submission.request.provider, &submission.request.transport)
            .ok_or(SubmitError::ProviderBlocked)?;
        submission.request.fence = self.control.fence(lane, watermark, question_revision);
        let now = self.clock.monotonic_micros();
        let queued_at_unix_ms = self.clock.unix_millis();
        if lane.is_question() {
            submission.request.deadline = MonotonicDeadline(
                submission
                    .request
                    .deadline
                    .0
                    .min(now.saturating_add(QUESTION_DEADLINE_MICROS)),
            );
        }
        let sequence = self.next_sequence()?;
        let event_fence = ui_fence(&submission.request.fence);
        self.queue.push_back(QueuedCall {
            sequence,
            queued_at_micros: now,
            queued_at_unix_ms,
            eligible_at_micros: now.saturating_add(debounce_micros),
            watermark,
            descriptor,
            submission,
            stage: QueueStage::NeedsCache,
            final_prepared: false,
        });
        self.push_event(CoordinatorEventDto::LaneState(LaneStateEventDto {
            lane,
            state: if debounce_micros > 0 {
                LaneStateDto::Debouncing
            } else {
                LaneStateDto::Queued
            },
            code: None,
            fence: event_fence,
        }));
        Ok(())
    }

    fn next_sequence(&mut self) -> Result<u64, SubmitError> {
        let value = self.next_sequence;
        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(SubmitError::GenerationOverflow)?;
        Ok(value)
    }

    fn next_question_revision(&mut self) -> Result<u64, SubmitError> {
        let value = self.next_question_revision;
        self.next_question_revision = self
            .next_question_revision
            .checked_add(1)
            .ok_or(SubmitError::GenerationOverflow)?;
        Ok(value)
    }

    fn call_exists(&self, call_id: &CallId) -> bool {
        self.active.contains_key(call_id)
            || self
                .queue
                .iter()
                .any(|queued| &queued.submission.request.call_id == call_id)
            || self.awaiting_final_apply.contains_key(call_id)
            || self.ad_hoc.iter().any(|item| &item.call_id == call_id)
    }

    pub(crate) fn question_summaries(&self) -> Vec<QuestionSummaryDto> {
        self.ad_hoc
            .iter()
            .map(|item| QuestionSummaryDto {
                call_id: item.call_id.clone(),
                as_of_revision: item.as_of_revision,
                status: item.status,
                error: item.error,
                usage: item.usage,
                cost: item.cost.clone(),
            })
            .collect()
    }

    pub(crate) fn question_content(&self, call_id: &CallId) -> Option<QuestionContent<'_>> {
        self.ad_hoc
            .iter()
            .find(|item| &item.call_id == call_id)
            .map(|item| QuestionContent {
                question: item.question.as_str(),
                answer: item.answer.as_ref().map(SensitiveText::as_str),
            })
    }

    pub(crate) fn pinned_run_count(&self) -> u64 {
        self.pinned.run_count
    }

    /// Tick deterministic deadlines/debounces. Vertex resolution itself is driven separately so callers can
    /// hand the opaque attempt to an injected ADC worker.
    pub(crate) fn tick(&mut self) {
        let now = self.clock.monotonic_micros();
        self.maybe_queue_pinned(now);
        self.expire_queued(now);
        self.expire_active(now);
    }

    fn meaningful_pinned_progress(&self, watermark: TranscriptWatermark) -> bool {
        let baseline = self.pinned.request_watermark;
        watermark.finalized_rows > baseline.finalized_rows
            && (watermark
                .finalized_word_tokens
                .saturating_sub(baseline.finalized_word_tokens)
                >= PINNED_WORD_THRESHOLD
                || watermark
                    .covered_speech_ms
                    .saturating_sub(baseline.covered_speech_ms)
                    >= PINNED_SPEECH_THRESHOLD_MS)
    }

    fn maybe_queue_pinned(&mut self, now: u64) {
        if !self.control.snapshot().enabled_for(Lane::PinnedQuestion)
            || !self.control.snapshot().pinned_auto_enabled
            || self.pinned_template.is_none()
            || self.active_lane_count(Lane::PinnedQuestion) > 0
            || self
                .queue
                .iter()
                .any(|queued| queued.submission.request.lane == Lane::PinnedQuestion)
        {
            return;
        }
        let Some(due) = self.pinned.quiet_due_at_micros else {
            return;
        };
        if now < due {
            return;
        }
        let Some(candidate) = self.pinned.candidate.take() else {
            return;
        };
        if !self.meaningful_pinned_progress(candidate.watermark) {
            return;
        }
        let question_revision = self.control.snapshot().pinned_question_revision;
        if self
            .submit_auto(
                candidate.submission,
                candidate.watermark,
                Lane::PinnedQuestion,
                0,
                Some(question_revision),
            )
            .is_ok()
        {
            self.pinned.quiet_due_at_micros = None;
            self.pinned.dirty_while_running = false;
        }
    }

    pub(crate) fn drive_vertex(&mut self) -> Option<VertexResolutionAttempt> {
        let attempt = self.vertex.drive()?;
        self.publish_vertex_state();
        Some(attempt)
    }

    pub(crate) fn complete_vertex(
        &mut self,
        attempt: VertexResolutionAttempt,
        outcome: VertexResolutionOutcome,
    ) -> Result<(), CoordinatorError> {
        let update = self.vertex.complete(attempt, outcome)?;
        self.publish_vertex_state();
        if matches!(update.state, VertexCredentialState::Ready { .. }) {
            let now = self.clock.monotonic_micros();
            let mut expired_calls = Vec::new();
            let mut catching_up = Vec::new();
            for queued in &mut self.queue {
                if queued.stage != QueueStage::WaitingVertex {
                    continue;
                }
                if queued.submission.request.lane == Lane::AdHocQuestion {
                    queued.stage = QueueStage::ReadyForCatalog;
                    continue;
                }
                let retained = update.catch_up.iter().any(|pending| {
                    pending.sequence() == queued.sequence
                        || (queued.submission.request.lane == Lane::Final
                            && pending.lane() == Lane::Final
                            && pending.fence() == &queued.submission.request.fence)
                });
                if retained && !queued.submission.request.deadline.is_expired_at(now) {
                    // The Vertex resolver intentionally keeps one newest slot per automatic lane. Every
                    // chunk in one final group shares the exact fence, so retain the complete group rather
                    // than dispatching only the last sequence and dropping otherwise valid chunks.
                    queued.stage = QueueStage::ReadyForCatalog;
                    catching_up.push((
                        queued.submission.request.lane,
                        ui_fence(&queued.submission.request.fence),
                    ));
                } else {
                    expired_calls.push(queued.submission.request.call_id.clone());
                }
            }
            for (lane, fence) in catching_up {
                self.push_event(CoordinatorEventDto::LaneState(LaneStateEventDto {
                    lane,
                    state: LaneStateDto::CatchingUp,
                    code: None,
                    fence,
                }));
            }
            for call_id in expired_calls {
                self.fail_queued_call(&call_id, ErrorCode::Timeout);
            }
        } else if let VertexCredentialState::Rejected { .. } = update.state {
            self.fail_waiting_vertex(ErrorCode::AuthRejected);
        } else if let VertexCredentialState::Error { code, .. } = update.state {
            self.fail_waiting_vertex(code);
        }
        Ok(())
    }

    pub(crate) fn mark_vertex_token_lost(&mut self) {
        self.vertex.mark_token_lost();
        self.publish_vertex_state();
    }

    fn fail_waiting_vertex(&mut self, code: ErrorCode) {
        let calls: Vec<CallId> = self
            .queue
            .iter()
            .filter(|queued| queued.stage == QueueStage::WaitingVertex)
            .map(|queued| queued.submission.request.call_id.clone())
            .collect();
        for call_id in calls {
            self.fail_queued_call(&call_id, code);
        }
    }

    fn publish_vertex_state(&mut self) {
        let descriptor = corti_postprocess::KnownTransport::VertexDirect.descriptor();
        let state = ProviderStateDto {
            descriptor: descriptor.clone(),
            credential: self.vertex.state().credential_state(),
            models: self
                .provider_states
                .get(&(descriptor.provider.clone(), descriptor.transport.clone()))
                .map_or_else(Vec::new, |state| state.models.clone()),
            service_error: self
                .provider_states
                .get(&(descriptor.provider.clone(), descriptor.transport.clone()))
                .and_then(|state| state.service_error),
        };
        self.provider_states.insert(
            (descriptor.provider.clone(), descriptor.transport.clone()),
            state.clone(),
        );
        self.push_event(CoordinatorEventDto::ProviderState(Box::new(state)));
    }

    /// Clear account/project-scoped catalog data after Settings changes a connection scope. Credential
    /// readiness may remain valid (notably Vertex ADC), but model availability must be refreshed for the
    /// new exact scope before another Settings selection is accepted.
    pub(crate) fn invalidate_provider_scope(
        &mut self,
        provider: &ProviderId,
        transport: &TransportId,
    ) {
        let key = (provider.clone(), transport.clone());
        if let Some(state) = self.provider_states.get_mut(&key) {
            state.models.clear();
            state.service_error = None;
            let state = state.clone();
            self.push_event(CoordinatorEventDto::ProviderState(Box::new(state)));
        }
    }

    /// Re-check direct credentials only after an injected auth manager reports a change. There is no ambient
    /// polling; Vertex's exact five-second cadence is handled by its dedicated resolver above.
    pub(crate) fn notify_credentials_changed(
        &mut self,
        provider: &ProviderId,
        transport: &TransportId,
    ) {
        if let Some(state) = self
            .provider_states
            .get_mut(&(provider.clone(), transport.clone()))
        {
            state.credential = CredentialState::Resolving;
            state.service_error = None;
            let state = state.clone();
            self.push_event(CoordinatorEventDto::ProviderState(Box::new(state)));
        }
        let mut ad_hoc_calls = Vec::new();
        for queued in &mut self.queue {
            if queued.stage == QueueStage::WaitingCredential
                && &queued.submission.request.provider == provider
                && &queued.submission.request.transport == transport
            {
                queued.stage = QueueStage::NeedsAuth;
                if queued.submission.request.lane == Lane::AdHocQuestion {
                    ad_hoc_calls.push(queued.submission.request.call_id.clone());
                }
            }
        }
        for call_id in ad_hoc_calls {
            self.set_question_status(&call_id, QuestionStatusDto::Queued, None);
        }
    }

    /// Perform one scheduler step. Repeated calls drain ready work; each returned ticket must go to a bounded
    /// provider worker rather than execute on this coordinator or a capture/ASR thread.
    pub(crate) fn dispatch_next(&mut self) -> DispatchOutcome {
        self.tick();
        let now = self.clock.monotonic_micros();
        let Some(index) = self.select_candidate(now) else {
            return if self.queue.iter().any(|queued| {
                now >= queued.eligible_at_micros
                    && !matches!(
                        queued.stage,
                        QueueStage::WaitingCredential | QueueStage::WaitingVertex
                    )
            }) {
                DispatchOutcome::Backpressured
            } else if self.queue.is_empty() {
                DispatchOutcome::Empty
            } else {
                DispatchOutcome::Waiting
            };
        };
        let mut queued = self
            .queue
            .remove(index)
            .expect("selected queue index exists");
        let call_id = queued.submission.request.call_id.clone();
        let lane = queued.submission.request.lane;

        if !self
            .control
            .fence_controls_are_current(lane, &queued.submission.request.fence)
        {
            self.finish_queued_failure(&queued, ErrorCode::Superseded);
            return DispatchOutcome::Failed {
                call_id,
                code: ErrorCode::Superseded,
            };
        }

        if lane == Lane::Final && !queued.final_prepared {
            let boundary = final_boundary(&queued.submission);
            if self.store.prepare_final(&boundary).is_err() {
                self.finish_queued_failure(&queued, ErrorCode::Cache);
                return DispatchOutcome::Failed {
                    call_id,
                    code: ErrorCode::Cache,
                };
            }
            queued.final_prepared = true;
        }

        if queued.stage == QueueStage::NeedsCache {
            match self.store.lookup_exact(queued.submission.request_key) {
                Ok(ExactLookup::Hit(output)) => {
                    return self.finish_cache_hit(queued, output);
                }
                Ok(ExactLookup::Corrupt) => {
                    if self
                        .store
                        .evict_corrupt(queued.submission.request_key)
                        .is_err()
                    {
                        self.finish_queued_failure(&queued, ErrorCode::Cache);
                        return DispatchOutcome::Failed {
                            call_id,
                            code: ErrorCode::Cache,
                        };
                    }
                    queued.stage = QueueStage::NeedsAuth;
                }
                Ok(ExactLookup::Miss) => queued.stage = QueueStage::NeedsAuth,
                Err(_) => {
                    self.finish_queued_failure(&queued, ErrorCode::Cache);
                    return DispatchOutcome::Failed {
                        call_id,
                        code: ErrorCode::Cache,
                    };
                }
            }
        }

        if queued.stage == QueueStage::NeedsAuth {
            if is_vertex(&queued.submission.request) {
                if lane.is_automatic() {
                    let pending = VertexAutoPending::new(
                        lane,
                        queued.sequence,
                        queued.submission.request.fence.clone(),
                        queued.submission.request.deadline,
                    )
                    .expect("automatic lane accepted by Vertex pending");
                    let intent = self.vertex.intend_auto_dispatch(pending);
                    if let Some(warning) = intent.warning {
                        self.push_event(CoordinatorEventDto::Notice(NoticeDto {
                            role: warning.role(),
                            visible_message: warning.visible_message(),
                            episode: warning.episode(),
                        }));
                    }
                    match intent.disposition {
                        VertexDispatchDisposition::DispatchNow => {
                            queued.stage = QueueStage::ReadyForCatalog;
                        }
                        VertexDispatchDisposition::Arming(_)
                        | VertexDispatchDisposition::WaitingForRefresh(_) => {
                            queued.stage = QueueStage::WaitingVertex;
                            self.push_event(CoordinatorEventDto::LaneState(LaneStateEventDto {
                                lane,
                                state: LaneStateDto::Arming,
                                code: None,
                                fence: ui_fence(&queued.submission.request.fence),
                            }));
                            self.queue.push_back(queued);
                            return DispatchOutcome::Waiting;
                        }
                        VertexDispatchDisposition::Blocked(code) => {
                            self.finish_queued_failure(&queued, code);
                            return DispatchOutcome::Failed { call_id, code };
                        }
                    }
                } else {
                    if let Some(warning) = self.vertex.observe_dispatch_intent() {
                        self.push_event(CoordinatorEventDto::Notice(NoticeDto {
                            role: warning.role(),
                            visible_message: warning.visible_message(),
                            episode: warning.episode(),
                        }));
                    }
                    match self.vertex.state() {
                        VertexCredentialState::Ready { .. } => {
                            queued.stage = QueueStage::ReadyForCatalog;
                        }
                        VertexCredentialState::Unarmed { .. }
                        | VertexCredentialState::Resolving { .. }
                        | VertexCredentialState::Refreshing { .. } => {
                            queued.stage = QueueStage::WaitingVertex;
                            self.set_question_status(
                                &call_id,
                                QuestionStatusDto::WaitingForCredential,
                                None,
                            );
                            self.queue.push_back(queued);
                            return DispatchOutcome::Waiting;
                        }
                        VertexCredentialState::Rejected { .. } => {
                            self.finish_queued_failure(&queued, ErrorCode::AuthRejected);
                            return DispatchOutcome::Failed {
                                call_id,
                                code: ErrorCode::AuthRejected,
                            };
                        }
                        VertexCredentialState::Error { code, .. } => {
                            self.finish_queued_failure(&queued, code);
                            return DispatchOutcome::Failed { call_id, code };
                        }
                    }
                }
            } else {
                let provider_key = (
                    queued.submission.request.provider.clone(),
                    queued.submission.request.transport.clone(),
                );
                let credential = if self
                    .provider_states
                    .get(&provider_key)
                    .is_some_and(|state| state.credential == CredentialState::Rejected)
                {
                    CredentialState::Rejected
                } else {
                    self.providers.credential_state(
                        &queued.submission.request.provider,
                        &queued.submission.request.transport,
                    )
                };
                self.publish_provider_credential(&queued, credential.clone());
                match credential {
                    CredentialState::Ready { .. } => queued.stage = QueueStage::ReadyForCatalog,
                    CredentialState::Absent
                    | CredentialState::Resolving
                    | CredentialState::AwaitingUser
                    | CredentialState::DeviceAuthorization { .. }
                    | CredentialState::Refreshing => {
                        queued.stage = QueueStage::WaitingCredential;
                        if lane.is_question() {
                            self.set_question_status(
                                &call_id,
                                QuestionStatusDto::WaitingForCredential,
                                None,
                            );
                        }
                        self.push_event(CoordinatorEventDto::LaneState(LaneStateEventDto {
                            lane,
                            state: LaneStateDto::Arming,
                            code: None,
                            fence: ui_fence(&queued.submission.request.fence),
                        }));
                        self.queue.push_back(queued);
                        return DispatchOutcome::Waiting;
                    }
                    CredentialState::Rejected => {
                        self.finish_queued_failure(&queued, ErrorCode::AuthRejected);
                        return DispatchOutcome::Failed {
                            call_id,
                            code: ErrorCode::AuthRejected,
                        };
                    }
                    CredentialState::Unsupported { code } | CredentialState::Error { code } => {
                        self.finish_queued_failure(&queued, code);
                        return DispatchOutcome::Failed { call_id, code };
                    }
                }
            }
        }

        if queued.stage == QueueStage::ReadyForCatalog {
            let catalog = self.providers.catalog(
                &queued.submission.request.provider,
                &queued.submission.request.transport,
                &queued.submission.scope,
            );
            let catalog = match catalog {
                Ok(catalog) => catalog,
                Err(error) => {
                    self.publish_service_error(&queued, error.code);
                    self.finish_queued_failure(&queued, error.code);
                    return DispatchOutcome::Failed {
                        call_id,
                        code: error.code,
                    };
                }
            };
            self.publish_catalog(&queued, &catalog);
            if let Err(code) = validate_exact_model(&queued, &catalog) {
                self.publish_service_error(&queued, code);
                self.finish_queued_failure(&queued, code);
                return DispatchOutcome::Failed { call_id, code };
            }
        }

        if !self.has_provider_capacity(&queued) {
            self.queue.push_back(queued);
            return DispatchOutcome::Backpressured;
        }

        if lane == Lane::Live {
            queued.submission.request.deadline =
                MonotonicDeadline(now.saturating_add(LIVE_TERMINAL_DEADLINE_MICROS));
        }
        if lane == Lane::Final {
            // Persist the ambiguous-dispatch boundary before handing any ticket to a transport thread. A
            // crash after this point may require explicit retry even if the OS never scheduled the worker,
            // but it can never silently repeat a paid request whose body may have left the process.
            let boundary = final_boundary(&queued.submission);
            if self.store.mark_final_dispatched(&boundary).is_err() {
                self.finish_queued_failure(&queued, ErrorCode::Cache);
                return DispatchOutcome::Failed {
                    call_id,
                    code: ErrorCode::Cache,
                };
            }
        }
        let cancel = CancellationToken::new();
        let submission = Arc::new(queued.submission);
        let context = request_context(&submission.request);
        let final_boundary = (lane == Lane::Final).then(|| final_boundary(&submission));
        self.active.insert(
            call_id.clone(),
            ActiveCall {
                sequence: queued.sequence,
                recording_id: submission.recording_id.clone(),
                context,
                cancel: cancel.clone(),
                descriptor: queued.descriptor.clone(),
                final_boundary,
                provider_request_sent: false,
                dispatch_started_at_micros: None,
                dispatched_at_unix_ms: None,
                first_text_seen: false,
                observed_terminal_usage: None,
                model: submission.request.model.clone(),
                region: submission.scope.region.clone(),
                deadline: submission.request.deadline,
            },
        );
        if lane == Lane::PinnedQuestion {
            self.pinned.request_watermark = queued.watermark;
            self.pinned.run_count = self.pinned.run_count.saturating_add(1);
            self.pinned.dirty_while_running = false;
        }
        if lane == Lane::AdHocQuestion {
            self.set_question_status(&call_id, QuestionStatusDto::Running, None);
        }
        self.push_event(CoordinatorEventDto::LaneState(LaneStateEventDto {
            lane,
            state: match lane {
                Lane::Final => LaneStateDto::Finalizing,
                _ => LaneStateDto::Rewriting,
            },
            code: None,
            fence: ui_fence(&submission.request.fence),
        }));
        DispatchOutcome::Ticket(DispatchTicket {
            sequence: queued.sequence,
            call: submission,
            descriptor: queued.descriptor,
            cancel,
            queued_at_micros: queued.queued_at_micros,
            queued_at_unix_ms: queued.queued_at_unix_ms,
        })
    }

    fn select_candidate(&self, now: u64) -> Option<usize> {
        let promoted_final = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, queued)| {
                queued.submission.request.lane == Lane::Final
                    && now.saturating_sub(queued.queued_at_micros) >= FINAL_PROMOTION_MICROS
                    && self.candidate_eligible(queued, now)
            })
            .min_by_key(|(_, queued)| queued.sequence)
            .map(|(index, _)| index);
        if promoted_final.is_some() {
            return promoted_final;
        }
        [
            Lane::Live,
            Lane::AdHocQuestion,
            Lane::PinnedQuestion,
            Lane::Final,
        ]
        .into_iter()
        .find_map(|lane| {
            self.queue
                .iter()
                .enumerate()
                .filter(|(_, queued)| {
                    queued.submission.request.lane == lane && self.candidate_eligible(queued, now)
                })
                .min_by_key(|(_, queued)| queued.sequence)
                .map(|(index, _)| index)
        })
    }

    fn candidate_eligible(&self, queued: &QueuedCall, now: u64) -> bool {
        if now < queued.eligible_at_micros
            || matches!(
                queued.stage,
                QueueStage::WaitingCredential | QueueStage::WaitingVertex
            )
        {
            return false;
        }
        let lane = queued.submission.request.lane;
        let lane_limit_reached = match lane {
            Lane::Live | Lane::PinnedQuestion | Lane::AdHocQuestion => {
                self.active_lane_count(lane) >= 1
            }
            Lane::Final => self.active_lane_count(lane) >= MAX_FINAL_CALLS,
        };
        if lane_limit_reached {
            return false;
        }
        if matches!(queued.stage, QueueStage::NeedsCache | QueueStage::NeedsAuth) {
            return true;
        }
        self.has_provider_capacity(queued)
    }

    fn has_provider_capacity(&self, queued: &QueuedCall) -> bool {
        if self.active.len() >= MAX_GLOBAL_PROVIDER_CALLS {
            return false;
        }
        let provider_count = self
            .active
            .values()
            .filter(|active| {
                active.descriptor.provider == queued.descriptor.provider
                    && active.descriptor.transport == queued.descriptor.transport
            })
            .count();
        if provider_count >= MAX_PROVIDER_CALLS {
            return false;
        }
        // Final work cannot occupy the reserved fourth interactive slot.
        queued.submission.request.lane != Lane::Final
            || (self.active.len() < MAX_GLOBAL_PROVIDER_CALLS - 1
                && self.active_lane_count(Lane::Final) < MAX_FINAL_CALLS)
    }

    fn active_lane_count(&self, lane: Lane) -> usize {
        self.active
            .values()
            .filter(|active| active.context.lane == lane)
            .count()
    }

    fn finish_cache_hit(&mut self, queued: QueuedCall, output: ProviderOutput) -> DispatchOutcome {
        let call_id = queued.submission.request.call_id.clone();
        let cache_output = output.clone();
        let validated = match validate_output(&queued.submission, output) {
            Ok(output) => output,
            Err(code) => {
                let _ = self.store.evict_corrupt(queued.submission.request_key);
                self.finish_queued_failure(&queued, code);
                return DispatchOutcome::Failed { call_id, code };
            }
        };
        let telemetry = self.telemetry_for_queued(
            &queued,
            TerminalOutcomeDto::Completed,
            None,
            false,
            false,
            CacheObservation::Local,
            NormalizedUsage::unknown(),
            CostEstimate::no_provider_request(),
            LatencyFields::default(),
            None,
        );
        let final_boundary = (queued.submission.request.lane == Lane::Final)
            .then(|| final_boundary(&queued.submission));
        let store_result = if let Some(boundary) = final_boundary.as_ref() {
            self.store.commit_validated(StoreCommit {
                request_key: queued.submission.request_key,
                lane: queued.submission.request.lane,
                local_cache_mode: queued.submission.request.cache_policy.local,
                cache_output: &cache_output,
                output: &validated,
                final_boundary: Some(boundary),
                telemetry: &telemetry,
            })
        } else {
            self.store.record_terminal(&telemetry)
        };
        if store_result.is_err() {
            self.finish_queued_failure(&queued, ErrorCode::Cache);
            return DispatchOutcome::Failed {
                call_id,
                code: ErrorCode::Cache,
            };
        }
        self.push_terminal_events(&telemetry, AccountingFinalityDto::Final);
        self.finish_question_success(&call_id, &validated, &telemetry);
        if let Some(boundary) = final_boundary {
            self.awaiting_final_apply.insert(call_id.clone(), boundary);
        }
        self.push_event(CoordinatorEventDto::LaneState(LaneStateEventDto {
            lane: queued.submission.request.lane,
            state: LaneStateDto::Clean,
            code: None,
            fence: ui_fence(&queued.submission.request.fence),
        }));
        DispatchOutcome::CacheApply(ApplyReady {
            call_id,
            lane: queued.submission.request.lane,
            fence: queued.submission.request.fence.clone(),
            output: validated,
            recovery_committed: queued.submission.request.lane == Lane::Final,
        })
    }

    pub(crate) fn on_provider_event(
        &mut self,
        event: ProviderEvent,
    ) -> Result<(), CoordinatorError> {
        let call_id = event.context.call_id.clone();
        let Some(active) = self.active.get_mut(&call_id) else {
            return Err(CoordinatorError::UnknownCall);
        };
        if active.context != event.context {
            return Err(CoordinatorError::EventFenceMismatch);
        }
        match event.kind {
            ProviderEventKind::DispatchStarted => {
                active.provider_request_sent = true;
                active.dispatch_started_at_micros = Some(self.clock.monotonic_micros());
                active.dispatched_at_unix_ms = Some(self.clock.unix_millis());
            }
            ProviderEventKind::FirstText | ProviderEventKind::TextDelta(_) => {
                if !active.cancel.is_cancelled() {
                    active.first_text_seen = true;
                }
            }
            ProviderEventKind::UsageProvisional(usage) => {
                if !active.cancel.is_cancelled() {
                    let cost = estimate_cost(
                        self.pricing.as_ref(),
                        &active.descriptor,
                        &active.model,
                        active.region.as_deref(),
                        active.dispatched_at_unix_ms,
                        active.provider_request_sent,
                        CacheObservation::None,
                        &usage,
                    );
                    let recording_id = active.recording_id.clone();
                    let fence = event.context.fence.clone();
                    self.push_event(CoordinatorEventDto::Accounting(Box::new(
                        AccountingEventDto {
                            call_id,
                            recording_id,
                            lane: event.context.lane,
                            fence,
                            finality: AccountingFinalityDto::Provisional,
                            usage,
                            cost,
                            late: false,
                        },
                    )));
                }
            }
            ProviderEventKind::Completed(usage) => {
                active.observed_terminal_usage = Some(usage);
            }
            ProviderEventKind::Canceled { terminal_usage, .. }
            | ProviderEventKind::Failed { terminal_usage, .. } => {
                if terminal_usage.is_some() {
                    active.observed_terminal_usage = terminal_usage;
                }
            }
            ProviderEventKind::Queued
            | ProviderEventKind::AuthWaiting
            | ProviderEventKind::Headers
            | ProviderEventKind::CacheObserved(_) => {}
        }
        Ok(())
    }

    pub(crate) fn complete(
        &mut self,
        ticket: DispatchTicket,
        result: Result<corti_postprocess::ProviderTerminal, PostprocessError>,
    ) -> CompletionOutcome {
        let call_id = ticket.call.request.call_id.clone();
        let lane = ticket.call.request.lane;
        let active = self.active.remove(&call_id);
        let (provider_sent, dispatched_at, observed_usage, canceled_reason, final_boundary) =
            if let Some(active) = active {
                if active.sequence != ticket.sequence {
                    return CompletionOutcome::Discarded {
                        call_id,
                        code: ErrorCode::Superseded,
                    };
                }
                (
                    active.provider_request_sent || result.is_ok(),
                    active.dispatched_at_unix_ms,
                    active.observed_terminal_usage,
                    active.cancel.reason(),
                    active.final_boundary,
                )
            } else {
                (
                    result.is_ok(),
                    None,
                    None,
                    Some(CancellationReason::Superseded),
                    (lane == Lane::Final).then(|| final_boundary(&ticket.call)),
                )
            };

        let controls_current = self
            .control
            .fence_controls_are_current(lane, &ticket.call.request.fence);
        let deadline_expired = ticket
            .call
            .request
            .deadline
            .is_expired_at(self.clock.monotonic_micros());
        let stale_reason = canceled_reason
            .or_else(|| (!controls_current).then_some(CancellationReason::Superseded))
            .or_else(|| deadline_expired.then_some(CancellationReason::Deadline));

        let (terminal_usage, latency, cache, output, provider_error) = match result {
            Ok(terminal) => (
                terminal.usage,
                terminal.latency,
                terminal.cache,
                Some(terminal.output),
                None,
            ),
            Err(error) => (
                observed_usage.unwrap_or_else(NormalizedUsage::unknown),
                LatencyFields::default(),
                CacheObservation::None,
                None,
                Some(error.code),
            ),
        };
        let usage = if terminal_usage == NormalizedUsage::unknown() {
            observed_usage.unwrap_or(terminal_usage)
        } else {
            terminal_usage
        };
        let cost = estimate_cost(
            self.pricing.as_ref(),
            &ticket.descriptor,
            &ticket.call.request.model,
            ticket.call.scope.region.as_deref(),
            dispatched_at,
            provider_sent,
            cache,
            &usage,
        );

        if let Some(reason) = stale_reason {
            let (outcome, code) = cancellation_outcome(reason);
            let telemetry = self.telemetry_for_ticket(
                &ticket,
                outcome,
                Some(code),
                provider_sent,
                true,
                cache,
                usage,
                cost,
                latency,
                dispatched_at,
            );
            if let Some(boundary) = final_boundary.as_ref() {
                let _ = self.store.abandon_final(boundary);
            }
            let _ = self.store.record_terminal(&telemetry);
            self.push_terminal_events(&telemetry, AccountingFinalityDto::Final);
            self.finish_question_failure(&call_id, code, &telemetry);
            self.after_lane_completion(lane);
            return CompletionOutcome::Discarded { call_id, code };
        }

        if let Some(code) = provider_error {
            if code == ErrorCode::AuthRejected {
                self.reject_provider_credential(&ticket);
            }
            let telemetry = self.telemetry_for_ticket(
                &ticket,
                TerminalOutcomeDto::Failed,
                Some(code),
                provider_sent,
                false,
                cache,
                usage,
                cost,
                latency,
                dispatched_at,
            );
            if let Some(boundary) = final_boundary.as_ref() {
                let _ = self.store.abandon_final(boundary);
            }
            let _ = self.store.record_terminal(&telemetry);
            self.push_terminal_events(&telemetry, AccountingFinalityDto::Final);
            self.finish_question_failure(&call_id, code, &telemetry);
            self.push_event(CoordinatorEventDto::LaneState(LaneStateEventDto {
                lane,
                state: LaneStateDto::Failed,
                code: Some(code),
                fence: ui_fence(&ticket.call.request.fence),
            }));
            self.after_lane_completion(lane);
            return CompletionOutcome::Failed { call_id, code };
        }

        let output = output.expect("successful terminal has output");
        let cache_output = output.clone();
        let validated = match validate_output(&ticket.call, output) {
            Ok(output) => output,
            Err(code) => {
                let telemetry = self.telemetry_for_ticket(
                    &ticket,
                    TerminalOutcomeDto::Failed,
                    Some(code),
                    provider_sent,
                    false,
                    cache,
                    usage,
                    cost,
                    latency,
                    dispatched_at,
                );
                if let Some(boundary) = final_boundary.as_ref() {
                    let _ = self.store.abandon_final(boundary);
                }
                let _ = self.store.record_terminal(&telemetry);
                self.push_terminal_events(&telemetry, AccountingFinalityDto::Final);
                self.finish_question_failure(&call_id, code, &telemetry);
                self.after_lane_completion(lane);
                return CompletionOutcome::Failed { call_id, code };
            }
        };
        let telemetry = self.telemetry_for_ticket(
            &ticket,
            TerminalOutcomeDto::Completed,
            None,
            provider_sent,
            false,
            cache,
            usage,
            cost,
            latency,
            dispatched_at,
        );
        let commit = self.store.commit_validated(StoreCommit {
            request_key: ticket.call.request_key,
            lane,
            local_cache_mode: ticket.call.request.cache_policy.local,
            cache_output: &cache_output,
            output: &validated,
            final_boundary: final_boundary.as_ref(),
            telemetry: &telemetry,
        });
        if commit.is_err() {
            if let Some(boundary) = final_boundary.as_ref() {
                let _ = self.store.abandon_final(boundary);
            }
            self.finish_question_failure(&call_id, ErrorCode::Cache, &telemetry);
            self.after_lane_completion(lane);
            return CompletionOutcome::Failed {
                call_id,
                code: ErrorCode::Cache,
            };
        }
        self.push_terminal_events(&telemetry, AccountingFinalityDto::Final);
        self.finish_question_success(&call_id, &validated, &telemetry);
        if let Some(boundary) = final_boundary {
            self.awaiting_final_apply.insert(call_id.clone(), boundary);
        }
        self.push_event(CoordinatorEventDto::LaneState(LaneStateEventDto {
            lane,
            state: LaneStateDto::Clean,
            code: None,
            fence: ui_fence(&ticket.call.request.fence),
        }));
        self.after_lane_completion(lane);
        CompletionOutcome::Apply(ApplyReady {
            call_id,
            lane,
            fence: ticket.call.request.fence.clone(),
            output: validated,
            recovery_committed: lane == Lane::Final,
        })
    }

    fn after_lane_completion(&mut self, lane: Lane) {
        if lane == Lane::PinnedQuestion && self.pinned.dirty_while_running {
            self.pinned.dirty_while_running = false;
            if self.meaningful_pinned_progress(self.watermark) {
                self.pinned.quiet_due_at_micros = Some(
                    self.last_progress_at_micros
                        .saturating_add(PINNED_QUIET_DEBOUNCE_MICROS),
                );
            }
        }
        self.trim_ad_hoc();
    }

    pub(crate) fn application_is_current(&self, apply: &ApplyReady) -> bool {
        self.control
            .fence_controls_are_current(apply.lane, &apply.fence)
    }

    pub(crate) fn cancel_call(&mut self, call_id: &CallId, reason: CancellationReason) -> bool {
        if let Some(active) = self.active.get(call_id) {
            let canceled = active.cancel.cancel(reason);
            if canceled && let Some(boundary) = active.final_boundary.as_ref() {
                let _ = self.store.abandon_final(boundary);
            }
            return canceled;
        }
        if let Some(index) = self
            .queue
            .iter()
            .position(|queued| &queued.submission.request.call_id == call_id)
        {
            let queued = self.queue.remove(index).expect("queue index exists");
            self.finish_queued_canceled(&queued, reason);
            return true;
        }
        if let Some(boundary) = self.awaiting_final_apply.remove(call_id) {
            let _ = self.store.abandon_final(&boundary);
            return true;
        }
        false
    }

    pub(crate) fn mark_final_applied(&mut self, call_id: &CallId) -> Result<(), CoordinatorError> {
        self.mark_final_group_applied(std::slice::from_ref(call_id))
    }

    /// Validate every chunk fence before moving any journal row to Applied. Final chunking is
    /// all-or-nothing; a stale member must not leave a partially-applied journal group.
    pub(crate) fn mark_final_group_applied(
        &mut self,
        call_ids: &[CallId],
    ) -> Result<(), CoordinatorError> {
        let mut boundaries = Vec::with_capacity(call_ids.len());
        for call_id in call_ids {
            boundaries.push(
                self.awaiting_final_apply
                    .get(call_id)
                    .cloned()
                    .ok_or(CoordinatorError::UnknownCall)?,
            );
        }
        if boundaries.iter().any(|boundary| {
            !self
                .control
                .fence_controls_are_current(Lane::Final, &boundary.fence)
        }) {
            for boundary in &boundaries {
                let _ = self.store.abandon_final(boundary);
            }
            return Err(CoordinatorError::StaleApplication);
        }
        self.store
            .mark_final_group_applied(&boundaries)
            .map_err(|_| CoordinatorError::Store)
    }

    pub(crate) fn mark_final_checkpointed(
        &mut self,
        call_id: &CallId,
    ) -> Result<(), CoordinatorError> {
        self.mark_final_group_checkpointed(std::slice::from_ref(call_id))
    }

    pub(crate) fn mark_final_group_checkpointed(
        &mut self,
        call_ids: &[CallId],
    ) -> Result<(), CoordinatorError> {
        let mut boundaries = Vec::with_capacity(call_ids.len());
        for call_id in call_ids {
            boundaries.push(
                self.awaiting_final_apply
                    .get(call_id)
                    .cloned()
                    .ok_or(CoordinatorError::UnknownCall)?,
            );
        }
        self.store
            .mark_final_group_checkpointed(&boundaries)
            .map_err(|_| CoordinatorError::Store)?;
        for call_id in call_ids {
            self.awaiting_final_apply.remove(call_id);
        }
        Ok(())
    }

    pub(crate) fn recover_final(
        &mut self,
        recording_id: &str,
    ) -> Result<FinalRecoveryDirective, CoordinatorError> {
        let record = self
            .store
            .recover_final(recording_id)
            .map_err(|_| CoordinatorError::Store)?;
        Ok(match record {
            None
            | Some(FinalRecoveryRecord {
                state: FinalJournalState::Checkpointed,
                ..
            }) => FinalRecoveryDirective::None,
            Some(FinalRecoveryRecord {
                call_id,
                state: FinalJournalState::Prepared,
            }) => FinalRecoveryDirective::ResumePrepared { call_id },
            Some(FinalRecoveryRecord {
                call_id,
                state: FinalJournalState::ResultCached,
            }) => FinalRecoveryDirective::ResumeEncryptedResult { call_id },
            Some(FinalRecoveryRecord {
                call_id,
                state: FinalJournalState::Applied,
            }) => FinalRecoveryDirective::ResumeCheckpoint { call_id },
            Some(FinalRecoveryRecord {
                call_id,
                state: FinalJournalState::Dispatched,
            }) => FinalRecoveryDirective::Fallback {
                call_id,
                code: ErrorCode::AmbiguousDispatch,
                explicit_retry_required: true,
            },
            Some(FinalRecoveryRecord {
                call_id,
                state: FinalJournalState::Abandoned,
            }) => FinalRecoveryDirective::Fallback {
                call_id,
                code: ErrorCode::Canceled,
                explicit_retry_required: false,
            },
        })
    }

    fn reject_provider_credential(&mut self, ticket: &DispatchTicket) {
        let key = (
            ticket.descriptor.provider.clone(),
            ticket.descriptor.transport.clone(),
        );
        let state = ProviderStateDto {
            descriptor: ticket.descriptor.clone(),
            credential: CredentialState::Rejected,
            models: Vec::new(),
            service_error: Some(ErrorCode::AuthRejected),
        };
        self.provider_states.insert(key, state.clone());
        self.push_event(CoordinatorEventDto::ProviderState(Box::new(state)));

        let queued_peers = self
            .queue
            .iter()
            .filter(|queued| {
                queued.descriptor.provider == ticket.descriptor.provider
                    && queued.descriptor.transport == ticket.descriptor.transport
            })
            .map(|queued| queued.submission.request.call_id.clone())
            .collect::<Vec<_>>();
        for call_id in queued_peers {
            self.fail_queued_call(&call_id, ErrorCode::AuthRejected);
        }
    }

    fn publish_provider_credential(&mut self, queued: &QueuedCall, credential: CredentialState) {
        let key = (
            queued.descriptor.provider.clone(),
            queued.descriptor.transport.clone(),
        );
        let state = ProviderStateDto {
            descriptor: queued.descriptor.clone(),
            credential,
            models: self
                .provider_states
                .get(&key)
                .map_or_else(Vec::new, |state| state.models.clone()),
            service_error: None,
        };
        self.provider_states.insert(key, state.clone());
        self.push_event(CoordinatorEventDto::ProviderState(Box::new(state)));
    }

    fn publish_catalog(&mut self, queued: &QueuedCall, catalog: &ModelCatalog) {
        let key = (
            queued.descriptor.provider.clone(),
            queued.descriptor.transport.clone(),
        );
        let credential = if is_vertex(&queued.submission.request) {
            self.vertex.state().credential_state()
        } else {
            self.provider_states
                .get(&key)
                .map_or(CredentialState::Resolving, |state| state.credential.clone())
        };
        let state = ProviderStateDto {
            descriptor: queued.descriptor.clone(),
            credential,
            models: catalog.models.clone(),
            service_error: None,
        };
        self.provider_states.insert(key, state.clone());
        self.push_event(CoordinatorEventDto::ProviderState(Box::new(state)));
    }

    fn publish_service_error(&mut self, queued: &QueuedCall, code: ErrorCode) {
        let key = (
            queued.descriptor.provider.clone(),
            queued.descriptor.transport.clone(),
        );
        let mut state =
            self.provider_states
                .get(&key)
                .cloned()
                .unwrap_or_else(|| ProviderStateDto {
                    descriptor: queued.descriptor.clone(),
                    credential: if is_vertex(&queued.submission.request) {
                        self.vertex.state().credential_state()
                    } else {
                        CredentialState::Resolving
                    },
                    models: Vec::new(),
                    service_error: None,
                });
        state.service_error = Some(code);
        self.provider_states.insert(key, state.clone());
        self.push_event(CoordinatorEventDto::ProviderState(Box::new(state)));
    }

    fn expire_queued(&mut self, now: u64) {
        let expired: Vec<CallId> = self
            .queue
            .iter()
            .filter(|queued| queued.submission.request.deadline.is_expired_at(now))
            .map(|queued| queued.submission.request.call_id.clone())
            .collect();
        for call_id in expired {
            self.fail_queued_call(&call_id, ErrorCode::Timeout);
        }
    }

    fn expire_active(&mut self, now: u64) {
        let mut cancellations = Vec::new();
        for (call_id, active) in &self.active {
            let live_first_text_expired = active.context.lane == Lane::Live
                && !active.first_text_seen
                && active.dispatch_started_at_micros.is_some_and(|started| {
                    now.saturating_sub(started) >= LIVE_FIRST_TEXT_DEADLINE_MICROS
                });
            if live_first_text_expired || active.deadline.is_expired_at(now) {
                cancellations.push(call_id.clone());
            }
        }
        for call_id in cancellations {
            if let Some(active) = self.active.get(&call_id) {
                active.cancel.cancel(CancellationReason::Deadline);
                if let Some(boundary) = active.final_boundary.as_ref() {
                    let _ = self.store.abandon_final(boundary);
                }
            }
        }
    }

    fn cancel_scope(&mut self, scope: CancellationScope) {
        let matches = |lane: Lane| match scope {
            CancellationScope::None => false,
            CancellationScope::All(_) => true,
            CancellationScope::Family(family, _) => family.contains(lane),
            CancellationScope::Pinned(_) => lane == Lane::PinnedQuestion,
        };
        let reason = match scope {
            CancellationScope::None => return,
            CancellationScope::All(reason)
            | CancellationScope::Family(_, reason)
            | CancellationScope::Pinned(reason) => reason,
        };
        for active in self.active.values() {
            if matches(active.context.lane) {
                active.cancel.cancel(reason);
                if let Some(boundary) = active.final_boundary.as_ref() {
                    let _ = self.store.abandon_final(boundary);
                }
            }
        }
        let mut retained = VecDeque::with_capacity(self.queue.len());
        let mut removed = Vec::new();
        while let Some(queued) = self.queue.pop_front() {
            if matches(queued.submission.request.lane) {
                removed.push(queued);
            } else {
                retained.push_back(queued);
            }
        }
        self.queue = retained;
        for queued in removed {
            self.finish_queued_canceled(&queued, reason);
        }
        if matches(Lane::PinnedQuestion) {
            self.pinned.candidate = None;
            self.pinned.quiet_due_at_micros = None;
        }
    }

    fn remove_queued_lane(&mut self, lane: Lane, reason: CancellationReason) {
        let mut retained = VecDeque::with_capacity(self.queue.len());
        let mut removed = Vec::new();
        while let Some(queued) = self.queue.pop_front() {
            if queued.submission.request.lane == lane {
                removed.push(queued);
            } else {
                retained.push_back(queued);
            }
        }
        self.queue = retained;
        for queued in removed {
            self.finish_queued_canceled(&queued, reason);
        }
    }

    fn fail_queued_call(&mut self, call_id: &CallId, code: ErrorCode) {
        let Some(index) = self
            .queue
            .iter()
            .position(|queued| &queued.submission.request.call_id == call_id)
        else {
            return;
        };
        let queued = self.queue.remove(index).expect("queue index exists");
        self.finish_queued_failure(&queued, code);
    }

    fn finish_queued_canceled(&mut self, queued: &QueuedCall, reason: CancellationReason) {
        let (outcome, code) = cancellation_outcome(reason);
        if let Some(boundary) = (queued.submission.request.lane == Lane::Final)
            .then(|| final_boundary(&queued.submission))
            .filter(|_| queued.final_prepared)
        {
            let _ = self.store.abandon_final(&boundary);
        }
        let telemetry = self.telemetry_for_queued(
            queued,
            outcome,
            Some(code),
            false,
            false,
            CacheObservation::None,
            NormalizedUsage::unknown(),
            CostEstimate::unavailable(),
            LatencyFields::default(),
            None,
        );
        let _ = self.store.record_terminal(&telemetry);
        self.push_terminal_events(&telemetry, AccountingFinalityDto::Final);
        self.finish_question_failure(&queued.submission.request.call_id, code, &telemetry);
    }

    fn finish_queued_failure(&mut self, queued: &QueuedCall, code: ErrorCode) {
        if let Some(boundary) = (queued.submission.request.lane == Lane::Final)
            .then(|| final_boundary(&queued.submission))
            .filter(|_| queued.final_prepared)
        {
            let _ = self.store.abandon_final(&boundary);
        }
        let outcome = match code {
            ErrorCode::Timeout => TerminalOutcomeDto::Timeout,
            ErrorCode::Canceled => TerminalOutcomeDto::Canceled,
            ErrorCode::Superseded => TerminalOutcomeDto::Superseded,
            _ => TerminalOutcomeDto::Failed,
        };
        let telemetry = self.telemetry_for_queued(
            queued,
            outcome,
            Some(code),
            false,
            false,
            CacheObservation::None,
            NormalizedUsage::unknown(),
            CostEstimate::unavailable(),
            LatencyFields::default(),
            None,
        );
        let _ = self.store.record_terminal(&telemetry);
        self.push_terminal_events(&telemetry, AccountingFinalityDto::Final);
        self.finish_question_failure(&queued.submission.request.call_id, code, &telemetry);
        self.push_event(CoordinatorEventDto::LaneState(LaneStateEventDto {
            lane: queued.submission.request.lane,
            state: LaneStateDto::Failed,
            code: Some(code),
            fence: ui_fence(&queued.submission.request.fence),
        }));
    }

    fn finish_question_success(
        &mut self,
        call_id: &CallId,
        output: &ValidatedOutput,
        telemetry: &TerminalTelemetryDto,
    ) {
        let Some(item) = self.ad_hoc.iter_mut().find(|item| &item.call_id == call_id) else {
            return;
        };
        item.status = QuestionStatusDto::Completed;
        item.error = None;
        item.usage = Some(telemetry.usage);
        item.cost = Some(telemetry.cost.clone());
        item.answer = output
            .answer()
            .map(|answer| SensitiveText::new(answer.to_owned()));
    }

    fn finish_question_failure(
        &mut self,
        call_id: &CallId,
        code: ErrorCode,
        telemetry: &TerminalTelemetryDto,
    ) {
        let status = if matches!(code, ErrorCode::Canceled | ErrorCode::Superseded) {
            QuestionStatusDto::Canceled
        } else {
            QuestionStatusDto::Failed
        };
        if let Some(item) = self.ad_hoc.iter_mut().find(|item| &item.call_id == call_id) {
            item.status = status;
            item.error = Some(code);
            item.usage = Some(telemetry.usage);
            item.cost = Some(telemetry.cost.clone());
        }
    }

    fn set_question_status(
        &mut self,
        call_id: &CallId,
        status: QuestionStatusDto,
        error: Option<ErrorCode>,
    ) {
        if let Some(item) = self.ad_hoc.iter_mut().find(|item| &item.call_id == call_id) {
            item.status = status;
            item.error = error;
        }
    }

    fn trim_ad_hoc(&mut self) {
        loop {
            let bytes: usize = self
                .ad_hoc
                .iter()
                .map(|item| {
                    item.question
                        .len()
                        .saturating_add(item.answer.as_ref().map_or(0, SensitiveText::len))
                })
                .sum();
            if self.ad_hoc.len() <= MAX_VISIBLE_AD_HOC_EXCHANGES
                && bytes <= MAX_VISIBLE_ASSISTANT_BYTES
            {
                break;
            }
            let Some(index) = self.ad_hoc.iter().position(|item| {
                !matches!(
                    item.status,
                    QuestionStatusDto::Queued
                        | QuestionStatusDto::WaitingForCredential
                        | QuestionStatusDto::Running
                )
            }) else {
                break;
            };
            self.ad_hoc.remove(index);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn telemetry_for_queued(
        &self,
        queued: &QueuedCall,
        outcome: TerminalOutcomeDto,
        error: Option<ErrorCode>,
        provider_request_sent: bool,
        late: bool,
        cache: CacheObservation,
        usage: NormalizedUsage,
        cost: CostEstimate,
        latency: LatencyFields,
        dispatched_at_unix_ms: Option<i64>,
    ) -> TerminalTelemetryDto {
        TerminalTelemetryDto {
            call_id: queued.submission.request.call_id.clone(),
            recording_id: queued.submission.recording_id.clone(),
            request_group_id: queued.submission.request.group_id.clone(),
            target_id: queued.submission.request.target_id.clone(),
            lane: queued.submission.request.lane,
            attempt_no: 1,
            fence: queued.submission.request.fence.clone(),
            provider: queued.submission.request.provider.clone(),
            transport: queued.submission.request.transport.clone(),
            model: queued.submission.request.model.clone(),
            support_tier: queued.descriptor.support_tier,
            adapter_version: queued.submission.adapter_version,
            prompt_version: corti_postprocess::PROMPT_TEMPLATE_VERSION,
            output_schema_version: corti_postprocess::OUTPUT_SCHEMA_VERSION,
            outcome,
            error,
            provider_request_sent,
            late_content_discarded: late,
            cache,
            usage,
            cost,
            latency,
            queued_at_unix_ms: queued.queued_at_unix_ms,
            dispatched_at_unix_ms,
            completed_at_unix_ms: self.clock.unix_millis(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn telemetry_for_ticket(
        &self,
        ticket: &DispatchTicket,
        outcome: TerminalOutcomeDto,
        error: Option<ErrorCode>,
        provider_request_sent: bool,
        late: bool,
        cache: CacheObservation,
        usage: NormalizedUsage,
        cost: CostEstimate,
        latency: LatencyFields,
        dispatched_at_unix_ms: Option<i64>,
    ) -> TerminalTelemetryDto {
        TerminalTelemetryDto {
            call_id: ticket.call.request.call_id.clone(),
            recording_id: ticket.call.recording_id.clone(),
            request_group_id: ticket.call.request.group_id.clone(),
            target_id: ticket.call.request.target_id.clone(),
            lane: ticket.call.request.lane,
            attempt_no: 1,
            fence: ticket.call.request.fence.clone(),
            provider: ticket.call.request.provider.clone(),
            transport: ticket.call.request.transport.clone(),
            model: ticket.call.request.model.clone(),
            support_tier: ticket.descriptor.support_tier,
            adapter_version: ticket.call.adapter_version,
            prompt_version: corti_postprocess::PROMPT_TEMPLATE_VERSION,
            output_schema_version: corti_postprocess::OUTPUT_SCHEMA_VERSION,
            outcome,
            error,
            provider_request_sent,
            late_content_discarded: late,
            cache,
            usage,
            cost,
            latency,
            queued_at_unix_ms: ticket.queued_at_unix_ms,
            dispatched_at_unix_ms,
            completed_at_unix_ms: self.clock.unix_millis(),
        }
    }

    fn push_terminal_events(
        &mut self,
        telemetry: &TerminalTelemetryDto,
        finality: AccountingFinalityDto,
    ) {
        self.push_event(CoordinatorEventDto::Accounting(Box::new(
            AccountingEventDto {
                call_id: telemetry.call_id.clone(),
                recording_id: telemetry.recording_id.clone(),
                lane: telemetry.lane,
                fence: telemetry.fence.clone(),
                finality,
                usage: telemetry.usage,
                cost: telemetry.cost.clone(),
                late: telemetry.late_content_discarded,
            },
        )));
        self.push_event(CoordinatorEventDto::Terminal(Box::new(telemetry.clone())));
    }

    fn push_event(&mut self, event: CoordinatorEventDto) {
        if self.events.len() >= MAX_COORDINATOR_EVENTS {
            // State/accounting events are repairable from snapshots/store. Keep the queue bounded rather than
            // ever applying backpressure to row publication.
            self.events.pop_front();
        }
        self.events.push_back(event);
    }
}

fn final_boundary(submission: &RequestSubmission) -> FinalJournalBoundary {
    FinalJournalBoundary {
        recording_id: submission.recording_id.clone(),
        request_group_id: submission.request.group_id.clone(),
        call_id: submission.request.call_id.clone(),
        request_key: submission.request_key,
        fence: submission.request.fence.clone(),
    }
}

fn ui_fence(fence: &RequestFence) -> UiFenceDto {
    UiFenceDto {
        process_epoch: fence.process_epoch,
        session_generation: fence.session_generation,
        control_revision: fence.control_revision,
        lane_revision: fence.lane_revision,
    }
}

fn request_context(request: &HostedRequest) -> EventContext {
    EventContext {
        call_id: request.call_id.clone(),
        group_id: request.group_id.clone(),
        target_id: request.target_id.clone(),
        lane: request.lane,
        fence: request.fence.clone(),
    }
}

fn is_vertex(request: &HostedRequest) -> bool {
    let descriptor = corti_postprocess::KnownTransport::VertexDirect.descriptor();
    request.provider == descriptor.provider && request.transport == descriptor.transport
}

fn validate_exact_model(queued: &QueuedCall, catalog: &ModelCatalog) -> Result<(), ErrorCode> {
    let request = &queued.submission.request;
    let Some(model) = catalog.find_exact(&request.model, queued.submission.scope.region.as_deref())
    else {
        return Err(ErrorCode::ModelUnavailable);
    };
    if model.provider != request.provider
        || model.transport != request.transport
        || model.support_tier != queued.descriptor.support_tier
        || model.billing_basis != queued.descriptor.billing_basis
        || !model.account_scoped_available
        || model.deprecated
        || !model.capabilities.text_input
        || !model.capabilities.text_output
        || !model.capabilities.structured_output
        || (request.lane == Lane::Live && !model.benchmarked_for_live)
    {
        return Err(ErrorCode::PolicyBlocked);
    }
    match request.cache_policy.provider {
        ProviderCacheMode::ExplicitStablePrefix if !model.capabilities.explicit_prefix_cache => {
            Err(ErrorCode::PolicyBlocked)
        }
        ProviderCacheMode::UnavoidableImplicit if !model.capabilities.implicit_cache_may_apply => {
            Err(ErrorCode::PolicyBlocked)
        }
        ProviderCacheMode::Off if model.capabilities.implicit_cache_may_apply => {
            Err(ErrorCode::PolicyBlocked)
        }
        ProviderCacheMode::Off
        | ProviderCacheMode::ExplicitStablePrefix
        | ProviderCacheMode::UnavoidableImplicit
        | ProviderCacheMode::Unavailable => Ok(()),
    }
}

fn validate_output(
    submission: &RequestSubmission,
    output: ProviderOutput,
) -> Result<ValidatedOutput, ErrorCode> {
    match (submission.request.lane, output) {
        (Lane::Live | Lane::Final, ProviderOutput::Rewrite(output)) => {
            let bytes = serde_json::to_vec(&output).map_err(|_| ErrorCode::MalformedOutput)?;
            let validated: ValidatedRewrite = parse_and_validate_rewrite(
                &bytes,
                &submission.request.targets,
                RewriteValidationLimits {
                    request_max_output_bytes: submission.request_max_output_bytes,
                    catalog_max_output_bytes: submission.catalog_max_output_bytes,
                },
            )
            .map_err(|_| ErrorCode::MalformedOutput)?;
            let rows = validated
                .apply_to(&submission.request.targets)
                .map_err(|_| ErrorCode::MalformedOutput)?;
            Ok(ValidatedOutput::Rewrite { rows })
        }
        (Lane::AdHocQuestion | Lane::PinnedQuestion, ProviderOutput::Question(question)) => {
            let bytes =
                serde_json::to_vec(&question.output).map_err(|_| ErrorCode::MalformedOutput)?;
            let validated: ValidatedQuestion = parse_and_validate_question(
                &bytes,
                &submission.request.context,
                submission.expected_context_truncated,
                submission
                    .request_max_output_bytes
                    .min(submission.catalog_max_output_bytes)
                    .min(MAX_VISIBLE_ASSISTANT_BYTES),
            )
            .map_err(|_| ErrorCode::MalformedOutput)?;
            Ok(ValidatedOutput::Question {
                answer: validated.answer().to_owned(),
                cited_row_ids: validated.cited_row_ids().to_vec(),
                context_truncated: validated.context_truncated(),
            })
        }
        _ => Err(ErrorCode::MalformedOutput),
    }
}

#[allow(clippy::too_many_arguments)]
fn estimate_cost(
    pricing: &dyn PricingCatalog,
    descriptor: &ProviderDescriptor,
    model: &ModelId,
    region: Option<&str>,
    dispatched_at_unix_ms: Option<i64>,
    provider_request_sent: bool,
    cache: CacheObservation,
    usage: &NormalizedUsage,
) -> CostEstimate {
    if !provider_request_sent {
        return if cache == CacheObservation::Local {
            CostEstimate::no_provider_request()
        } else {
            CostEstimate::unavailable()
        };
    }
    let Some(dispatch_unix_ms) = dispatched_at_unix_ms else {
        return CostEstimate::unavailable();
    };
    pricing
        .estimate(
            PricingQuery {
                provider: &descriptor.provider,
                exact_model_id: model,
                region,
                support_tier: descriptor.support_tier,
                dispatch_unix_ms,
                billing_basis: descriptor.billing_basis,
            },
            usage,
        )
        .unwrap_or_else(|_| CostEstimate::unavailable())
}

fn cancellation_outcome(reason: CancellationReason) -> (TerminalOutcomeDto, ErrorCode) {
    match reason {
        CancellationReason::Superseded
        | CancellationReason::SteeringChanged
        | CancellationReason::WordBankChanged
        | CancellationReason::ModelChanged => {
            (TerminalOutcomeDto::Superseded, ErrorCode::Superseded)
        }
        CancellationReason::Deadline => (TerminalOutcomeDto::Timeout, ErrorCode::Timeout),
        _ => (TerminalOutcomeDto::Canceled, ErrorCode::Canceled),
    }
}

/// Bounded hot-path handoff. Only `try_send` is exposed, so capture/live-ASR code cannot accidentally wait
/// behind cache, auth, provider, or store work.
pub(crate) struct CoordinatorIngress {
    sender: SyncSender<HotPathCommand>,
}

impl Clone for CoordinatorIngress {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

pub(crate) enum HotPathCommand {
    FinalizedRows {
        recording_id: String,
        rows: Vec<TranscriptRow>,
    },
    LiveRequest {
        submission: Box<RequestSubmission>,
        watermark: TranscriptWatermark,
    },
}

impl fmt::Debug for HotPathCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FinalizedRows { recording_id, rows } => f
                .debug_struct("FinalizedRows")
                .field("recording_id", recording_id)
                .field("row_count", &rows.len())
                .finish(),
            Self::LiveRequest {
                submission,
                watermark,
            } => f
                .debug_struct("LiveRequest")
                .field("submission", submission)
                .field("watermark", watermark)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub(crate) enum IngressError {
    #[error("hosted coordinator is saturated")]
    Full,
    #[error("hosted coordinator has stopped")]
    Disconnected,
}

impl CoordinatorIngress {
    pub(crate) fn bounded(capacity: usize) -> (Self, Receiver<HotPathCommand>) {
        let (sender, receiver) = sync_channel(capacity);
        (Self { sender }, receiver)
    }

    pub(crate) fn standard() -> (Self, Receiver<HotPathCommand>) {
        Self::bounded(COORDINATOR_COMMAND_CAPACITY)
    }

    pub(crate) fn try_send(&self, command: HotPathCommand) -> Result<(), IngressError> {
        self.sender.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => IngressError::Full,
            TrySendError::Disconnected(_) => IngressError::Disconnected,
        })
    }
}

/// Bounded provider-event handoff for worker sinks. Workers may retry/coalesce off the hot path; this receiver
/// is drained by the coordinator and cannot grow without bound.
pub(crate) struct BoundedProviderEventSink {
    sender: SyncSender<ProviderEvent>,
}

impl BoundedProviderEventSink {
    pub(crate) fn channel() -> (Arc<Self>, Receiver<ProviderEvent>) {
        let (sender, receiver) = sync_channel(PROVIDER_EVENT_CAPACITY);
        (Arc::new(Self { sender }), receiver)
    }
}

impl ProviderEventSink for BoundedProviderEventSink {
    fn emit(&self, event: ProviderEvent) {
        // Provider workers are explicitly allowed to block off the capture/ASR path. A bounded blocking send
        // preserves terminal usage instead of allocating an unbounded event queue.
        let _ = self.sender.send(event);
    }
}

pub(crate) fn drain_provider_events(
    receiver: &Receiver<ProviderEvent>,
    coordinator: &mut PostprocessCoordinator,
) -> Result<usize, CoordinatorError> {
    let mut drained = 0;
    loop {
        match receiver.try_recv() {
            Ok(event) => {
                coordinator.on_provider_event(event)?;
                drained += 1;
            }
            Err(TryRecvError::Empty) => return Ok(drained),
            Err(TryRecvError::Disconnected) => return Ok(drained),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicI64, AtomicU64, Ordering},
    };

    use corti_postprocess::{
        AdapterCapabilities, BillingBasis, CachePolicy, CanonicalPrompt, ConnectionScopeId,
        CredentialSourceKind, CurrencyCode, DigestKey, InputTokenAccounting, KnownTransport,
        ModelId, NormalizedUsage, OUTPUT_SCHEMA_VERSION, OutputTokenAccounting,
        PROMPT_TEMPLATE_VERSION, PricingError, QuestionOutput, QuestionTerminal, RawUsage,
        Replacement, RequestGroupId, RequestKeyMaterial, RewriteOutput, RowId, TargetId, Tariff,
        TariffCatalog, TariffRates, WordBankDocument,
    };

    use super::*;

    #[derive(Clone)]
    struct FakeClock {
        monotonic: Arc<AtomicU64>,
        wall: Arc<AtomicI64>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                monotonic: Arc::new(AtomicU64::new(0)),
                wall: Arc::new(AtomicI64::new(1_800_000_000_000)),
            }
        }

        fn now(&self) -> u64 {
            self.monotonic.load(Ordering::SeqCst)
        }

        fn set(&self, value: u64) {
            self.monotonic.store(value, Ordering::SeqCst);
        }

        fn advance(&self, micros: u64) {
            self.monotonic.fetch_add(micros, Ordering::SeqCst);
            let millis = i64::try_from(micros / 1_000).unwrap();
            self.wall.fetch_add(millis, Ordering::SeqCst);
        }
    }

    impl ProviderClock for FakeClock {
        fn monotonic_micros(&self) -> u64 {
            self.now()
        }
    }

    impl CoordinatorClock for FakeClock {
        fn unix_millis(&self) -> i64 {
            self.wall.load(Ordering::SeqCst)
        }
    }

    #[derive(Default)]
    struct PersistenceState {
        snapshots: Vec<ControlSnapshotDto>,
        fail: bool,
    }

    struct FakePersistence(Arc<Mutex<PersistenceState>>);

    impl ControlPersistence for FakePersistence {
        fn persist(&mut self, snapshot: &ControlSnapshotDto) -> Result<(), ErrorCode> {
            let mut state = self.0.lock().unwrap();
            if state.fail {
                return Err(ErrorCode::Cache);
            }
            state.snapshots.push(snapshot.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct StoreState {
        lookups: VecDeque<ExactLookup>,
        trace: Vec<&'static str>,
        commits: Vec<(CallId, ValidatedOutput)>,
        telemetry: Vec<TerminalTelemetryDto>,
        journal: Vec<(CallId, FinalJournalState)>,
        recovery: Option<FinalRecoveryRecord>,
    }

    struct FakeStore(Arc<Mutex<StoreState>>);

    impl EncryptedPostprocessStore for FakeStore {
        fn lookup_exact(&mut self, _key: RequestKey) -> Result<ExactLookup, ErrorCode> {
            let mut state = self.0.lock().unwrap();
            state.trace.push("lookup");
            Ok(state.lookups.pop_front().unwrap_or(ExactLookup::Miss))
        }

        fn evict_corrupt(&mut self, _key: RequestKey) -> Result<(), ErrorCode> {
            self.0.lock().unwrap().trace.push("evict");
            Ok(())
        }

        fn prepare_final(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode> {
            let mut state = self.0.lock().unwrap();
            state.trace.push("prepared");
            state
                .journal
                .push((boundary.call_id.clone(), FinalJournalState::Prepared));
            Ok(())
        }

        fn mark_final_dispatched(
            &mut self,
            boundary: &FinalJournalBoundary,
        ) -> Result<(), ErrorCode> {
            let mut state = self.0.lock().unwrap();
            state.trace.push("dispatched");
            state
                .journal
                .push((boundary.call_id.clone(), FinalJournalState::Dispatched));
            Ok(())
        }

        fn commit_validated(&mut self, commit: StoreCommit<'_>) -> Result<(), ErrorCode> {
            let mut state = self.0.lock().unwrap();
            state.trace.push("commit");
            state
                .commits
                .push((commit.telemetry.call_id.clone(), commit.output.clone()));
            state.telemetry.push(commit.telemetry.clone());
            if let Some(boundary) = commit.final_boundary {
                state
                    .journal
                    .push((boundary.call_id.clone(), FinalJournalState::ResultCached));
            }
            Ok(())
        }

        fn abandon_final(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode> {
            self.0
                .lock()
                .unwrap()
                .journal
                .push((boundary.call_id.clone(), FinalJournalState::Abandoned));
            Ok(())
        }

        fn mark_final_group_applied(
            &mut self,
            boundaries: &[FinalJournalBoundary],
        ) -> Result<(), ErrorCode> {
            let mut state = self.0.lock().unwrap();
            state.trace.push("group_applied");
            state.journal.extend(
                boundaries
                    .iter()
                    .map(|boundary| (boundary.call_id.clone(), FinalJournalState::Applied)),
            );
            Ok(())
        }

        fn mark_final_group_checkpointed(
            &mut self,
            boundaries: &[FinalJournalBoundary],
        ) -> Result<(), ErrorCode> {
            let mut state = self.0.lock().unwrap();
            state.trace.push("group_checkpointed");
            state.journal.extend(
                boundaries
                    .iter()
                    .map(|boundary| (boundary.call_id.clone(), FinalJournalState::Checkpointed)),
            );
            Ok(())
        }

        fn recover_final(
            &mut self,
            _recording_id: &str,
        ) -> Result<Option<FinalRecoveryRecord>, ErrorCode> {
            Ok(self.0.lock().unwrap().recovery.clone())
        }

        fn record_terminal(&mut self, telemetry: &TerminalTelemetryDto) -> Result<(), ErrorCode> {
            let mut state = self.0.lock().unwrap();
            state.trace.push("telemetry");
            state.telemetry.push(telemetry.clone());
            Ok(())
        }
    }

    struct ProviderState {
        trace: Vec<&'static str>,
        credential: CredentialState,
        catalog_error: Option<ErrorCode>,
    }

    struct FakeProviders(Arc<Mutex<ProviderState>>);

    impl ProviderAccess for FakeProviders {
        fn descriptor(
            &mut self,
            provider: &ProviderId,
            transport: &TransportId,
        ) -> Option<ProviderDescriptor> {
            self.0.lock().unwrap().trace.push("descriptor");
            known_descriptor(provider, transport)
        }

        fn credential_state(
            &mut self,
            _provider: &ProviderId,
            _transport: &TransportId,
        ) -> CredentialState {
            let mut state = self.0.lock().unwrap();
            state.trace.push("auth");
            state.credential.clone()
        }

        fn catalog(
            &mut self,
            provider: &ProviderId,
            transport: &TransportId,
            scope: &ProviderScope,
        ) -> Result<ModelCatalog, PostprocessError> {
            let mut state = self.0.lock().unwrap();
            state.trace.push("catalog");
            if let Some(code) = state.catalog_error {
                return Err(code.into());
            }
            let descriptor = known_descriptor(provider, transport).ok_or(PostprocessError {
                code: ErrorCode::ModelUnavailable,
            })?;
            Ok(ModelCatalog {
                models: vec![model_descriptor(&descriptor, scope.region.clone(), true)],
            })
        }
    }

    fn known_descriptor(
        provider: &ProviderId,
        transport: &TransportId,
    ) -> Option<ProviderDescriptor> {
        [
            KnownTransport::OpenAiDirect,
            KnownTransport::VertexDirect,
            KnownTransport::AnthropicDirect,
            KnownTransport::CodexAppServer,
            KnownTransport::ClaudeSubscription,
            KnownTransport::BedrockRuntime,
        ]
        .into_iter()
        .map(KnownTransport::descriptor)
        .find(|descriptor| &descriptor.provider == provider && &descriptor.transport == transport)
    }

    fn model_descriptor(
        descriptor: &ProviderDescriptor,
        region: Option<String>,
        benchmarked_for_live: bool,
    ) -> ModelDescriptor {
        ModelDescriptor {
            provider: descriptor.provider.clone(),
            transport: descriptor.transport.clone(),
            support_tier: descriptor.support_tier,
            exact_model_id: ModelId::new("fixture-model-v1").unwrap(),
            account_scoped_available: true,
            region,
            max_context_tokens: 32_000,
            max_output_tokens: 4_096,
            capabilities: AdapterCapabilities {
                text_input: true,
                text_output: true,
                streaming: true,
                structured_output: true,
                explicit_prefix_cache: true,
                implicit_cache_may_apply: false,
            },
            billing_basis: descriptor.billing_basis,
            tariff_version: Some("fixture-tariff-v1".into()),
            deprecated: false,
            benchmarked_for_live,
        }
    }

    struct Harness {
        coordinator: PostprocessCoordinator,
        clock: FakeClock,
        persistence: Arc<Mutex<PersistenceState>>,
        store: Arc<Mutex<StoreState>>,
        providers: Arc<Mutex<ProviderState>>,
    }

    impl Harness {
        fn new() -> Self {
            let clock = FakeClock::new();
            let persistence = Arc::new(Mutex::new(PersistenceState::default()));
            let store = Arc::new(Mutex::new(StoreState::default()));
            let providers = Arc::new(Mutex::new(ProviderState {
                trace: Vec::new(),
                credential: CredentialState::Ready {
                    expires_at_unix_ms: None,
                    source: CredentialSourceKind::Keychain,
                },
                catalog_error: None,
            }));
            let pricing = Arc::new(pricing_catalog());
            let coordinator = PostprocessCoordinator::new(
                ProcessEpoch(77),
                Arc::new(clock.clone()),
                Box::new(FakePersistence(persistence.clone())),
                Box::new(FakeStore(store.clone())),
                Box::new(FakeProviders(providers.clone())),
                pricing,
            );
            Self {
                coordinator,
                clock,
                persistence,
                store,
                providers,
            }
        }

        fn configure(&mut self, family: LaneFamily, transport: KnownTransport) {
            let descriptor = transport.descriptor();
            let selection = LaneSelectionDto {
                provider: Some(descriptor.provider),
                transport: Some(descriptor.transport),
                model: Some(ModelId::new("fixture-model-v1").unwrap()),
                cache_policy: fixture_cache_policy(),
            };
            self.coordinator
                .apply_patch(ControlPatch::SetLaneSelection {
                    lane: family,
                    selection,
                })
                .unwrap();
            self.coordinator
                .apply_patch(ControlPatch::SetLaneEnabled {
                    lane: family,
                    enabled: true,
                })
                .unwrap();
        }

        fn enable_master(&mut self) {
            self.coordinator
                .apply_patch(ControlPatch::SetEgressAcknowledged(true))
                .unwrap();
            self.coordinator
                .apply_patch(ControlPatch::SetMaster(true))
                .unwrap();
        }
    }

    fn fixture_cache_policy() -> CachePolicy {
        CachePolicy {
            local: LocalCacheMode::Reusable,
            provider: ProviderCacheMode::Off,
        }
    }

    fn pricing_catalog() -> TariffCatalog {
        let tariffs = [
            KnownTransport::OpenAiDirect.descriptor(),
            KnownTransport::VertexDirect.descriptor(),
            KnownTransport::AnthropicDirect.descriptor(),
        ]
        .into_iter()
        .map(|descriptor| Tariff {
            tariff_id: format!("fixture-{}", descriptor.provider.as_str()),
            provider: descriptor.provider,
            exact_model_id: ModelId::new("fixture-model-v1").unwrap(),
            region: None,
            support_tier: descriptor.support_tier,
            effective_from_unix_ms: 0,
            effective_until_unix_ms: None,
            currency: CurrencyCode::usd(),
            input_accounting: InputTokenAccounting::ClassesDisjoint,
            output_accounting: OutputTokenAccounting::IncludesReasoning,
            rates: TariffRates {
                input_micros_per_million: Some(1_000_000),
                output_micros_per_million: Some(2_000_000),
                cached_read_micros_per_million: None,
                cached_write_micros_per_million: None,
                reasoning_micros_per_million: None,
            },
        })
        .collect();
        TariffCatalog {
            version: "fixture-pricing-v1".into(),
            source_url: "https://fixture.invalid/pricing".into(),
            retrieved_at_unix_ms: 0,
            valid_until_unix_ms: i64::MAX,
            tariffs,
        }
    }

    fn row(id: u64, text: impl Into<String>, start_ms: u64, end_ms: u64) -> TranscriptRow {
        TranscriptRow {
            row_id: RowId::new(format!("fixture-row-{id}")).unwrap(),
            speaker: "Fixture speaker".into(),
            start_ms,
            end_ms,
            text: text.into(),
        }
    }

    fn dummy_fence() -> RequestFence {
        RequestFence {
            process_epoch: ProcessEpoch(1),
            session_generation: 0,
            transcript_revision: 0,
            control_revision: 0,
            lane_revision: 0,
            steering_revision: 0,
            bank_revision: 0,
            question_revision: None,
        }
    }

    fn submission(
        call: &str,
        lane: Lane,
        transport: KnownTransport,
        now: u64,
        target_rows: Vec<TranscriptRow>,
        context_rows: Vec<TranscriptRow>,
    ) -> RequestSubmission {
        let descriptor = transport.descriptor();
        let model = ModelId::new("fixture-model-v1").unwrap();
        let scope_id = ConnectionScopeId::new("fixture-scope").unwrap();
        let scope = ProviderScope {
            connection_scope_id: scope_id.clone(),
            region: None,
        };
        let bank = WordBankDocument::empty();
        let prompt = if lane.is_question() {
            CanonicalPrompt::question(
                &bank,
                "fixture steering",
                &context_rows,
                "fixture question",
                false,
            )
        } else {
            CanonicalPrompt::rewrite(&bank, "fixture steering", &context_rows, &target_rows)
        };
        let request_key = RequestKey::derive(
            &DigestKey::new([9; 32]),
            &RequestKeyMaterial {
                provider: &descriptor.provider,
                transport: &descriptor.transport,
                support_tier: descriptor.support_tier,
                connection_scope_id: &scope_id,
                region: None,
                exact_model_id: &model,
                adapter_version: 1,
                prompt_template_version: PROMPT_TEMPLATE_VERSION,
                output_schema_version: OUTPUT_SCHEMA_VERSION,
                chunker_version: 1,
                lane,
                billing_basis: descriptor.billing_basis,
                cache_policy: fixture_cache_policy(),
                word_bank_canonical_digest: bank.content_digest(),
                effective_steering: "fixture steering",
                targets: &target_rows,
                context: &context_rows,
                question: lane.is_question().then_some("fixture question"),
            },
        );
        let request = HostedRequest {
            call_id: CallId::new(call).unwrap(),
            group_id: RequestGroupId::new(format!("group-{call}")).unwrap(),
            target_id: Some(TargetId::new(format!("target-{call}")).unwrap()),
            lane,
            fence: dummy_fence(),
            provider: descriptor.provider,
            transport: descriptor.transport,
            model,
            targets: target_rows,
            context: context_rows,
            prompt,
            deadline: MonotonicDeadline(now.saturating_add(120_000_000)),
            cache_policy: fixture_cache_policy(),
        };
        RequestSubmission::new(
            format!("recording-{call}"),
            request,
            request_key,
            scope,
            1,
            64 * 1024,
            64 * 1024,
            false,
        )
        .unwrap()
    }

    fn rewrite_output(target: &TranscriptRow, text: &str) -> ProviderOutput {
        ProviderOutput::Rewrite(RewriteOutput {
            schema: 1,
            replacements: vec![Replacement {
                row_id: target.row_id.clone(),
                text: text.into(),
            }],
        })
    }

    fn question_output(context: &TranscriptRow, answer: &str) -> ProviderOutput {
        ProviderOutput::Question(QuestionTerminal {
            output: QuestionOutput {
                schema: 1,
                answer: answer.into(),
                cited_row_ids: vec![context.row_id.clone()],
                context_truncated: false,
            },
        })
    }

    fn terminal(
        output: ProviderOutput,
        usage: NormalizedUsage,
    ) -> corti_postprocess::ProviderTerminal {
        corti_postprocess::ProviderTerminal {
            output,
            usage,
            latency: LatencyFields {
                total_us: Some(42_000),
                ..LatencyFields::default()
            },
            cache: CacheObservation::None,
        }
    }

    fn complete_usage() -> NormalizedUsage {
        NormalizedUsage::try_from(RawUsage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            cached_read_tokens: None,
            cached_write_tokens: None,
            reasoning_tokens: None,
            usage_complete: true,
        })
        .unwrap()
    }

    fn provider_event(ticket: &DispatchTicket, kind: ProviderEventKind) -> ProviderEvent {
        ProviderEvent {
            context: request_context(ticket.request()),
            kind,
        }
    }

    fn ticket(outcome: DispatchOutcome) -> DispatchTicket {
        match outcome {
            DispatchOutcome::Ticket(ticket) => ticket,
            other => panic!("expected dispatch ticket, got {other:?}"),
        }
    }

    #[test]
    fn controls_are_monotonic_independent_and_disable_is_fail_safe() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Live, KnownTransport::OpenAiDirect);
        harness.configure(LaneFamily::Final, KnownTransport::OpenAiDirect);
        harness.enable_master();
        let before = harness.coordinator.control_snapshot().clone();

        harness.persistence.lock().unwrap().fail = true;
        let outcome = harness
            .coordinator
            .apply_patch(ControlPatch::SetLaneEnabled {
                lane: LaneFamily::Final,
                enabled: false,
            })
            .unwrap();
        assert!(matches!(outcome, PatchOutcome::DisabledForSession { .. }));
        let after = harness.coordinator.control_snapshot();
        assert!(!after.final_lane.enabled);
        assert!(after.live.enabled);
        assert_eq!(after.live.revision, before.live.revision);
        assert!(after.final_lane.revision > before.final_lane.revision);
        assert_eq!(after.control_revision, before.control_revision);

        let mut fresh = Harness::new();
        fresh.persistence.lock().unwrap().fail = true;
        assert_eq!(
            fresh
                .coordinator
                .apply_patch(ControlPatch::SetEgressAcknowledged(true)),
            Err(ControlError::Persistence)
        );
        assert!(!fresh.coordinator.control_snapshot().egress_acknowledged);
    }

    #[test]
    fn final_exact_cache_is_checked_before_auth_and_recovery_committed_before_apply() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Final, KnownTransport::OpenAiDirect);
        harness.enable_master();
        let target = row(1, "fixture raw", 0, 1_000);
        let watermark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&target))
            .unwrap();
        harness
            .store
            .lock()
            .unwrap()
            .lookups
            .push_back(ExactLookup::Hit(rewrite_output(&target, "fixture clean")));
        harness.store.lock().unwrap().trace.clear();
        harness.providers.lock().unwrap().trace.clear();
        harness
            .coordinator
            .submit_final(
                submission(
                    "cache-final",
                    Lane::Final,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    vec![target.clone()],
                    Vec::new(),
                ),
                watermark,
            )
            .unwrap();

        let apply = match harness.coordinator.dispatch_next() {
            DispatchOutcome::CacheApply(apply) => apply,
            other => panic!("expected cache apply, got {other:?}"),
        };
        assert!(apply.recovery_committed);
        assert!(harness.coordinator.application_is_current(&apply));
        assert_eq!(
            harness.store.lock().unwrap().trace,
            ["prepared", "lookup", "commit"]
        );
        assert!(!harness.providers.lock().unwrap().trace.contains(&"auth"));
        let telemetry = harness
            .store
            .lock()
            .unwrap()
            .telemetry
            .last()
            .unwrap()
            .clone();
        assert!(!telemetry.provider_request_sent);
        assert_eq!(telemetry.usage, NormalizedUsage::unknown());
        assert_eq!(telemetry.cost.render(), "Local cache · no provider request");

        harness
            .coordinator
            .mark_final_applied(&apply.call_id)
            .unwrap();
        harness
            .coordinator
            .mark_final_checkpointed(&apply.call_id)
            .unwrap();
        assert_eq!(
            harness
                .store
                .lock()
                .unwrap()
                .journal
                .iter()
                .map(|(_, state)| *state)
                .collect::<Vec<_>>(),
            [
                FinalJournalState::Prepared,
                FinalJournalState::ResultCached,
                FinalJournalState::Applied,
                FinalJournalState::Checkpointed,
            ]
        );
    }

    #[test]
    fn live_debounce_keeps_only_the_latest_pending_snapshot() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Live, KnownTransport::OpenAiDirect);
        harness.enable_master();
        let first = row(1, "fixture one", 0, 500);
        let first_mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&first))
            .unwrap();
        harness
            .coordinator
            .submit_live(
                submission(
                    "live-old",
                    Lane::Live,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    vec![first],
                    Vec::new(),
                ),
                first_mark,
            )
            .unwrap();
        let second = row(2, "fixture two", 500, 1_000);
        let second_mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&second))
            .unwrap();
        harness
            .coordinator
            .submit_live(
                submission(
                    "live-new",
                    Lane::Live,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    vec![second],
                    Vec::new(),
                ),
                second_mark,
            )
            .unwrap();

        harness.clock.advance(LIVE_DEBOUNCE_MICROS - 1);
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Waiting
        ));
        harness.clock.advance(1);
        let dispatched = ticket(harness.coordinator.dispatch_next());
        assert_eq!(dispatched.request().call_id.as_str(), "live-new");
        assert!(
            !harness.coordinator.queue.iter().any(|queued| queued
                .submission
                .request
                .call_id
                .as_str()
                == "live-old")
        );
    }

    #[test]
    fn ad_hoc_questions_are_visible_bounded_fifo_and_single_flight() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Question, KnownTransport::OpenAiDirect);
        harness.enable_master();
        let context = row(1, "fixture context", 0, 1_000);
        let watermark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&context))
            .unwrap();
        for index in 0..MAX_QUEUED_AD_HOC_QUESTIONS {
            harness
                .coordinator
                .submit_ad_hoc(
                    submission(
                        &format!("question-{index}"),
                        Lane::AdHocQuestion,
                        KnownTransport::OpenAiDirect,
                        harness.clock.now(),
                        Vec::new(),
                        vec![context.clone()],
                    ),
                    watermark,
                    format!("fixture question {index}"),
                )
                .unwrap();
        }
        let overflow = harness.coordinator.submit_ad_hoc(
            submission(
                "question-overflow",
                Lane::AdHocQuestion,
                KnownTransport::OpenAiDirect,
                harness.clock.now(),
                Vec::new(),
                vec![context.clone()],
            ),
            watermark,
            "fixture overflow".into(),
        );
        assert_eq!(overflow, Err(SubmitError::AdHocQueueFull));
        assert_eq!(harness.coordinator.question_summaries().len(), 8);

        let first = ticket(harness.coordinator.dispatch_next());
        assert_eq!(first.request().call_id.as_str(), "question-0");
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Backpressured | DispatchOutcome::Waiting
        ));
        let first_id = first.request().call_id.clone();
        let outcome = harness.coordinator.complete(
            first,
            Ok(terminal(
                question_output(&context, "fixture answer"),
                complete_usage(),
            )),
        );
        assert!(matches!(outcome, CompletionOutcome::Apply(_)));
        assert_eq!(
            harness
                .coordinator
                .question_content(&first_id)
                .unwrap()
                .answer,
            Some("fixture answer")
        );
        let second = ticket(harness.coordinator.dispatch_next());
        assert_eq!(second.request().call_id.as_str(), "question-1");
    }

    #[test]
    fn terminal_auth_rejection_updates_coordinator_and_cancels_queued_peers() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Question, KnownTransport::OpenAiDirect);
        harness.enable_master();
        let context = row(1, "fixture context", 0, 1_000);
        let watermark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&context))
            .unwrap();
        for index in 0..3 {
            harness
                .coordinator
                .submit_ad_hoc(
                    submission(
                        &format!("auth-question-{index}"),
                        Lane::AdHocQuestion,
                        KnownTransport::OpenAiDirect,
                        harness.clock.now(),
                        Vec::new(),
                        vec![context.clone()],
                    ),
                    watermark,
                    format!("fixture question {index}"),
                )
                .unwrap();
        }
        let first = ticket(harness.coordinator.dispatch_next());
        let outcome = harness
            .coordinator
            .complete(first, Err(ErrorCode::AuthRejected.into()));
        assert!(matches!(
            outcome,
            CompletionOutcome::Failed {
                code: ErrorCode::AuthRejected,
                ..
            }
        ));
        assert!(harness.coordinator.queue.is_empty());
        assert!(
            harness
                .coordinator
                .question_summaries()
                .iter()
                .all(|summary| summary.status == QuestionStatusDto::Failed
                    && summary.error == Some(ErrorCode::AuthRejected))
        );
        let descriptor = KnownTransport::OpenAiDirect.descriptor();
        let state = harness
            .coordinator
            .provider_states()
            .find(|state| state.descriptor == descriptor)
            .unwrap();
        assert_eq!(state.credential, CredentialState::Rejected);
        assert!(state.models.is_empty());
        assert!(
            harness
                .store
                .lock()
                .unwrap()
                .telemetry
                .iter()
                .all(|telemetry| telemetry.cost.render() == "Cost unavailable"),
            "pre-egress auth failures must never be mislabeled as local cache hits"
        );
    }

    #[test]
    fn pinned_policy_hits_exact_thresholds_and_coalesces_one_dirty_rerun() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Question, KnownTransport::OpenAiDirect);
        harness.enable_master();
        harness
            .coordinator
            .apply_patch(ControlPatch::SetPinnedAuto(true))
            .unwrap();
        harness
            .coordinator
            .edit_pinned_template("fixture pinned template".into())
            .unwrap();
        assert_eq!(
            harness
                .coordinator
                .control_snapshot()
                .pinned_question_revision,
            2,
            "the frontend owns edit debounce; the backend commits one accepted edit atomically"
        );

        let words_39 = (0..39)
            .map(|index| format!("w{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let context_39 = row(1, words_39, 0, 29_999);
        let mark_39 = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&context_39))
            .unwrap();
        harness
            .coordinator
            .submit_pinned_snapshot(
                submission(
                    "pinned-39",
                    Lane::PinnedQuestion,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    Vec::new(),
                    vec![context_39],
                ),
                mark_39,
            )
            .unwrap();
        harness.clock.advance(PINNED_QUIET_DEBOUNCE_MICROS);
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Empty
        ));

        let threshold = row(2, "w39", 29_999, 30_000);
        let mark_40 = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&threshold))
            .unwrap();
        harness
            .coordinator
            .submit_pinned_snapshot(
                submission(
                    "pinned-40",
                    Lane::PinnedQuestion,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    Vec::new(),
                    vec![threshold.clone()],
                ),
                mark_40,
            )
            .unwrap();
        harness.clock.advance(PINNED_QUIET_DEBOUNCE_MICROS - 1);
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Empty
        ));
        harness.clock.advance(1);
        harness.coordinator.tick();
        assert!(
            harness.coordinator.queue.iter().any(|queued| queued
                .submission
                .request
                .call_id
                .as_str()
                == "pinned-40")
        );

        // Even after the debounce has put a call in the scheduler queue, a newer not-yet-dispatched
        // snapshot replaces it. This is separate from the one dirty rerun allowed during an active call.
        let replacement = row(3, "w40", 30_000, 30_001);
        let replacement_mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&replacement))
            .unwrap();
        harness
            .coordinator
            .submit_pinned_snapshot(
                submission(
                    "pinned-newest-pending",
                    Lane::PinnedQuestion,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    Vec::new(),
                    vec![replacement.clone()],
                ),
                replacement_mark,
            )
            .unwrap();
        harness.clock.advance(PINNED_QUIET_DEBOUNCE_MICROS);
        let first = ticket(harness.coordinator.dispatch_next());
        assert_eq!(first.request().call_id.as_str(), "pinned-newest-pending");
        assert_eq!(harness.coordinator.pinned_run_count(), 1);

        let more_words = (0..40)
            .map(|index| format!("n{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        let newer = row(4, more_words, 30_001, 31_001);
        let newer_mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&newer))
            .unwrap();
        harness
            .coordinator
            .submit_pinned_snapshot(
                submission(
                    "pinned-rerun",
                    Lane::PinnedQuestion,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    Vec::new(),
                    vec![newer.clone()],
                ),
                newer_mark,
            )
            .unwrap();
        harness.clock.advance(PINNED_QUIET_DEBOUNCE_MICROS);
        let completed = harness.coordinator.complete(
            first,
            Ok(terminal(
                question_output(&replacement, "fixture pinned answer"),
                complete_usage(),
            )),
        );
        assert!(matches!(completed, CompletionOutcome::Apply(_)));
        let rerun = ticket(harness.coordinator.dispatch_next());
        assert_eq!(rerun.request().call_id.as_str(), "pinned-rerun");
        assert_eq!(harness.coordinator.pinned_run_count(), 2);
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Waiting | DispatchOutcome::Empty | DispatchOutcome::Backpressured
        ));
    }

    #[test]
    fn promoted_final_prevents_continuous_interactive_starvation() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Live, KnownTransport::OpenAiDirect);
        harness.configure(LaneFamily::Final, KnownTransport::OpenAiDirect);
        harness.enable_master();
        let final_row = row(1, "fixture final", 0, 1_000);
        let final_mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&final_row))
            .unwrap();
        harness
            .coordinator
            .submit_final(
                submission(
                    "promoted-final",
                    Lane::Final,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    vec![final_row],
                    Vec::new(),
                ),
                final_mark,
            )
            .unwrap();
        harness.clock.advance(FINAL_PROMOTION_MICROS);
        let live_row = row(2, "fixture live", 1_000, 2_000);
        let live_mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&live_row))
            .unwrap();
        harness
            .coordinator
            .submit_live(
                submission(
                    "new-live",
                    Lane::Live,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    vec![live_row],
                    Vec::new(),
                ),
                live_mark,
            )
            .unwrap();
        harness.clock.advance(LIVE_DEBOUNCE_MICROS);
        let first = ticket(harness.coordinator.dispatch_next());
        assert_eq!(first.request().call_id.as_str(), "promoted-final");
    }

    #[test]
    fn per_provider_backpressure_caps_calls_while_preserving_raw_pending_work() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Live, KnownTransport::OpenAiDirect);
        harness.configure(LaneFamily::Final, KnownTransport::OpenAiDirect);
        harness.enable_master();
        let live_row = row(1, "fixture live capacity", 0, 1_000);
        let mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&live_row))
            .unwrap();
        harness
            .coordinator
            .submit_live(
                submission(
                    "capacity-live",
                    Lane::Live,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    vec![live_row],
                    Vec::new(),
                ),
                mark,
            )
            .unwrap();
        for index in 0..2 {
            let final_row = row(
                u64::try_from(index + 2).unwrap(),
                format!("fixture final capacity {index}"),
                1_000,
                2_000,
            );
            harness
                .coordinator
                .submit_final(
                    submission(
                        &format!("capacity-final-{index}"),
                        Lane::Final,
                        KnownTransport::OpenAiDirect,
                        harness.clock.now(),
                        vec![final_row],
                        Vec::new(),
                    ),
                    mark,
                )
                .unwrap();
        }
        harness.clock.advance(LIVE_DEBOUNCE_MICROS);
        let live = ticket(harness.coordinator.dispatch_next());
        assert_eq!(live.request().call_id.as_str(), "capacity-live");
        let first_final = ticket(harness.coordinator.dispatch_next());
        assert_eq!(first_final.request().call_id.as_str(), "capacity-final-0");
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Backpressured
        ));
        assert!(
            harness.coordinator.queue.iter().any(|queued| queued
                .submission
                .request
                .call_id
                .as_str()
                == "capacity-final-1")
        );
    }

    #[test]
    fn cancellation_discards_late_content_but_retains_terminal_usage_and_cost() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Live, KnownTransport::OpenAiDirect);
        harness.enable_master();
        let target = row(1, "fixture source", 0, 1_000);
        let watermark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&target))
            .unwrap();
        harness
            .coordinator
            .submit_live(
                submission(
                    "late-live",
                    Lane::Live,
                    KnownTransport::OpenAiDirect,
                    harness.clock.now(),
                    vec![target.clone()],
                    Vec::new(),
                ),
                watermark,
            )
            .unwrap();
        harness.clock.advance(LIVE_DEBOUNCE_MICROS);
        let dispatched = ticket(harness.coordinator.dispatch_next());
        harness
            .coordinator
            .on_provider_event(provider_event(
                &dispatched,
                ProviderEventKind::DispatchStarted,
            ))
            .unwrap();
        harness
            .coordinator
            .on_provider_event(provider_event(
                &dispatched,
                ProviderEventKind::UsageProvisional(NormalizedUsage {
                    input_tokens: Some(50),
                    output_tokens: Some(5),
                    usage_complete: true,
                    ..NormalizedUsage::default()
                }),
            ))
            .unwrap();
        harness
            .coordinator
            .apply_patch(ControlPatch::SetLaneEnabled {
                lane: LaneFamily::Live,
                enabled: false,
            })
            .unwrap();
        assert!(dispatched.cancellation().is_cancelled());
        let outcome = harness.coordinator.complete(
            dispatched,
            Ok(terminal(
                rewrite_output(&target, "fixture late replacement"),
                complete_usage(),
            )),
        );
        assert!(matches!(
            outcome,
            CompletionOutcome::Discarded {
                code: ErrorCode::Canceled,
                ..
            }
        ));
        let telemetry = harness
            .store
            .lock()
            .unwrap()
            .telemetry
            .last()
            .unwrap()
            .clone();
        assert!(telemetry.late_content_discarded);
        assert!(telemetry.provider_request_sent);
        assert_eq!(telemetry.usage, complete_usage());
        assert_eq!(telemetry.cost.cost_micros(), Some(140));
        assert!(harness.store.lock().unwrap().commits.is_empty());
        let accounting: Vec<_> = harness
            .coordinator
            .take_events()
            .into_iter()
            .filter_map(|event| match event {
                CoordinatorEventDto::Accounting(event) => Some(event),
                _ => None,
            })
            .collect();
        assert!(
            accounting.iter().any(|event| {
                event.finality == AccountingFinalityDto::Provisional && !event.late
            })
        );
        assert!(
            accounting
                .iter()
                .any(|event| { event.finality == AccountingFinalityDto::Final && event.late })
        );
    }

    #[test]
    fn vertex_warns_once_polls_exactly_and_catches_up_newest_auto_plus_fifo_questions() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Live, KnownTransport::VertexDirect);
        harness.configure(LaneFamily::Final, KnownTransport::VertexDirect);
        harness.configure(LaneFamily::Question, KnownTransport::VertexDirect);
        harness.enable_master();
        let first_row = row(1, "fixture first", 0, 500);
        let first_mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&first_row))
            .unwrap();
        harness
            .coordinator
            .submit_live(
                submission(
                    "vertex-old",
                    Lane::Live,
                    KnownTransport::VertexDirect,
                    harness.clock.now(),
                    vec![first_row],
                    Vec::new(),
                ),
                first_mark,
            )
            .unwrap();
        harness.clock.advance(LIVE_DEBOUNCE_MICROS);
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Waiting
        ));

        let newest_row = row(2, "fixture newest", 500, 1_000);
        let newest_mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&newest_row))
            .unwrap();
        harness
            .coordinator
            .submit_live(
                submission(
                    "vertex-new",
                    Lane::Live,
                    KnownTransport::VertexDirect,
                    harness.clock.now(),
                    vec![newest_row],
                    Vec::new(),
                ),
                newest_mark,
            )
            .unwrap();
        let context = row(3, "fixture context", 1_000, 1_500);
        let context_mark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&context))
            .unwrap();
        for index in 0..2 {
            harness
                .coordinator
                .submit_ad_hoc(
                    submission(
                        &format!("vertex-question-{index}"),
                        Lane::AdHocQuestion,
                        KnownTransport::VertexDirect,
                        harness.clock.now(),
                        Vec::new(),
                        vec![context.clone()],
                    ),
                    context_mark,
                    format!("fixture vertex question {index}"),
                )
                .unwrap();
        }
        let mut expiring_final = submission(
            "vertex-expiring-final",
            Lane::Final,
            KnownTransport::VertexDirect,
            harness.clock.now(),
            vec![context.clone()],
            Vec::new(),
        );
        expiring_final.request.deadline =
            MonotonicDeadline(harness.clock.now().saturating_add(1_000_000));
        harness
            .coordinator
            .submit_final(expiring_final, context_mark)
            .unwrap();
        harness.clock.advance(LIVE_DEBOUNCE_MICROS);
        for _ in 0..4 {
            assert!(matches!(
                harness.coordinator.dispatch_next(),
                DispatchOutcome::Waiting
            ));
        }
        let notices: Vec<_> = harness
            .coordinator
            .take_events()
            .into_iter()
            .filter_map(|event| match event {
                CoordinatorEventDto::Notice(notice) => Some(notice),
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].role, "alert");
        assert_eq!(notices[0].visible_message, "gcloud token isn't armed");

        let first_attempt = harness.coordinator.drive_vertex().unwrap();
        harness
            .coordinator
            .complete_vertex(first_attempt, VertexResolutionOutcome::Unarmed)
            .unwrap();
        harness.clock.advance(4_999_999);
        assert!(harness.coordinator.drive_vertex().is_none());
        harness.clock.advance(1);
        let second_attempt = harness.coordinator.drive_vertex().unwrap();
        harness
            .coordinator
            .complete_vertex(
                second_attempt,
                VertexResolutionOutcome::Ready {
                    expires_at_unix_ms: Some(1_900_000_000_000),
                },
            )
            .unwrap();
        assert!(
            !harness.coordinator.queue.iter().any(|queued| queued
                .submission
                .request
                .call_id
                .as_str()
                == "vertex-expiring-final")
        );
        assert!(
            harness
                .store
                .lock()
                .unwrap()
                .journal
                .iter()
                .any(|(call, state)| {
                    call.as_str() == "vertex-expiring-final"
                        && *state == FinalJournalState::Abandoned
                })
        );

        let live = ticket(harness.coordinator.dispatch_next());
        assert_eq!(live.request().call_id.as_str(), "vertex-new");
        let question = ticket(harness.coordinator.dispatch_next());
        assert_eq!(question.request().call_id.as_str(), "vertex-question-0");
        assert_eq!(
            harness
                .coordinator
                .question_summaries()
                .iter()
                .filter(|question| {
                    matches!(
                        question.status,
                        QuestionStatusDto::Running
                            | QuestionStatusDto::Queued
                            | QuestionStatusDto::WaitingForCredential
                    )
                })
                .count(),
            2
        );
    }

    #[test]
    fn vertex_ready_catches_up_every_final_chunk_sharing_the_atomic_fence() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Final, KnownTransport::VertexDirect);
        harness.enable_master();
        let first_row = row(1, "first final chunk", 0, 1_000);
        let second_row = row(2, "second final chunk", 1_000, 2_000);
        let watermark = harness
            .coordinator
            .observe_finalized_rows(&[first_row.clone(), second_row.clone()])
            .unwrap();
        harness
            .coordinator
            .submit_final(
                submission(
                    "vertex-final-a",
                    Lane::Final,
                    KnownTransport::VertexDirect,
                    harness.clock.now(),
                    vec![first_row],
                    Vec::new(),
                ),
                watermark,
            )
            .unwrap();
        harness
            .coordinator
            .submit_final(
                submission(
                    "vertex-final-b",
                    Lane::Final,
                    KnownTransport::VertexDirect,
                    harness.clock.now(),
                    vec![second_row],
                    Vec::new(),
                ),
                watermark,
            )
            .unwrap();
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Waiting
        ));
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Waiting
        ));
        let attempt = harness.coordinator.drive_vertex().unwrap();
        harness
            .coordinator
            .complete_vertex(
                attempt,
                VertexResolutionOutcome::Ready {
                    expires_at_unix_ms: None,
                },
            )
            .unwrap();
        let first = ticket(harness.coordinator.dispatch_next());
        let second = ticket(harness.coordinator.dispatch_next());
        let calls = [
            first.request().call_id.as_str(),
            second.request().call_id.as_str(),
        ];
        assert_eq!(calls, ["vertex-final-a", "vertex-final-b"]);
    }

    #[test]
    fn vertex_token_ready_and_service_failure_remain_distinct() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Live, KnownTransport::VertexDirect);
        harness.enable_master();
        let attempt = harness.coordinator.drive_vertex().unwrap();
        harness
            .coordinator
            .complete_vertex(
                attempt,
                VertexResolutionOutcome::Ready {
                    expires_at_unix_ms: None,
                },
            )
            .unwrap();
        harness.providers.lock().unwrap().catalog_error = Some(ErrorCode::Permission);
        let target = row(1, "fixture service", 0, 1_000);
        let watermark = harness
            .coordinator
            .observe_finalized_rows(std::slice::from_ref(&target))
            .unwrap();
        harness
            .coordinator
            .submit_live(
                submission(
                    "vertex-service",
                    Lane::Live,
                    KnownTransport::VertexDirect,
                    harness.clock.now(),
                    vec![target],
                    Vec::new(),
                ),
                watermark,
            )
            .unwrap();
        harness.clock.advance(LIVE_DEBOUNCE_MICROS);
        assert!(matches!(
            harness.coordinator.dispatch_next(),
            DispatchOutcome::Failed {
                code: ErrorCode::Permission,
                ..
            }
        ));
        let state = harness
            .coordinator
            .provider_states()
            .find(|state| state.descriptor.transport.as_str() == "vertex_api")
            .unwrap();
        assert!(matches!(state.credential, CredentialState::Ready { .. }));
        assert_eq!(state.service_error, Some(ErrorCode::Permission));
    }

    #[test]
    fn ambiguous_recovery_never_schedules_an_automatic_paid_repeat() {
        let mut harness = Harness::new();
        let call_id = CallId::new("ambiguous-call").unwrap();
        harness.store.lock().unwrap().recovery = Some(FinalRecoveryRecord {
            call_id: call_id.clone(),
            state: FinalJournalState::Dispatched,
        });
        assert_eq!(
            harness
                .coordinator
                .recover_final("recording-fixture")
                .unwrap(),
            FinalRecoveryDirective::Fallback {
                call_id: call_id.clone(),
                code: ErrorCode::AmbiguousDispatch,
                explicit_retry_required: true,
            }
        );
        assert!(harness.coordinator.queue.is_empty());

        harness.store.lock().unwrap().recovery = Some(FinalRecoveryRecord {
            call_id: call_id.clone(),
            state: FinalJournalState::ResultCached,
        });
        assert_eq!(
            harness
                .coordinator
                .recover_final("recording-fixture")
                .unwrap(),
            FinalRecoveryDirective::ResumeEncryptedResult { call_id }
        );
        assert!(harness.coordinator.queue.is_empty());
    }

    #[test]
    fn dto_surfaces_are_secret_free_content_debug_is_redacted_and_hot_path_is_nonblocking() {
        let mut harness = Harness::new();
        harness.configure(LaneFamily::Live, KnownTransport::OpenAiDirect);
        harness.enable_master();
        let serialized = serde_json::to_string(harness.coordinator.control_snapshot()).unwrap();
        for forbidden in [
            "api_key",
            "access_token",
            "refresh_token",
            "credential_path",
            "provider_body",
        ] {
            assert!(!serialized.contains(forbidden), "{serialized}");
        }
        let sensitive_fixture = "fixture-private-transcript-marker";
        let target = row(1, sensitive_fixture, 0, 1_000);
        let request = submission(
            "redacted",
            Lane::Live,
            KnownTransport::OpenAiDirect,
            harness.clock.now(),
            vec![target.clone()],
            Vec::new(),
        );
        assert!(!format!("{request:?}").contains(sensitive_fixture));
        let apply = ApplyReady {
            call_id: CallId::new("redacted-apply").unwrap(),
            lane: Lane::Live,
            fence: dummy_fence(),
            output: ValidatedOutput::Rewrite { rows: vec![target] },
            recovery_committed: false,
        };
        assert!(!format!("{apply:?}").contains(sensitive_fixture));

        let (ingress, receiver) = CoordinatorIngress::bounded(1);
        let command = || HotPathCommand::FinalizedRows {
            recording_id: "fixture-recording".into(),
            rows: Vec::new(),
        };
        ingress.try_send(command()).unwrap();
        assert_eq!(ingress.try_send(command()), Err(IngressError::Full));
        drop(receiver);
        assert_eq!(ingress.try_send(command()), Err(IngressError::Disconnected));

        assert_eq!(
            KnownTransport::ClaudeSubscription.descriptor().support_tier,
            SupportTier::Blocked
        );
        assert_eq!(
            KnownTransport::CodexAppServer.descriptor().support_tier,
            SupportTier::Experimental
        );
        assert_eq!(
            KnownTransport::BedrockRuntime.descriptor().support_tier,
            SupportTier::Documented
        );
        assert_eq!(provider_support_catalog().len(), 6);
        let claude_state = harness
            .coordinator
            .provider_states()
            .find(|state| state.descriptor.transport.as_str() == "claude_subscription")
            .unwrap();
        assert!(matches!(
            claude_state.credential,
            CredentialState::Unsupported {
                code: ErrorCode::PolicyBlocked
            }
        ));
        let codex = KnownTransport::CodexAppServer.descriptor();
        let codex_selection = LaneSelectionDto {
            provider: Some(codex.provider),
            transport: Some(codex.transport),
            model: Some(ModelId::new("fixture-model-v1").unwrap()),
            cache_policy: fixture_cache_policy(),
        };
        assert_eq!(
            harness
                .coordinator
                .apply_patch(ControlPatch::SetLaneSelection {
                    lane: LaneFamily::Live,
                    selection: codex_selection,
                }),
            Err(ControlError::ExperimentalProviderOff)
        );
    }

    #[test]
    fn pricing_fixture_remains_truthful_for_unknown_and_negative_usage() {
        let catalog = pricing_catalog();
        let descriptor = KnownTransport::OpenAiDirect.descriptor();
        let model = ModelId::new("fixture-model-v1").unwrap();
        let unknown = catalog
            .estimate(
                PricingQuery {
                    provider: &descriptor.provider,
                    exact_model_id: &model,
                    region: None,
                    support_tier: descriptor.support_tier,
                    dispatch_unix_ms: 1,
                    billing_basis: BillingBasis::MeteredEstimate,
                },
                &NormalizedUsage::unknown(),
            )
            .unwrap();
        assert_eq!(unknown.render(), "Cost unavailable");
        assert_eq!(
            NormalizedUsage::try_from(RawUsage {
                input_tokens: Some(-1),
                ..RawUsage::default()
            }),
            Err(PricingError::NegativeUsage)
        );
    }
}
