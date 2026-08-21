//! Tauri/backend wiring for the hosted post-processing coordinator.
//!
//! The coordinator and every content-bearing request remain on a dedicated thread. Live ASR publishes raw
//! rows first, then uses the coordinator's bounded `try_send` ingress. Batch/live final callers may wait only
//! after ASR has completed and the raw transcript is already recoverable. Production provider execution is
//! deliberately deny-by-default until an approved credential/store factory is installed; this wiring still
//! exposes truthful provider posture, Vertex arming/catch-up, controls, history, and hermetic injection seams.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use corti_core::DiarizedTranscript;
use corti_postprocess::{
    BillingBasis, CacheObservation, CachePolicy, CallId, CancellationReason, CanonicalPrompt,
    ConnectionScopeId, CostEstimate, CredentialState, DigestKey, ErrorCode, HostedRequest, Lane,
    LocalCacheMode, ModelCatalog, ModelId, MonotonicDeadline, OUTPUT_SCHEMA_VERSION,
    PROMPT_TEMPLATE_VERSION, PricingCatalog, PricingError, PricingQuery, ProcessEpoch,
    ProviderCacheMode, ProviderDescriptor, ProviderEventSink, ProviderId, ProviderScope,
    ProviderTerminal, RequestFence, RequestGroupId, RequestKey, RequestKeyMaterial, RowId,
    SupportTier, TargetId, TranscriptRow, TransportId, WordBankDocument,
};
use corti_postprocess_providers::{
    ANTHROPIC_MESSAGES_ADAPTER_VERSION, OPENAI_RESPONSES_ADAPTER_VERSION,
    VERTEX_REST_ADAPTER_VERSION, VertexResolutionAttempt, VertexResolutionOutcome,
};
use corti_queue::{
    PostprocessCacheSource, PostprocessCallRecord, PostprocessCost, PostprocessOutcome, Queue,
};
use corti_vagus::provenance::{
    AppliedCacheSource, AppliedPostprocessDetails, AppliedPostprocessProvenance,
    AppliedPostprocessState, AppliedWordBankProvenance, FinalPostprocessOutcome,
    ProvenanceFingerprint,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

use crate::live_view::LiveTranscriptStore;
use crate::pipeline::PipelineMsg;
use crate::postprocess::{
    CompletionOutcome, ControlError, ControlPatch, ControlPersistence, ControlSnapshotDto,
    CoordinatorClock, CoordinatorEventDto, CoordinatorIngress, DispatchOutcome, DispatchTicket,
    EncryptedPostprocessStore, ExactLookup, FinalJournalBoundary, FinalJournalState,
    FinalRecoveryRecord, HotPathCommand, IngressError, LaneControlDto, LaneFamily,
    LaneSelectionDto, PatchOutcome, PostprocessCoordinator, ProviderAccess, ProviderStateDto,
    QuestionStatusDto, RequestSubmission, StoreCommit, SubmitError, TerminalOutcomeDto,
    TerminalTelemetryDto, TranscriptWatermark,
};
use crate::postprocess_config::{
    EGRESS_DISCLOSURE_VERSION, HostedPreferences, PINNED_AUTO_DISCLOSURE_VERSION,
    ProviderScopePreferences,
};
use crate::private_file::{atomic_write_private, read_private};

pub(crate) const HOSTED_STATE_CHANGED_EVENT: &str = "hosted-state-changed";
const SERVICE_COMMAND_CAPACITY: usize = 256;
const SERVICE_TICK: Duration = Duration::from_millis(20);
const REQUEST_CHUNK_BYTES: usize = 64 * 1024;
const MAX_FINAL_CHUNKS: usize = 64;
const MAX_LIVE_TARGET_BYTES: usize = 4 * 1024;
const MAX_LIVE_TARGET_ROWS: usize = 8;
const MAX_CONTEXT_ROWS: usize = 8;
const MAX_SESSION_LEDGER_BYTES: usize = 16 * 1024 * 1024;
const OUTBOX_SCHEMA: u32 = 1;
const MAX_OUTBOX_BYTES: usize = 16 * 1024 * 1024;
const FINGERPRINT_DOMAIN: &[u8] = b"corti-app-provenance-v1\0";

/// Managed Tauri state. The handle is cloneable; the coordinator itself never leaves its owner thread.
pub(crate) struct HostedState {
    handle: HostedHandle,
}

impl HostedState {
    pub(crate) fn handle(&self) -> &HostedHandle {
        &self.handle
    }
}

/// Cloneable app/backend seam used by live ASR, the serial pipeline, and Tauri commands.
#[derive(Clone)]
pub(crate) struct HostedHandle {
    command_tx: SyncSender<ServiceCommand>,
    ingress: CoordinatorIngress,
    snapshot: Arc<Mutex<HostedSettingsDto>>,
    ingress_incomplete: Arc<AtomicBool>,
    outbox: Arc<TelemetryOutbox>,
}

impl fmt::Debug for HostedHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HostedHandle(<managed>)")
    }
}

/// Final result consumed at the narrow post-ASR filing boundary.
pub(crate) struct SettledFinalTranscript {
    pub(crate) transcript: DiarizedTranscript,
    pub(crate) applied_postprocess: AppliedPostprocessProvenance,
    pub(crate) source_transcript_fingerprint: Option<ProvenanceFingerprint>,
    pub(crate) call_ids: Vec<CallId>,
    pub(crate) hosted_text_applied: bool,
    pub(crate) fallback_code: Option<ErrorCode>,
}

impl fmt::Debug for SettledFinalTranscript {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettledFinalTranscript")
            .field("segment_count", &self.transcript.segments.len())
            .field("applied_postprocess", &self.applied_postprocess)
            .field(
                "source_transcript_fingerprint",
                &self.source_transcript_fingerprint,
            )
            .field("call_ids", &self.call_ids)
            .field("hosted_text_applied", &self.hosted_text_applied)
            .field("fallback_code", &self.fallback_code)
            .finish()
    }
}

impl HostedHandle {
    pub(crate) fn snapshot(&self) -> HostedSettingsDto {
        self.snapshot.lock().unwrap().clone()
    }

    /// Start a recording-scoped hosted session off the capture path. Failure simply leaves hosted work off;
    /// raw ASR and note durability are independent.
    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub(crate) fn begin_live_session(&self, recording_id: &str) -> Result<(), ErrorCode> {
        self.ingress_incomplete.store(false, Ordering::Release);
        let (reply, _discarded_reply) = std::sync::mpsc::channel();
        self.command_tx
            .try_send(ServiceCommand::BeginSession {
                recording_id: recording_id.to_owned(),
                reply,
            })
            .map_err(|error| {
                self.ingress_incomplete.store(true, Ordering::Release);
                match error {
                    TrySendError::Full(_) => ErrorCode::RateLimited,
                    TrySendError::Disconnected(_) => ErrorCode::Internal,
                }
            })
    }

    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub(crate) fn end_live_session(&self, recording_id: &str) {
        let _ = self.command_tx.try_send(ServiceCommand::EndSession {
            recording_id: recording_id.to_owned(),
        });
    }

    /// Raw/UI publication must happen before this call. This method can never block.
    pub(crate) fn try_observe_finalized_rows(
        &self,
        recording_id: &str,
        rows: Vec<TranscriptRow>,
    ) -> Result<(), IngressError> {
        match self.ingress.try_send(HotPathCommand::FinalizedRows {
            recording_id: recording_id.to_owned(),
            rows,
        }) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.ingress_incomplete.store(true, Ordering::Release);
                Err(error)
            }
        }
    }

    /// Run the stronger all-or-nothing final pass. This is called only after ASR and may wait up to the
    /// configured final deadline. Every error returns the immutable raw transcript with typed no-apply
    /// provenance; it never turns a hosted failure into a pipeline failure.
    pub(crate) fn finalize(
        &self,
        recording_id: &str,
        transcript: DiarizedTranscript,
        live_session: bool,
    ) -> SettledFinalTranscript {
        let raw = transcript.clone();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if self
            .send(ServiceCommand::Finalize {
                recording_id: recording_id.to_owned(),
                transcript,
                live_session,
                reply: reply_tx,
            })
            .is_err()
        {
            return fallback_final(raw, ErrorCode::Internal, Vec::new(), None);
        }
        let wait = self
            .snapshot()
            .final_deadline_seconds
            .saturating_add(2)
            .max(2);
        match reply_rx.recv_timeout(Duration::from_secs(u64::from(wait))) {
            Ok(result) => result,
            Err(_) => fallback_final(raw, ErrorCode::Timeout, Vec::new(), None),
        }
    }

    /// Strict application fence immediately before a checkpoint/note rewrite.
    pub(crate) fn mark_final_applied(&self, call_ids: &[CallId]) -> Result<(), ErrorCode> {
        if call_ids.is_empty() {
            return Ok(());
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send(ServiceCommand::MarkFinalApplied {
            call_ids: call_ids.to_vec(),
            reply: reply_tx,
        })?;
        reply_rx.recv().unwrap_or(Err(ErrorCode::Internal))
    }

    pub(crate) fn abandon_final_result(&self, call_ids: &[CallId]) {
        if call_ids.is_empty() {
            return;
        }
        let _ = self.command_tx.try_send(ServiceCommand::AbandonFinal {
            call_ids: call_ids.to_vec(),
        });
    }

    /// Acknowledge only after the FilingCheckpoint or live-note final state is durable.
    pub(crate) fn mark_final_checkpointed(&self, call_ids: &[CallId]) {
        if call_ids.is_empty() {
            return;
        }
        let _ = self
            .command_tx
            .try_send(ServiceCommand::MarkFinalCheckpointed {
                call_ids: call_ids.to_vec(),
            });
    }

    pub(crate) fn import_outbox(&self, queue: &Queue) -> Result<usize> {
        import_outbox(queue, &self.outbox)
    }

    #[cfg(test)]
    fn patch_for_test(
        &self,
        request: HostedPatchRequest,
    ) -> Result<HostedMutationResult, ErrorCode> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send(ServiceCommand::Patch {
            request,
            reply: reply_tx,
        })?;
        reply_rx.recv().unwrap_or(Err(ErrorCode::Internal))
    }

    fn send(&self, command: ServiceCommand) -> Result<(), ErrorCode> {
        self.command_tx
            .send(command)
            .map_err(|_| ErrorCode::Internal)
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HostedWordBankDto {
    pub(crate) revision: u64,
    pub(crate) entries: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HostedProviderScopeDto {
    pub(crate) provider: String,
    pub(crate) transport: String,
    pub(crate) configured: bool,
    pub(crate) alias: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) quota_project: Option<String>,
}

/// Complete secret-free Settings projection. It cannot represent a key, token, credential path, transcript,
/// question, answer, or provider body.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct HostedSettingsDto {
    pub(crate) state_revision: u64,
    pub(crate) preferences_revision: u64,
    pub(crate) control: ControlSnapshotDto,
    pub(crate) providers: Vec<ProviderStateDto>,
    pub(crate) scopes: Vec<HostedProviderScopeDto>,
    pub(crate) default_steering: String,
    pub(crate) word_bank: HostedWordBankDto,
    pub(crate) final_deadline_seconds: u32,
    pub(crate) show_history_diagnostics: bool,
    pub(crate) show_live_metrics_by_default: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostedLaneDto {
    Live,
    Final,
    Question,
}

impl From<HostedLaneDto> for LaneFamily {
    fn from(value: HostedLaneDto) -> Self {
        match value {
            HostedLaneDto::Live => Self::Live,
            HostedLaneDto::Final => Self::Final,
            HostedLaneDto::Question => Self::Question,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HostedSelectionInput {
    pub(crate) provider: Option<String>,
    pub(crate) transport: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) local_cache: LocalCacheMode,
    pub(crate) provider_cache: ProviderCacheMode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::enum_variant_names)] // `set_*` is the stable command vocabulary exposed to the webview.
pub(crate) enum HostedPatchInput {
    SetEgressAcknowledged {
        acknowledged: bool,
    },
    SetMaster {
        enabled: bool,
    },
    SetLaneEnabled {
        lane: HostedLaneDto,
        enabled: bool,
    },
    SetLaneSelection {
        lane: HostedLaneDto,
        selection: HostedSelectionInput,
    },
    SetPinnedAuto {
        enabled: bool,
        acknowledged: bool,
    },
    SetCodexExperimentalApproved {
        approved: bool,
    },
    SetDisplayPreferences {
        show_history_diagnostics: bool,
        show_live_metrics_by_default: bool,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HostedPatchRequest {
    pub(crate) observed_state_revision: u64,
    pub(crate) patch: HostedPatchInput,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum HostedMutationResult {
    Applied {
        settings: HostedSettingsDto,
    },
    Unchanged {
        settings: HostedSettingsDto,
    },
    Conflict {
        settings: HostedSettingsDto,
    },
    DisabledForSession {
        settings: HostedSettingsDto,
        code: ErrorCode,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SteeringUpdateRequest {
    pub(crate) observed_state_revision: u64,
    pub(crate) text: String,
    pub(crate) persist_as_default: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WordBankUpdateRequest {
    pub(crate) observed_state_revision: u64,
    pub(crate) entries: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProviderRefreshRequest {
    pub(crate) provider: String,
    pub(crate) transport: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProviderScopeUpdateRequest {
    pub(crate) observed_state_revision: u64,
    pub(crate) provider: String,
    pub(crate) transport: String,
    pub(crate) alias: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) quota_project: Option<String>,
}

#[derive(Clone, Serialize)]
pub(crate) struct AssistantExchangeDto {
    pub(crate) call_id: CallId,
    pub(crate) as_of_revision: u64,
    pub(crate) status: QuestionStatusDto,
    pub(crate) error: Option<ErrorCode>,
    pub(crate) question: String,
    pub(crate) answer: Option<String>,
    pub(crate) cost_label: Option<String>,
}

impl fmt::Debug for AssistantExchangeDto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssistantExchangeDto")
            .field("call_id", &self.call_id)
            .field("as_of_revision", &self.as_of_revision)
            .field("status", &self.status)
            .field("error", &self.error)
            .field("question_bytes", &self.question.len())
            .field("answer_bytes", &self.answer.as_ref().map(String::len))
            .field("cost_label", &self.cost_label)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AssistantSnapshotDto {
    pub(crate) pinned_run_count: u64,
    pub(crate) pinned: Option<AssistantExchangeDto>,
    pub(crate) exchanges: Vec<AssistantExchangeDto>,
}

#[tauri::command]
pub(crate) fn get_hosted_settings(state: State<'_, HostedState>) -> HostedSettingsDto {
    state.handle.snapshot()
}

#[tauri::command]
pub(crate) fn patch_hosted_settings(
    request: HostedPatchRequest,
    state: State<'_, HostedState>,
) -> Result<HostedMutationResult, String> {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::Patch {
            request,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

#[tauri::command]
pub(crate) fn update_hosted_steering(
    request: SteeringUpdateRequest,
    state: State<'_, HostedState>,
) -> Result<HostedMutationResult, String> {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::UpdateSteering {
            request,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

#[tauri::command]
pub(crate) fn replace_hosted_word_bank(
    request: WordBankUpdateRequest,
    state: State<'_, HostedState>,
) -> Result<HostedMutationResult, String> {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::ReplaceWordBank {
            request,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

#[tauri::command]
pub(crate) fn update_hosted_provider_scope(
    request: ProviderScopeUpdateRequest,
    state: State<'_, HostedState>,
) -> Result<HostedMutationResult, String> {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::UpdateProviderScope {
            request,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

#[tauri::command]
pub(crate) fn refresh_hosted_provider(
    request: ProviderRefreshRequest,
    state: State<'_, HostedState>,
) -> Result<ProviderStateDto, String> {
    let provider = ProviderId::new(request.provider).map_err(|_| "invalid provider".to_string())?;
    let transport =
        TransportId::new(request.transport).map_err(|_| "invalid transport".to_string())?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::RefreshProvider {
            provider,
            transport,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

#[tauri::command]
pub(crate) fn submit_hosted_question(
    question: String,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<CallId, String> {
    require_live_window(&window)?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::SubmitAdHoc {
            question,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

#[tauri::command]
pub(crate) fn cancel_hosted_question(
    call_id: String,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    require_live_window(&window)?;
    let call_id = CallId::new(call_id).map_err(|_| "invalid call id".to_string())?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::CancelQuestion {
            call_id,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    if reply_rx.recv().unwrap_or(false) {
        Ok(())
    } else {
        Err("question is no longer cancelable".to_string())
    }
}

#[tauri::command]
pub(crate) fn set_hosted_pinned_question(
    template: String,
    state: State<'_, HostedState>,
) -> Result<(), String> {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::SetPinnedTemplate {
            template,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

#[tauri::command]
pub(crate) fn get_hosted_assistant(
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<AssistantSnapshotDto, String> {
    require_live_window(&window)?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::AssistantSnapshot { reply: reply_tx })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())
}

fn require_live_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    if window.label() == "live" {
        Ok(())
    } else {
        Err("assistant content is available only in the live window".to_string())
    }
}

fn sanitized_error(code: ErrorCode) -> String {
    code.to_string()
}

/// Start production wiring. No production executor can make a provider request in this slice: documented
/// transports report truthful unarmed/absent state, Codex stays experimental/off, and Claude subscription
/// remains blocked. Tests inject every credential/catalog/transport result explicitly.
pub(crate) fn start(
    app: AppHandle,
    live_view: LiveTranscriptStore,
    pipeline_tx: Sender<PipelineMsg>,
) -> Result<(HostedState, HostedHandle)> {
    let preferences = HostedPreferences::load().unwrap_or_else(|error| {
        tracing::warn!(
            target: "corti::hosted",
            error = %format!("{error:#}"),
            "hosted preferences are unreadable; all hosted egress remains off"
        );
        HostedPreferences::default()
    });
    let word_bank = crate::word_bank::load().unwrap_or_else(|error| {
        tracing::warn!(
            target: "corti::hosted",
            error = %format!("{error:#}"),
            "word bank is unreadable; hosted egress remains safely unavailable for this run"
        );
        WordBankDocument::empty()
    });
    let outbox = Arc::new(TelemetryOutbox::open(default_outbox_path()?)?);
    let notifier: EventNotifier = Arc::new(move |event| {
        let _ = app.emit(HOSTED_STATE_CHANGED_EVENT, event);
    });
    start_with_components(
        preferences,
        word_bank,
        live_view,
        pipeline_tx,
        outbox,
        Arc::new(DenyExecutor),
        Box::new(UnavailableProviders),
        Arc::new(NoPricing),
        Arc::new(UnarmedVertex),
        notifier,
        system_digest_key()?,
        process_epoch()?,
        true,
        None,
    )
}

type EventNotifier = Arc<dyn Fn(&CoordinatorEventDto) + Send + Sync>;

#[allow(clippy::too_many_arguments)]
fn start_with_components(
    preferences: HostedPreferences,
    word_bank: WordBankDocument,
    live_view: LiveTranscriptStore,
    pipeline_tx: Sender<PipelineMsg>,
    outbox: Arc<TelemetryOutbox>,
    executor: Arc<dyn TicketExecutor>,
    providers: Box<dyn ProviderAccess>,
    pricing: Arc<dyn PricingCatalog>,
    vertex_resolver: Arc<dyn VertexResolver>,
    notifier: EventNotifier,
    digest_key: DigestKey,
    process_epoch: ProcessEpoch,
    persist_to_disk: bool,
    clock_override: Option<Arc<dyn CoordinatorClock>>,
) -> Result<(HostedState, HostedHandle)> {
    let preferences = Arc::new(Mutex::new(preferences));
    let initial_control = control_from_preferences(
        process_epoch,
        &preferences.lock().unwrap(),
        word_bank.revision(),
    );
    let initial_settings = settings_snapshot(
        1,
        &preferences.lock().unwrap(),
        &word_bank,
        &initial_control,
        &initial_provider_states(),
    );
    let observed_pinned_revision = initial_control.pinned_question_revision;
    let snapshot = Arc::new(Mutex::new(initial_settings));
    let persistence = Box::new(HostedControlPersistence {
        preferences: preferences.clone(),
        persist_to_disk,
    });
    let store = Box::new(RuntimeStore::new(outbox.clone(), pipeline_tx.clone()));
    let clock: Arc<dyn CoordinatorClock> =
        clock_override.unwrap_or_else(|| Arc::new(SystemCoordinatorClock::new()));
    let pinned = preferences
        .lock()
        .unwrap()
        .values()
        .pinned_question_template
        .clone();
    let coordinator = PostprocessCoordinator::new_with_snapshot(
        initial_control,
        (!pinned.trim().is_empty()).then_some(pinned),
        clock.clone(),
        persistence,
        store,
        providers,
        pricing,
    );
    let (ingress, ingress_rx) = CoordinatorIngress::standard();
    let (command_tx, command_rx) = sync_channel(SERVICE_COMMAND_CAPACITY);
    let (worker_tx, worker_rx) = std::sync::mpsc::channel();
    let (vertex_tx, vertex_rx) = std::sync::mpsc::channel();
    let (event_sink, provider_event_rx) = crate::postprocess::BoundedProviderEventSink::channel();
    let ingress_incomplete = Arc::new(AtomicBool::new(false));
    let handle = HostedHandle {
        command_tx,
        ingress,
        snapshot: snapshot.clone(),
        ingress_incomplete: ingress_incomplete.clone(),
        outbox,
    };
    let service = Service {
        coordinator,
        clock,
        preferences,
        word_bank,
        digest_key: Arc::new(digest_key),
        live_view,
        ingress_incomplete,
        current_recording: None,
        ledger: Vec::new(),
        ledger_bytes: 0,
        session_steering: None,
        state_revision: 1,
        snapshot,
        notifier,
        executor,
        vertex_resolver,
        worker_tx,
        worker_rx,
        vertex_tx,
        vertex_rx,
        provider_event_rx,
        event_sink,
        next_id: 1,
        pending_finals: HashMap::new(),
        final_by_call: HashMap::new(),
        call_cache: HashMap::new(),
        observed_pinned_revision,
        pinned_exchange: None,
        persist_to_disk,
    };
    std::thread::Builder::new()
        .name("corti-hosted-control".into())
        .spawn(move || service.run(command_rx, ingress_rx))
        .context("spawning hosted coordinator")?;
    Ok((
        HostedState {
            handle: handle.clone(),
        },
        handle,
    ))
}

struct SystemCoordinatorClock {
    started: Instant,
}

impl SystemCoordinatorClock {
    fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl corti_postprocess_providers::Clock for SystemCoordinatorClock {
    fn monotonic_micros(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

impl CoordinatorClock for SystemCoordinatorClock {
    fn unix_millis(&self) -> i64 {
        unix_millis()
    }
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

fn process_epoch() -> Result<ProcessEpoch> {
    let mut bytes = [0u8; 8];
    random_bytes(&mut bytes)?;
    Ok(ProcessEpoch(u64::from_le_bytes(bytes).max(1)))
}

fn system_digest_key() -> Result<DigestKey> {
    let mut bytes = [0u8; 32];
    random_bytes(&mut bytes)?;
    Ok(DigestKey::new(bytes))
}

fn random_bytes(bytes: &mut [u8]) -> Result<()> {
    use std::io::Read as _;
    std::fs::File::open("/dev/urandom")
        .context("opening system random source")?
        .read_exact(bytes)
        .context("reading system random source")
}

fn control_from_preferences(
    process_epoch: ProcessEpoch,
    preferences: &HostedPreferences,
    bank_revision: u64,
) -> ControlSnapshotDto {
    let values = preferences.values();
    let revision = preferences.revision().saturating_add(1).max(1);
    let lane = |source: &crate::postprocess_config::LanePreferences| LaneControlDto {
        enabled: source.enabled,
        revision,
        selection: LaneSelectionDto {
            provider: source.provider.clone(),
            transport: source.transport.clone(),
            model: source.model.clone(),
            cache_policy: CachePolicy {
                local: source.local_cache,
                provider: source.provider_cache,
            },
        },
    };
    ControlSnapshotDto {
        process_epoch,
        session_generation: 1,
        control_revision: revision,
        steering_revision: revision,
        bank_revision,
        pinned_question_revision: (!values.pinned_question_template.trim().is_empty()) as u64,
        master_enabled: values.master_enabled,
        egress_acknowledged: values.egress_acknowledgement_version
            == Some(EGRESS_DISCLOSURE_VERSION),
        pinned_auto_enabled: values.pinned_auto_enabled,
        codex_experimental_approved: values.providers.codex_experimental_approved,
        live: lane(&values.live),
        final_lane: lane(&values.final_lane),
        questions: lane(&values.questions),
    }
}

struct HostedControlPersistence {
    preferences: Arc<Mutex<HostedPreferences>>,
    persist_to_disk: bool,
}

impl ControlPersistence for HostedControlPersistence {
    fn persist(&mut self, snapshot: &ControlSnapshotDto) -> Result<(), ErrorCode> {
        let current = self.preferences.lock().unwrap().clone();
        let next = current
            .revise(|values| {
                values.master_enabled = snapshot.master_enabled;
                values.egress_acknowledgement_version = snapshot
                    .egress_acknowledged
                    .then_some(EGRESS_DISCLOSURE_VERSION);
                copy_lane(&snapshot.live, &mut values.live);
                copy_lane(&snapshot.final_lane, &mut values.final_lane);
                copy_lane(&snapshot.questions, &mut values.questions);
                values.pinned_auto_enabled = snapshot.pinned_auto_enabled;
                values.providers.codex_experimental_approved = snapshot.codex_experimental_approved;
            })
            .map_err(|_| ErrorCode::Cache)?;
        if self.persist_to_disk {
            next.save().map_err(|_| ErrorCode::Cache)?;
        }
        *self.preferences.lock().unwrap() = next;
        Ok(())
    }
}

fn copy_lane(source: &LaneControlDto, target: &mut crate::postprocess_config::LanePreferences) {
    target.enabled = source.enabled;
    target.provider = source.selection.provider.clone();
    target.transport = source.selection.transport.clone();
    target.model = source.selection.model.clone();
    target.local_cache = source.selection.cache_policy.local;
    target.provider_cache = source.selection.cache_policy.provider;
}

struct UnavailableProviders;

impl ProviderAccess for UnavailableProviders {
    fn descriptor(
        &mut self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> Option<ProviderDescriptor> {
        crate::postprocess::provider_support_catalog()
            .into_iter()
            .find(|candidate| &candidate.provider == provider && &candidate.transport == transport)
    }

    fn credential_state(
        &mut self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> CredentialState {
        let descriptor = self.descriptor(provider, transport);
        match descriptor.map(|value| value.support_tier) {
            Some(SupportTier::Blocked | SupportTier::Experimental) => {
                CredentialState::Unsupported {
                    code: ErrorCode::PolicyBlocked,
                }
            }
            Some(SupportTier::Documented) => CredentialState::Absent,
            None => CredentialState::Unsupported {
                code: ErrorCode::PolicyBlocked,
            },
        }
    }

    fn catalog(
        &mut self,
        _provider: &ProviderId,
        _transport: &TransportId,
        _scope: &ProviderScope,
    ) -> Result<ModelCatalog, corti_postprocess::PostprocessError> {
        Err(ErrorCode::AuthUnarmed.into())
    }
}

trait TicketExecutor: Send + Sync {
    fn execute(
        &self,
        ticket: &DispatchTicket,
        sink: &dyn ProviderEventSink,
    ) -> Result<ProviderTerminal, corti_postprocess::PostprocessError>;
}

struct DenyExecutor;

impl TicketExecutor for DenyExecutor {
    fn execute(
        &self,
        _ticket: &DispatchTicket,
        _sink: &dyn ProviderEventSink,
    ) -> Result<ProviderTerminal, corti_postprocess::PostprocessError> {
        Err(ErrorCode::PolicyBlocked.into())
    }
}

trait VertexResolver: Send + Sync {
    fn resolve(&self, attempt: &VertexResolutionAttempt) -> VertexResolutionOutcome;
}

struct UnarmedVertex;

impl VertexResolver for UnarmedVertex {
    fn resolve(&self, _attempt: &VertexResolutionAttempt) -> VertexResolutionOutcome {
        VertexResolutionOutcome::Unarmed
    }
}

struct NoPricing;

impl PricingCatalog for NoPricing {
    fn estimate(
        &self,
        _query: PricingQuery<'_>,
        _usage: &corti_postprocess::NormalizedUsage,
    ) -> Result<CostEstimate, PricingError> {
        Ok(CostEstimate::unavailable())
    }
}

struct RuntimeStore {
    outbox: Arc<TelemetryOutbox>,
    pipeline_tx: Sender<PipelineMsg>,
    state: RuntimeStoreState,
}

#[derive(Default)]
struct RuntimeStoreState {
    journals: HashMap<CallId, (FinalJournalBoundary, FinalJournalState)>,
}

impl RuntimeStore {
    fn new(outbox: Arc<TelemetryOutbox>, pipeline_tx: Sender<PipelineMsg>) -> Self {
        Self {
            outbox,
            pipeline_tx,
            state: RuntimeStoreState::default(),
        }
    }

    fn terminal(&self, telemetry: &TerminalTelemetryDto) -> Result<(), ErrorCode> {
        self.outbox.append(telemetry.clone())?;
        let _ = self.pipeline_tx.send(PipelineMsg::ImportPostprocessOutbox);
        Ok(())
    }
}

impl EncryptedPostprocessStore for RuntimeStore {
    fn lookup_exact(&mut self, _key: RequestKey) -> Result<ExactLookup, ErrorCode> {
        // Reusable encrypted cache is not armed in this wiring slice. A miss is safe; production provider
        // execution is independently denied until the real encrypted store/keychain factory is installed.
        Ok(ExactLookup::Miss)
    }

    fn evict_corrupt(&mut self, _key: RequestKey) -> Result<(), ErrorCode> {
        Ok(())
    }

    fn prepare_final(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode> {
        self.state.journals.insert(
            boundary.call_id.clone(),
            (boundary.clone(), FinalJournalState::Prepared),
        );
        Ok(())
    }

    fn mark_final_dispatched(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode> {
        if let Some((_, state)) = self.state.journals.get_mut(&boundary.call_id) {
            *state = FinalJournalState::Dispatched;
        }
        Ok(())
    }

    fn commit_validated(&mut self, commit: StoreCommit<'_>) -> Result<(), ErrorCode> {
        self.terminal(commit.telemetry)?;
        if let Some(boundary) = commit.final_boundary
            && let Some((_, state)) = self.state.journals.get_mut(&boundary.call_id)
        {
            *state = FinalJournalState::ResultCached;
        }
        Ok(())
    }

    fn abandon_final(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode> {
        if let Some((_, state)) = self.state.journals.get_mut(&boundary.call_id) {
            *state = FinalJournalState::Abandoned;
        }
        Ok(())
    }

    fn mark_final_applied(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode> {
        if let Some((_, state)) = self.state.journals.get_mut(&boundary.call_id) {
            *state = FinalJournalState::Applied;
        }
        Ok(())
    }

    fn mark_final_checkpointed(
        &mut self,
        boundary: &FinalJournalBoundary,
    ) -> Result<(), ErrorCode> {
        if let Some((_, state)) = self.state.journals.get_mut(&boundary.call_id) {
            *state = FinalJournalState::Checkpointed;
        }
        Ok(())
    }

    fn recover_final(
        &mut self,
        recording_id: &str,
    ) -> Result<Option<FinalRecoveryRecord>, ErrorCode> {
        Ok(self
            .state
            .journals
            .values()
            .filter(|(boundary, _)| boundary.recording_id == recording_id)
            .max_by_key(|(boundary, _)| boundary.call_id.as_str())
            .map(|(boundary, state)| FinalRecoveryRecord {
                call_id: boundary.call_id.clone(),
                state: *state,
            }))
    }

    fn record_terminal(&mut self, telemetry: &TerminalTelemetryDto) -> Result<(), ErrorCode> {
        self.terminal(telemetry)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutboxDocument {
    schema: u32,
    entries: Vec<TerminalTelemetryDto>,
}

struct TelemetryOutbox {
    path: PathBuf,
    entries: Mutex<Vec<TerminalTelemetryDto>>,
}

impl TelemetryOutbox {
    fn open(path: PathBuf) -> Result<Self> {
        let entries = match read_private(&path, "hosted telemetry outbox", MAX_OUTBOX_BYTES)? {
            Some(bytes) => {
                let document: OutboxDocument =
                    serde_json::from_slice(&bytes).context("parsing hosted telemetry outbox")?;
                if document.schema != OUTBOX_SCHEMA {
                    bail!("unsupported hosted telemetry outbox schema");
                }
                document.entries
            }
            None => Vec::new(),
        };
        Ok(Self {
            path,
            entries: Mutex::new(entries),
        })
    }

    fn append(&self, telemetry: TerminalTelemetryDto) -> Result<(), ErrorCode> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.call_id == telemetry.call_id)
        {
            *existing = telemetry;
        } else {
            entries.push(telemetry);
        }
        self.persist(&entries).map_err(|_| ErrorCode::Cache)
    }

    fn pending(&self) -> Vec<TerminalTelemetryDto> {
        self.entries.lock().unwrap().clone()
    }

    fn acknowledge(&self, call_ids: &HashSet<CallId>) -> Result<()> {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|entry| !call_ids.contains(&entry.call_id));
        self.persist(&entries)
    }

    fn persist(&self, entries: &[TerminalTelemetryDto]) -> Result<()> {
        let bytes = serde_json::to_vec(&OutboxDocument {
            schema: OUTBOX_SCHEMA,
            entries: entries.to_vec(),
        })
        .context("serializing hosted telemetry outbox")?;
        if bytes.len() > MAX_OUTBOX_BYTES {
            bail!("hosted telemetry outbox reached its bounded capacity");
        }
        atomic_write_private(&self.path, &bytes, "hosted telemetry outbox")
    }
}

fn default_outbox_path() -> Result<PathBuf> {
    Ok(corti_queue::data_dir()?.join("postprocess-outbox.json"))
}

fn import_outbox(queue: &Queue, outbox: &TelemetryOutbox) -> Result<usize> {
    let mut imported = HashSet::new();
    for telemetry in outbox.pending() {
        // Live calls can settle before LiveNoteCreated/enqueue publishes the recording row. Keep those
        // entries pending; every later pipeline wake retries idempotently.
        if queue.get(&telemetry.recording_id)?.is_none() {
            continue;
        }
        let record = telemetry_to_record(&telemetry)?;
        queue.upsert_postprocess_call(&record)?;
        if telemetry.lane == Lane::Final {
            let projection = match telemetry.outcome {
                TerminalOutcomeDto::Completed => Some(corti_queue::PostprocessState::Complete),
                TerminalOutcomeDto::Failed
                | TerminalOutcomeDto::Canceled
                | TerminalOutcomeDto::Superseded
                | TerminalOutcomeDto::Timeout => Some(corti_queue::PostprocessState::Fallback),
            };
            queue.set_postprocess_state(&telemetry.recording_id, projection)?;
        }
        imported.insert(telemetry.call_id);
    }
    let count = imported.len();
    if !imported.is_empty() {
        outbox.acknowledge(&imported)?;
    }
    Ok(count)
}

fn telemetry_to_record(telemetry: &TerminalTelemetryDto) -> Result<PostprocessCallRecord> {
    let completed = utc_from_millis(telemetry.completed_at_unix_ms)?;
    let queued = utc_from_millis(telemetry.queued_at_unix_ms)?;
    let dispatched = telemetry
        .dispatched_at_unix_ms
        .map(utc_from_millis)
        .transpose()?;
    let cost = match telemetry.cost.billing_basis() {
        BillingBasis::MeteredEstimate => PostprocessCost::metered_estimate(
            telemetry
                .cost
                .cost_micros()
                .context("metered hosted cost missing amount")?,
            telemetry
                .cost
                .currency()
                .cloned()
                .context("metered hosted cost missing currency")?,
            telemetry
                .cost
                .pricing_catalog_version()
                .context("metered hosted cost missing catalog")?,
            telemetry
                .cost
                .tariff_id()
                .context("metered hosted cost missing tariff")?,
            utc_from_millis(
                telemetry
                    .cost
                    .tariff_effective_at_unix_ms()
                    .context("metered hosted cost missing effective time")?,
            )?,
        ),
        BillingBasis::IncludedSubscription => PostprocessCost::included_subscription(),
        BillingBasis::NoProviderRequest => PostprocessCost::no_provider_request(),
        BillingBasis::Unknown => PostprocessCost::unknown(),
    };
    let outcome = match telemetry.outcome {
        TerminalOutcomeDto::Completed => PostprocessOutcome::Completed,
        TerminalOutcomeDto::Failed => {
            if telemetry.error == Some(ErrorCode::AmbiguousDispatch) {
                PostprocessOutcome::Ambiguous
            } else {
                PostprocessOutcome::Failed
            }
        }
        TerminalOutcomeDto::Canceled => PostprocessOutcome::Canceled,
        TerminalOutcomeDto::Superseded => PostprocessOutcome::Superseded,
        TerminalOutcomeDto::Timeout => PostprocessOutcome::Timeout,
    };
    let cache_source = match telemetry.cache {
        CacheObservation::Local => PostprocessCacheSource::Local,
        CacheObservation::ProviderRead
        | CacheObservation::ProviderWrite
        | CacheObservation::ProviderImplicit => PostprocessCacheSource::Provider,
        CacheObservation::None if telemetry.provider_request_sent && telemetry.error.is_none() => {
            PostprocessCacheSource::Network
        }
        CacheObservation::None => PostprocessCacheSource::None,
    };
    Ok(PostprocessCallRecord {
        call_id: telemetry.call_id.clone(),
        recording_id: telemetry.recording_id.clone(),
        request_group_id: telemetry.request_group_id.clone(),
        target_id: telemetry.target_id.clone(),
        lane: telemetry.lane,
        attempt_no: telemetry.attempt_no,
        provider_id: telemetry.provider.clone(),
        transport_id: telemetry.transport.clone(),
        support_tier: telemetry.support_tier,
        model_id: telemetry.model.clone(),
        adapter_version: telemetry.adapter_version,
        prompt_version: telemetry.prompt_version,
        output_schema_version: telemetry.output_schema_version,
        session_generation: telemetry.fence.session_generation,
        transcript_revision: telemetry.fence.transcript_revision,
        control_revision: telemetry.fence.control_revision,
        steering_revision: telemetry.fence.steering_revision,
        bank_revision: telemetry.fence.bank_revision,
        question_revision: telemetry.fence.question_revision,
        outcome,
        error_code: telemetry.error,
        cache_source,
        provider_request_sent: telemetry.provider_request_sent,
        usage: telemetry.usage,
        cost,
        queued_at: queued,
        dispatched_at: dispatched,
        completed_at: Some(completed),
        latency: telemetry.latency,
        created_at: completed,
    })
}

fn utc_from_millis(value: i64) -> Result<DateTime<Utc>> {
    DateTime::from_timestamp_millis(value).context("invalid hosted history timestamp")
}

struct WorkerCompletion {
    ticket: DispatchTicket,
    result: Result<ProviderTerminal, corti_postprocess::PostprocessError>,
    cache: CacheObservation,
}

#[cfg_attr(not(feature = "local"), allow(dead_code))]
enum ServiceCommand {
    BeginSession {
        recording_id: String,
        reply: Sender<Result<(), ErrorCode>>,
    },
    EndSession {
        recording_id: String,
    },
    Patch {
        request: HostedPatchRequest,
        reply: Sender<Result<HostedMutationResult, ErrorCode>>,
    },
    UpdateSteering {
        request: SteeringUpdateRequest,
        reply: Sender<Result<HostedMutationResult, ErrorCode>>,
    },
    ReplaceWordBank {
        request: WordBankUpdateRequest,
        reply: Sender<Result<HostedMutationResult, ErrorCode>>,
    },
    UpdateProviderScope {
        request: ProviderScopeUpdateRequest,
        reply: Sender<Result<HostedMutationResult, ErrorCode>>,
    },
    RefreshProvider {
        provider: ProviderId,
        transport: TransportId,
        reply: Sender<Result<ProviderStateDto, ErrorCode>>,
    },
    Finalize {
        recording_id: String,
        transcript: DiarizedTranscript,
        live_session: bool,
        reply: Sender<SettledFinalTranscript>,
    },
    MarkFinalApplied {
        call_ids: Vec<CallId>,
        reply: Sender<Result<(), ErrorCode>>,
    },
    AbandonFinal {
        call_ids: Vec<CallId>,
    },
    MarkFinalCheckpointed {
        call_ids: Vec<CallId>,
    },
    SubmitAdHoc {
        question: String,
        reply: Sender<Result<CallId, ErrorCode>>,
    },
    CancelQuestion {
        call_id: CallId,
        reply: Sender<bool>,
    },
    SetPinnedTemplate {
        template: String,
        reply: Sender<Result<(), ErrorCode>>,
    },
    AssistantSnapshot {
        reply: Sender<AssistantSnapshotDto>,
    },
}

struct PendingFinal {
    live_session: bool,
    deadline_micros: u64,
    original: DiarizedTranscript,
    row_ids: Vec<RowId>,
    call_ids: Vec<CallId>,
    remaining: HashSet<CallId>,
    rewritten: HashMap<RowId, String>,
    reply: Sender<SettledFinalTranscript>,
    metadata: AppliedMetadata,
    source_fingerprint: Option<ProvenanceFingerprint>,
}

struct AppliedMetadata {
    provider: ProviderId,
    transport: TransportId,
    support_tier: SupportTier,
    model: ModelId,
    adapter_version: u32,
    word_bank_revision: u64,
    word_bank_fingerprint: ProvenanceFingerprint,
    word_bank_count: u32,
    steering_fingerprint: ProvenanceFingerprint,
}

struct Service {
    coordinator: PostprocessCoordinator,
    clock: Arc<dyn CoordinatorClock>,
    preferences: Arc<Mutex<HostedPreferences>>,
    word_bank: WordBankDocument,
    digest_key: Arc<DigestKey>,
    live_view: LiveTranscriptStore,
    ingress_incomplete: Arc<AtomicBool>,
    current_recording: Option<String>,
    ledger: Vec<TranscriptRow>,
    ledger_bytes: usize,
    session_steering: Option<String>,
    state_revision: u64,
    snapshot: Arc<Mutex<HostedSettingsDto>>,
    notifier: EventNotifier,
    executor: Arc<dyn TicketExecutor>,
    vertex_resolver: Arc<dyn VertexResolver>,
    worker_tx: Sender<WorkerCompletion>,
    worker_rx: Receiver<WorkerCompletion>,
    vertex_tx: Sender<(VertexResolutionAttempt, VertexResolutionOutcome)>,
    vertex_rx: Receiver<(VertexResolutionAttempt, VertexResolutionOutcome)>,
    provider_event_rx: Receiver<corti_postprocess::ProviderEvent>,
    event_sink: Arc<crate::postprocess::BoundedProviderEventSink>,
    next_id: u64,
    pending_finals: HashMap<RequestGroupId, PendingFinal>,
    final_by_call: HashMap<CallId, RequestGroupId>,
    call_cache: HashMap<CallId, CacheObservation>,
    observed_pinned_revision: u64,
    pinned_exchange: Option<AssistantExchangeDto>,
    persist_to_disk: bool,
}

impl Service {
    fn run(mut self, command_rx: Receiver<ServiceCommand>, ingress_rx: Receiver<HotPathCommand>) {
        loop {
            match command_rx.recv_timeout(SERVICE_TICK) {
                Ok(command) => self.handle_command(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.cancel_pending_finals(ErrorCode::Canceled);
                    return;
                }
            }
            while let Ok(command) = command_rx.try_recv() {
                self.handle_command(command);
            }
            self.drain_ingress(&ingress_rx);
            self.drain_provider_events();
            self.drain_workers();
            self.drain_vertex();
            self.coordinator.tick();
            self.expire_pending_finals();
            self.sync_pinned_revision();
            if let Some(attempt) = self.coordinator.drive_vertex() {
                self.spawn_vertex_resolution(attempt);
            }
            self.drive_dispatch();
            self.publish_events(false);
        }
    }

    fn handle_command(&mut self, command: ServiceCommand) {
        match command {
            ServiceCommand::BeginSession {
                recording_id,
                reply,
            } => {
                let result = self.begin_session(recording_id);
                let _ = reply.send(result);
            }
            ServiceCommand::EndSession { recording_id } => {
                if self.current_recording.as_deref() == Some(&recording_id) {
                    self.cancel_pending_finals(ErrorCode::Canceled);
                    let _ = self.coordinator.begin_session();
                    self.current_recording = None;
                    self.ledger.clear();
                    self.ledger_bytes = 0;
                    self.session_steering = None;
                    self.pinned_exchange = None;
                    self.ingress_incomplete.store(false, Ordering::Release);
                    self.bump_state();
                }
            }
            ServiceCommand::Patch { request, reply } => {
                let result = self.patch(request);
                let _ = reply.send(result);
            }
            ServiceCommand::UpdateSteering { request, reply } => {
                let result = self.update_steering(request);
                let _ = reply.send(result);
            }
            ServiceCommand::ReplaceWordBank { request, reply } => {
                let result = self.replace_word_bank(request);
                let _ = reply.send(result);
            }
            ServiceCommand::UpdateProviderScope { request, reply } => {
                let result = self.update_provider_scope(request);
                let _ = reply.send(result);
            }
            ServiceCommand::RefreshProvider {
                provider,
                transport,
                reply,
            } => {
                let result = self.refresh_provider(&provider, &transport);
                let _ = reply.send(result);
            }
            ServiceCommand::Finalize {
                recording_id,
                transcript,
                live_session,
                reply,
            } => self.start_final(recording_id, transcript, live_session, reply),
            ServiceCommand::MarkFinalApplied { call_ids, reply } => {
                let result = self
                    .coordinator
                    .mark_final_group_applied(&call_ids)
                    .map_err(coordinator_error_code);
                let _ = reply.send(result);
            }
            ServiceCommand::AbandonFinal { call_ids } => {
                for call_id in call_ids {
                    self.coordinator
                        .cancel_call(&call_id, CancellationReason::Superseded);
                }
            }
            ServiceCommand::MarkFinalCheckpointed { call_ids } => {
                for call_id in call_ids {
                    let _ = self.coordinator.mark_final_checkpointed(&call_id);
                }
            }
            ServiceCommand::SubmitAdHoc { question, reply } => {
                let result = self.submit_ad_hoc(question);
                let _ = reply.send(result);
            }
            ServiceCommand::CancelQuestion { call_id, reply } => {
                let canceled = self
                    .coordinator
                    .cancel_call(&call_id, CancellationReason::Explicit);
                let _ = reply.send(canceled);
            }
            ServiceCommand::SetPinnedTemplate { template, reply } => {
                let result = self.set_pinned_template(template);
                let _ = reply.send(result);
            }
            ServiceCommand::AssistantSnapshot { reply } => {
                let _ = reply.send(self.assistant_snapshot());
            }
        }
        self.drive_dispatch();
        self.publish_events(true);
    }

    fn begin_session(&mut self, recording_id: String) -> Result<(), ErrorCode> {
        validate_recording_id(&recording_id)?;
        self.cancel_pending_finals(ErrorCode::Superseded);
        self.coordinator
            .begin_session()
            .map_err(control_error_code)?;
        self.current_recording = Some(recording_id);
        self.ledger.clear();
        self.ledger_bytes = 0;
        self.session_steering = None;
        self.pinned_exchange = None;
        self.ingress_incomplete.store(false, Ordering::Release);
        self.bump_state();
        Ok(())
    }

    fn patch(&mut self, request: HostedPatchRequest) -> Result<HostedMutationResult, ErrorCode> {
        if request.observed_state_revision != self.state_revision {
            return Ok(HostedMutationResult::Conflict {
                settings: self.current_settings(),
            });
        }
        if let HostedPatchInput::SetPinnedAuto {
            enabled: true,
            acknowledged,
        } = &request.patch
        {
            if !acknowledged {
                return Err(ErrorCode::PolicyBlocked);
            }
            self.revise_preferences(|values| {
                values.pinned_auto_acknowledgement_version = Some(PINNED_AUTO_DISCLOSURE_VERSION)
            })?;
        }
        if let HostedPatchInput::SetDisplayPreferences {
            show_history_diagnostics,
            show_live_metrics_by_default,
        } = request.patch
        {
            let before = self.preferences.lock().unwrap().clone();
            self.revise_preferences(|values| {
                values.show_history_diagnostics = show_history_diagnostics;
                values.show_live_metrics_by_default = show_live_metrics_by_default;
            })?;
            let changed = *self.preferences.lock().unwrap() != before;
            if changed {
                self.bump_state();
                self.refresh_snapshot();
                return Ok(HostedMutationResult::Applied {
                    settings: self.current_settings(),
                });
            }
            return Ok(HostedMutationResult::Unchanged {
                settings: self.current_settings(),
            });
        }
        let patch = match request.patch {
            HostedPatchInput::SetEgressAcknowledged { acknowledged } => {
                ControlPatch::SetEgressAcknowledged(acknowledged)
            }
            HostedPatchInput::SetMaster { enabled } => ControlPatch::SetMaster(enabled),
            HostedPatchInput::SetLaneEnabled { lane, enabled } => ControlPatch::SetLaneEnabled {
                lane: lane.into(),
                enabled,
            },
            HostedPatchInput::SetLaneSelection { lane, selection } => {
                let family = LaneFamily::from(lane);
                let selection = LaneSelectionDto::try_from(selection)?;
                self.validate_settings_selection(family, &selection)?;
                ControlPatch::SetLaneSelection {
                    lane: family,
                    selection,
                }
            }
            HostedPatchInput::SetPinnedAuto { enabled, .. } => ControlPatch::SetPinnedAuto(enabled),
            HostedPatchInput::SetCodexExperimentalApproved { approved } => {
                ControlPatch::SetCodexExperimentalApproved(approved)
            }
            HostedPatchInput::SetDisplayPreferences { .. } => unreachable!(),
        };
        let outcome = self
            .coordinator
            .apply_patch(patch)
            .map_err(control_error_code)?;
        if !matches!(outcome, PatchOutcome::Unchanged(_)) {
            self.bump_state();
        }
        self.refresh_snapshot();
        Ok(match outcome {
            PatchOutcome::Applied(_) => HostedMutationResult::Applied {
                settings: self.current_settings(),
            },
            PatchOutcome::Unchanged(_) => HostedMutationResult::Unchanged {
                settings: self.current_settings(),
            },
            PatchOutcome::DisabledForSession { error, .. } => {
                HostedMutationResult::DisabledForSession {
                    settings: self.current_settings(),
                    code: error,
                }
            }
        })
    }

    fn update_steering(
        &mut self,
        request: SteeringUpdateRequest,
    ) -> Result<HostedMutationResult, ErrorCode> {
        if request.observed_state_revision != self.state_revision {
            return Ok(HostedMutationResult::Conflict {
                settings: self.current_settings(),
            });
        }
        if request.text.len() > 256 * 1024 || request.text.chars().any(char::is_control) {
            return Err(ErrorCode::PolicyBlocked);
        }
        if request.persist_as_default {
            let text = request.text;
            self.revise_preferences(|values| values.default_steering = text)?;
            self.session_steering = None;
            self.coordinator
                .apply_patch(ControlPatch::SteeringChanged)
                .map_err(control_error_code)?;
        } else {
            self.session_steering = Some(request.text);
            self.coordinator
                .apply_patch(ControlPatch::SessionSteeringChanged)
                .map_err(control_error_code)?;
        }
        self.bump_state();
        self.refresh_snapshot();
        Ok(HostedMutationResult::Applied {
            settings: self.current_settings(),
        })
    }

    fn replace_word_bank(
        &mut self,
        request: WordBankUpdateRequest,
    ) -> Result<HostedMutationResult, ErrorCode> {
        if request.observed_state_revision != self.state_revision {
            return Ok(HostedMutationResult::Conflict {
                settings: self.current_settings(),
            });
        }
        let next = self
            .word_bank
            .replace(request.entries)
            .map_err(|_| ErrorCode::PolicyBlocked)?;
        if next == self.word_bank {
            return Ok(HostedMutationResult::Unchanged {
                settings: self.current_settings(),
            });
        }
        if self.persist_to_disk {
            crate::word_bank::save(&next).map_err(|_| ErrorCode::Cache)?;
        }
        self.word_bank = next;
        self.coordinator
            .apply_patch(ControlPatch::BankChanged)
            .map_err(control_error_code)?;
        self.bump_state();
        self.refresh_snapshot();
        Ok(HostedMutationResult::Applied {
            settings: self.current_settings(),
        })
    }

    fn update_provider_scope(
        &mut self,
        request: ProviderScopeUpdateRequest,
    ) -> Result<HostedMutationResult, ErrorCode> {
        if request.observed_state_revision != self.state_revision {
            return Ok(HostedMutationResult::Conflict {
                settings: self.current_settings(),
            });
        }
        let provider = ProviderId::new(request.provider).map_err(|_| ErrorCode::PolicyBlocked)?;
        let transport =
            TransportId::new(request.transport).map_err(|_| ErrorCode::PolicyBlocked)?;
        if known_descriptor(&provider, &transport).is_none() {
            return Err(ErrorCode::PolicyBlocked);
        }
        let alias = bounded_optional(request.alias)?;
        let project = bounded_optional(request.project)?;
        let region = bounded_optional(request.region)?;
        let quota_project = bounded_optional(request.quota_project)?;
        let configured =
            alias.is_some() || project.is_some() || region.is_some() || quota_project.is_some();
        let existing_id = {
            let preferences = self.preferences.lock().unwrap();
            let values = preferences.values();
            match (provider.as_str(), transport.as_str()) {
                ("google", "vertex_api") => values.providers.vertex.connection_scope_id.clone(),
                ("openai", "openai_api") => {
                    values.providers.openai.scope.connection_scope_id.clone()
                }
                ("anthropic", "anthropic_api") => {
                    values.providers.anthropic.scope.connection_scope_id.clone()
                }
                _ => return Err(ErrorCode::PolicyBlocked),
            }
        };
        let generated_id = if configured && existing_id.is_none() {
            let id = self.next_id;
            self.next_id = self.next_id.checked_add(1).ok_or(ErrorCode::Internal)?;
            Some(
                ConnectionScopeId::new(format!(
                    "scope-{}-{id}",
                    self.coordinator.control_snapshot().process_epoch.0
                ))
                .map_err(|_| ErrorCode::Internal)?,
            )
        } else if configured {
            existing_id
        } else {
            None
        };
        self.revise_preferences(|values| {
            let scope = match (provider.as_str(), transport.as_str()) {
                ("google", "vertex_api") => &mut values.providers.vertex,
                ("openai", "openai_api") => &mut values.providers.openai.scope,
                ("anthropic", "anthropic_api") => &mut values.providers.anthropic.scope,
                _ => unreachable!("validated provider transport"),
            };
            scope.connection_scope_id = generated_id;
            scope.alias = alias;
            scope.project = project;
            scope.region = region;
            scope.quota_project = quota_project;
        })?;
        self.coordinator
            .apply_patch(ControlPatch::ProviderScopeChanged)
            .map_err(control_error_code)?;
        self.coordinator
            .invalidate_provider_scope(&provider, &transport);
        self.bump_state();
        self.refresh_snapshot();
        Ok(HostedMutationResult::Applied {
            settings: self.current_settings(),
        })
    }

    fn validate_settings_selection(
        &self,
        family: LaneFamily,
        selection: &LaneSelectionDto,
    ) -> Result<(), ErrorCode> {
        let fields = (
            selection.provider.as_ref(),
            selection.transport.as_ref(),
            selection.model.as_ref(),
        );
        let (Some(provider), Some(transport), Some(model)) = fields else {
            return if selection.provider.is_none()
                && selection.transport.is_none()
                && selection.model.is_none()
            {
                Ok(())
            } else {
                Err(ErrorCode::PolicyBlocked)
            };
        };
        let scope = self.scope_for(provider, transport)?;
        let state = self
            .coordinator
            .provider_states()
            .find(|state| {
                &state.descriptor.provider == provider && &state.descriptor.transport == transport
            })
            .ok_or(ErrorCode::ModelUnavailable)?;
        let candidate = state
            .models
            .iter()
            .find(|candidate| {
                &candidate.exact_model_id == model
                    && candidate.region.as_deref() == scope.region.as_deref()
            })
            .ok_or(ErrorCode::ModelUnavailable)?;
        if !candidate.account_scoped_available
            || candidate.deprecated
            || !candidate.capabilities.text_input
            || !candidate.capabilities.text_output
            || !candidate.capabilities.structured_output
            || (family == LaneFamily::Live && !candidate.benchmarked_for_live)
        {
            return Err(ErrorCode::PolicyBlocked);
        }
        match selection.cache_policy.provider {
            ProviderCacheMode::ExplicitStablePrefix
                if !candidate.capabilities.explicit_prefix_cache =>
            {
                Err(ErrorCode::PolicyBlocked)
            }
            ProviderCacheMode::UnavoidableImplicit
                if !candidate.capabilities.implicit_cache_may_apply =>
            {
                Err(ErrorCode::PolicyBlocked)
            }
            ProviderCacheMode::Off if candidate.capabilities.implicit_cache_may_apply => {
                Err(ErrorCode::PolicyBlocked)
            }
            _ => Ok(()),
        }
    }

    fn refresh_provider(
        &mut self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> Result<ProviderStateDto, ErrorCode> {
        let scope = self.scope_for(provider, transport)?;
        let state = self
            .coordinator
            .refresh_provider(provider, transport, &scope)?;
        self.bump_state();
        self.refresh_snapshot();
        Ok(state)
    }

    fn set_pinned_template(&mut self, template: String) -> Result<(), ErrorCode> {
        if template.len() > crate::postprocess::MAX_QUESTION_TEXT_BYTES
            || template.chars().any(char::is_control)
        {
            return Err(ErrorCode::PolicyBlocked);
        }
        self.coordinator
            .edit_pinned_template(template.clone())
            .map_err(submit_error_code)?;
        self.pinned_exchange = None;
        self.revise_preferences(|values| values.pinned_question_template = template)?;
        self.bump_state();
        self.refresh_snapshot();
        Ok(())
    }

    fn revise_preferences(
        &mut self,
        update: impl FnOnce(&mut crate::postprocess_config::HostedPreferenceValues),
    ) -> Result<(), ErrorCode> {
        let current = self.preferences.lock().unwrap().clone();
        let next = current
            .revise(update)
            .map_err(|_| ErrorCode::PolicyBlocked)?;
        if self.persist_to_disk {
            next.save().map_err(|_| ErrorCode::Cache)?;
        }
        *self.preferences.lock().unwrap() = next;
        Ok(())
    }

    fn drain_ingress(&mut self, ingress_rx: &Receiver<HotPathCommand>) {
        loop {
            match ingress_rx.try_recv() {
                Ok(HotPathCommand::FinalizedRows { recording_id, rows }) => {
                    self.observe_rows(&recording_id, rows)
                }
                Ok(HotPathCommand::LiveRequest {
                    submission,
                    watermark,
                }) => {
                    let _ = self.coordinator.submit_live(*submission, watermark);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    fn observe_rows(&mut self, recording_id: &str, rows: Vec<TranscriptRow>) {
        if self.current_recording.as_deref() != Some(recording_id) || rows.is_empty() {
            return;
        }
        let added = rows.iter().fold(0usize, |total, row| {
            total
                .saturating_add(row.text.len())
                .saturating_add(row.speaker.len())
                .saturating_add(64)
        });
        if self.ledger_bytes.saturating_add(added) > MAX_SESSION_LEDGER_BYTES {
            self.ingress_incomplete.store(true, Ordering::Release);
            return;
        }
        let watermark = match self.coordinator.observe_finalized_rows(&rows) {
            Ok(watermark) => watermark,
            Err(_) => {
                self.ingress_incomplete.store(true, Ordering::Release);
                return;
            }
        };
        let old_len = self.ledger.len();
        self.ledger_bytes = self.ledger_bytes.saturating_add(added);
        self.ledger.extend(rows);
        if let Some(submission) = self.build_live_submission(recording_id, old_len, watermark) {
            let _ = self.coordinator.submit_live(submission, watermark);
        }
        if let Some(submission) = self.build_pinned_submission(recording_id, watermark) {
            self.submit_pinned_candidate(submission, watermark);
        }
    }

    fn build_live_submission(
        &mut self,
        recording_id: &str,
        old_len: usize,
        watermark: TranscriptWatermark,
    ) -> Option<RequestSubmission> {
        if !lane_enabled(self.coordinator.control_snapshot(), LaneFamily::Live) {
            return None;
        }
        let mut bytes = 0usize;
        let mut targets = Vec::new();
        for row in &self.ledger[old_len..] {
            let next = bytes.saturating_add(row.text.len());
            if targets.len() >= MAX_LIVE_TARGET_ROWS || next > MAX_LIVE_TARGET_BYTES {
                break;
            }
            bytes = next;
            targets.push(row.clone());
        }
        if targets.is_empty() {
            return None;
        }
        let context_start = old_len.saturating_sub(MAX_CONTEXT_ROWS);
        let context = self.ledger[context_start..old_len].to_vec();
        self.build_submission(
            recording_id,
            Lane::Live,
            targets,
            context,
            None,
            watermark,
            None,
            self.clock
                .monotonic_micros()
                .saturating_add(crate::postprocess::LIVE_TERMINAL_DEADLINE_MICROS),
        )
        .ok()
    }

    fn sync_pinned_revision(&mut self) {
        let revision = self.coordinator.control_snapshot().pinned_question_revision;
        if revision == self.observed_pinned_revision {
            return;
        }
        self.observed_pinned_revision = revision;
        let Some(recording_id) = self.current_recording.clone() else {
            return;
        };
        let watermark = self.coordinator.watermark();
        if let Some(submission) = self.build_pinned_submission(&recording_id, watermark) {
            self.submit_pinned_candidate(submission, watermark);
        }
    }

    fn submit_pinned_candidate(
        &mut self,
        submission: RequestSubmission,
        watermark: TranscriptWatermark,
    ) {
        let call_id = submission.request.call_id.clone();
        if self
            .coordinator
            .submit_pinned_snapshot(submission, watermark)
            .is_ok()
        {
            let question = self
                .preferences
                .lock()
                .unwrap()
                .values()
                .pinned_question_template
                .clone();
            self.pinned_exchange = Some(AssistantExchangeDto {
                call_id,
                as_of_revision: watermark.transcript_revision,
                status: QuestionStatusDto::Queued,
                error: None,
                question,
                answer: None,
                cost_label: None,
            });
        }
    }

    fn build_pinned_submission(
        &mut self,
        recording_id: &str,
        watermark: TranscriptWatermark,
    ) -> Option<RequestSubmission> {
        let template = self
            .preferences
            .lock()
            .unwrap()
            .values()
            .pinned_question_template
            .clone();
        if template.trim().is_empty()
            || !self.coordinator.control_snapshot().pinned_auto_enabled
            || !lane_enabled(self.coordinator.control_snapshot(), LaneFamily::Question)
        {
            return None;
        }
        let context = bounded_question_context(&self.ledger);
        self.build_submission(
            recording_id,
            Lane::PinnedQuestion,
            Vec::new(),
            context,
            Some(&template),
            watermark,
            None,
            self.clock
                .monotonic_micros()
                .saturating_add(crate::postprocess::QUESTION_DEADLINE_MICROS),
        )
        .ok()
    }

    fn submit_ad_hoc(&mut self, question: String) -> Result<CallId, ErrorCode> {
        let recording_id = self
            .current_recording
            .clone()
            .ok_or(ErrorCode::PolicyBlocked)?;
        let watermark = self.coordinator.watermark();
        let context = bounded_question_context(&self.ledger);
        let submission = self.build_submission(
            &recording_id,
            Lane::AdHocQuestion,
            Vec::new(),
            context,
            Some(&question),
            watermark,
            None,
            self.clock
                .monotonic_micros()
                .saturating_add(crate::postprocess::QUESTION_DEADLINE_MICROS),
        )?;
        let call_id = submission.request.call_id.clone();
        self.coordinator
            .submit_ad_hoc(submission, watermark, question)
            .map_err(submit_error_code)?;
        Ok(call_id)
    }

    fn assistant_snapshot(&self) -> AssistantSnapshotDto {
        let exchanges = self
            .coordinator
            .question_summaries()
            .into_iter()
            .filter_map(|summary| {
                let content = self.coordinator.question_content(&summary.call_id)?;
                Some(AssistantExchangeDto {
                    call_id: summary.call_id,
                    as_of_revision: summary.as_of_revision,
                    status: summary.status,
                    error: summary.error,
                    question: content.question.to_owned(),
                    answer: content.answer.map(str::to_owned),
                    cost_label: summary.cost.as_ref().map(CostEstimate::render),
                })
            })
            .collect();
        AssistantSnapshotDto {
            pinned_run_count: self.coordinator.pinned_run_count(),
            pinned: self.pinned_exchange.clone(),
            exchanges,
        }
    }

    fn start_final(
        &mut self,
        recording_id: String,
        transcript: DiarizedTranscript,
        live_session: bool,
        reply: Sender<SettledFinalTranscript>,
    ) {
        if validate_recording_id(&recording_id).is_err() {
            let _ = reply.send(fallback_final(
                transcript,
                ErrorCode::PolicyBlocked,
                Vec::new(),
                None,
            ));
            return;
        }
        if !lane_enabled(self.coordinator.control_snapshot(), LaneFamily::Final) {
            let _ = reply.send(fallback_final(
                transcript,
                ErrorCode::PolicyBlocked,
                Vec::new(),
                None,
            ));
            return;
        }
        if live_session && self.ingress_incomplete.load(Ordering::Acquire) {
            let _ = reply.send(fallback_final(
                transcript,
                ErrorCode::Cache,
                Vec::new(),
                None,
            ));
            return;
        }
        if let Some(current) = self.current_recording.as_deref()
            && current != recording_id
        {
            // A serial-pipeline final must never cancel a newer capture's hosted session. Raw batch filing
            // remains safe and the active call keeps its live/questions state.
            let _ = reply.send(fallback_final(
                transcript,
                ErrorCode::Superseded,
                Vec::new(),
                None,
            ));
            return;
        }
        if self.current_recording.is_none() && self.begin_session(recording_id.clone()).is_err() {
            let _ = reply.send(fallback_final(
                transcript,
                ErrorCode::Internal,
                Vec::new(),
                None,
            ));
            return;
        }
        let rows = transcript_rows(&transcript);
        let watermark = if self.coordinator.watermark().transcript_revision == 0 {
            match self.coordinator.observe_finalized_rows(&rows) {
                Ok(value) => value,
                Err(_) => {
                    let _ = reply.send(fallback_final(
                        transcript,
                        ErrorCode::Internal,
                        Vec::new(),
                        None,
                    ));
                    return;
                }
            }
        } else {
            self.coordinator.watermark()
        };
        let chunks = match final_chunks(&rows) {
            Ok(chunks) => chunks,
            Err(code) => {
                let _ = reply.send(fallback_final(transcript, code, Vec::new(), None));
                return;
            }
        };
        let group_id = match self.next_group_id("final") {
            Ok(value) => value,
            Err(code) => {
                let _ = reply.send(fallback_final(transcript, code, Vec::new(), None));
                return;
            }
        };
        let deadline_seconds = self
            .preferences
            .lock()
            .unwrap()
            .values()
            .final_deadline_seconds;
        let deadline = self
            .clock
            .monotonic_micros()
            .saturating_add(u64::from(deadline_seconds).saturating_mul(1_000_000));
        let mut submissions = Vec::with_capacity(chunks.len());
        for (index, (targets, context)) in chunks.into_iter().enumerate() {
            let target_id = TargetId::new(format!("{}-chunk-{index}", group_id.as_str())).ok();
            match self.build_submission(
                &recording_id,
                Lane::Final,
                targets,
                context,
                None,
                watermark,
                Some((group_id.clone(), target_id)),
                deadline,
            ) {
                Ok(value) => submissions.push(value),
                Err(code) => {
                    let _ = reply.send(fallback_final(transcript, code, Vec::new(), None));
                    return;
                }
            }
        }
        let metadata = match self.applied_metadata(LaneFamily::Final) {
            Ok(value) => value,
            Err(code) => {
                let _ = reply.send(fallback_final(transcript, code, Vec::new(), None));
                return;
            }
        };
        let source_fingerprint = transcript_fingerprint(&self.digest_key, &transcript).ok();
        let call_ids: Vec<CallId> = submissions
            .iter()
            .map(|submission| submission.request.call_id.clone())
            .collect();
        for submission in submissions {
            if let Err(error) = self.coordinator.submit_final(submission, watermark) {
                for call_id in &call_ids {
                    self.coordinator
                        .cancel_call(call_id, CancellationReason::Superseded);
                }
                let _ = reply.send(fallback_final(
                    transcript,
                    submit_error_code(error),
                    call_ids,
                    source_fingerprint,
                ));
                return;
            }
        }
        for call_id in &call_ids {
            self.final_by_call.insert(call_id.clone(), group_id.clone());
        }
        self.pending_finals.insert(
            group_id,
            PendingFinal {
                live_session,
                deadline_micros: deadline,
                original: transcript,
                row_ids: rows.iter().map(|row| row.row_id.clone()).collect(),
                remaining: call_ids.iter().cloned().collect(),
                call_ids,
                rewritten: HashMap::new(),
                reply,
                metadata,
                source_fingerprint,
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn build_submission(
        &mut self,
        recording_id: &str,
        lane: Lane,
        targets: Vec<TranscriptRow>,
        context: Vec<TranscriptRow>,
        question: Option<&str>,
        watermark: TranscriptWatermark,
        identity: Option<(RequestGroupId, Option<TargetId>)>,
        deadline: u64,
    ) -> Result<RequestSubmission, ErrorCode> {
        let family = LaneFamily::of(lane);
        let lane_control = match family {
            LaneFamily::Live => self.coordinator.control_snapshot().live.clone(),
            LaneFamily::Final => self.coordinator.control_snapshot().final_lane.clone(),
            LaneFamily::Question => self.coordinator.control_snapshot().questions.clone(),
        };
        let provider = lane_control
            .selection
            .provider
            .clone()
            .ok_or(ErrorCode::PolicyBlocked)?;
        let transport = lane_control
            .selection
            .transport
            .clone()
            .ok_or(ErrorCode::PolicyBlocked)?;
        let model = lane_control
            .selection
            .model
            .clone()
            .ok_or(ErrorCode::PolicyBlocked)?;
        let descriptor = known_descriptor(&provider, &transport).ok_or(ErrorCode::PolicyBlocked)?;
        let scope = self.scope_for(&provider, &transport)?;
        let steering = self.effective_steering();
        let prompt = if lane.is_question() {
            CanonicalPrompt::question(
                &self.word_bank,
                &steering,
                &context,
                question.ok_or(ErrorCode::PolicyBlocked)?,
                false,
            )
        } else {
            CanonicalPrompt::rewrite(&self.word_bank, &steering, &context, &targets)
        };
        let (group_id, target_id) = match identity {
            Some(value) => value,
            None => (self.next_group_id("request")?, None),
        };
        let call_id = self.next_call_id()?;
        let fence = RequestFence {
            process_epoch: self.coordinator.control_snapshot().process_epoch,
            session_generation: watermark.session_generation,
            transcript_revision: watermark.transcript_revision,
            control_revision: self.coordinator.control_snapshot().control_revision,
            lane_revision: lane_control.revision,
            steering_revision: self.coordinator.control_snapshot().steering_revision,
            bank_revision: self.coordinator.control_snapshot().bank_revision,
            question_revision: None,
        };
        let request = HostedRequest {
            call_id,
            group_id,
            target_id,
            lane,
            fence,
            provider: provider.clone(),
            transport: transport.clone(),
            model: model.clone(),
            targets,
            context,
            prompt,
            deadline: MonotonicDeadline(deadline),
            cache_policy: lane_control.selection.cache_policy,
        };
        let adapter_version = adapter_version(&transport);
        let request_key = RequestKey::derive(
            &self.digest_key,
            &RequestKeyMaterial {
                provider: &provider,
                transport: &transport,
                support_tier: descriptor.support_tier,
                connection_scope_id: &scope.connection_scope_id,
                region: scope.region.as_deref(),
                exact_model_id: &model,
                adapter_version,
                prompt_template_version: PROMPT_TEMPLATE_VERSION,
                output_schema_version: OUTPUT_SCHEMA_VERSION,
                chunker_version: 1,
                lane,
                billing_basis: descriptor.billing_basis,
                cache_policy: request.cache_policy,
                word_bank_canonical_digest: self.word_bank.content_digest(),
                effective_steering: &steering,
                targets: &request.targets,
                context: &request.context,
                question,
            },
        );
        RequestSubmission::new(
            recording_id,
            request,
            request_key,
            scope,
            adapter_version,
            2 * 1024 * 1024,
            2 * 1024 * 1024,
            false,
        )
        .map_err(submit_error_code)
    }

    fn applied_metadata(&self, family: LaneFamily) -> Result<AppliedMetadata, ErrorCode> {
        let lane = match family {
            LaneFamily::Live => &self.coordinator.control_snapshot().live,
            LaneFamily::Final => &self.coordinator.control_snapshot().final_lane,
            LaneFamily::Question => &self.coordinator.control_snapshot().questions,
        };
        let provider = lane
            .selection
            .provider
            .clone()
            .ok_or(ErrorCode::PolicyBlocked)?;
        let transport = lane
            .selection
            .transport
            .clone()
            .ok_or(ErrorCode::PolicyBlocked)?;
        let model = lane
            .selection
            .model
            .clone()
            .ok_or(ErrorCode::PolicyBlocked)?;
        let descriptor = known_descriptor(&provider, &transport).ok_or(ErrorCode::PolicyBlocked)?;
        let bank_fingerprint = self
            .word_bank
            .external_fingerprint(&self.digest_key)
            .map_err(|_| ErrorCode::Internal)?;
        let steering = self.effective_steering();
        let steering_fingerprint = self
            .digest_key
            .fingerprint(FINGERPRINT_DOMAIN, steering.as_bytes());
        Ok(AppliedMetadata {
            provider,
            transport: transport.clone(),
            support_tier: descriptor.support_tier,
            model,
            adapter_version: adapter_version(&transport),
            word_bank_revision: self.word_bank.revision(),
            word_bank_fingerprint: ProvenanceFingerprint::new(bank_fingerprint.as_str())
                .map_err(|_| ErrorCode::Internal)?,
            word_bank_count: u32::try_from(self.word_bank.entries().len())
                .map_err(|_| ErrorCode::Internal)?,
            steering_fingerprint: ProvenanceFingerprint::new(steering_fingerprint.as_str())
                .map_err(|_| ErrorCode::Internal)?,
        })
    }

    fn effective_steering(&self) -> String {
        self.session_steering.clone().unwrap_or_else(|| {
            self.preferences
                .lock()
                .unwrap()
                .values()
                .default_steering
                .clone()
        })
    }

    fn scope_for(
        &self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> Result<ProviderScope, ErrorCode> {
        let preferences = self.preferences.lock().unwrap();
        let values = preferences.values();
        let scope = match (provider.as_str(), transport.as_str()) {
            ("google", "vertex_api") => &values.providers.vertex,
            ("openai", "openai_api") => &values.providers.openai.scope,
            ("anthropic", "anthropic_api") => &values.providers.anthropic.scope,
            _ => return Err(ErrorCode::PolicyBlocked),
        };
        Ok(ProviderScope {
            connection_scope_id: scope
                .connection_scope_id
                .clone()
                .ok_or(ErrorCode::PolicyBlocked)?,
            region: scope.region.clone(),
        })
    }

    fn next_call_id(&mut self) -> Result<CallId, ErrorCode> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(ErrorCode::Internal)?;
        CallId::new(format!(
            "hosted-{}-{id}",
            self.coordinator.control_snapshot().process_epoch.0
        ))
        .map_err(|_| ErrorCode::Internal)
    }

    fn next_group_id(&mut self, kind: &str) -> Result<RequestGroupId, ErrorCode> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(ErrorCode::Internal)?;
        RequestGroupId::new(format!(
            "{kind}-{}-{id}",
            self.coordinator.control_snapshot().process_epoch.0
        ))
        .map_err(|_| ErrorCode::Internal)
    }

    fn drive_dispatch(&mut self) {
        loop {
            match self.coordinator.dispatch_next() {
                DispatchOutcome::Ticket(ticket) => self.spawn_ticket(ticket),
                DispatchOutcome::CacheApply(apply) => {
                    self.call_cache
                        .insert(apply.call_id.clone(), CacheObservation::Local);
                    self.handle_apply(apply);
                }
                DispatchOutcome::Failed { call_id, code } => {
                    self.fail_final_call(&call_id, code);
                }
                DispatchOutcome::Empty
                | DispatchOutcome::Waiting
                | DispatchOutcome::Backpressured => break,
            }
        }
    }

    fn spawn_vertex_resolution(&mut self, attempt: VertexResolutionAttempt) {
        let resolver = self.vertex_resolver.clone();
        let completed = self.vertex_tx.clone();
        if std::thread::Builder::new()
            .name("corti-hosted-auth".into())
            .spawn(move || {
                let outcome = resolver.resolve(&attempt);
                let _ = completed.send((attempt, outcome));
            })
            .is_err()
        {
            let _ = self.coordinator.complete_vertex(
                attempt,
                VertexResolutionOutcome::Error {
                    code: ErrorCode::Internal,
                },
            );
        }
    }

    fn drain_vertex(&mut self) {
        while let Ok((attempt, outcome)) = self.vertex_rx.try_recv() {
            let _ = self.coordinator.complete_vertex(attempt, outcome);
        }
    }

    fn spawn_ticket(&mut self, ticket: DispatchTicket) {
        // Builder::spawn drops its closure on failure. Keep ticket ownership in a recoverable slot so a
        // thread-limit/resource error settles the final immediately instead of leaking an active call.
        let slot = Arc::new(Mutex::new(Some(ticket)));
        let worker_slot = slot.clone();
        let executor = self.executor.clone();
        let sink = self.event_sink.clone();
        let completed = self.worker_tx.clone();
        let spawn = std::thread::Builder::new()
            .name("corti-hosted-provider".into())
            .spawn(move || {
                let Some(ticket) = worker_slot.lock().unwrap().take() else {
                    return;
                };
                let result = executor.execute(&ticket, sink.as_ref());
                let cache = result
                    .as_ref()
                    .map_or(CacheObservation::None, |terminal| terminal.cache);
                let _ = completed.send(WorkerCompletion {
                    ticket,
                    result,
                    cache,
                });
            });
        if spawn.is_err()
            && let Some(ticket) = slot.lock().unwrap().take()
        {
            match self
                .coordinator
                .complete(ticket, Err(ErrorCode::Internal.into()))
            {
                CompletionOutcome::Apply(apply) => self.handle_apply(apply),
                CompletionOutcome::Discarded { call_id, code }
                | CompletionOutcome::Failed { call_id, code } => {
                    self.fail_final_call(&call_id, code)
                }
            }
        }
    }

    fn drain_provider_events(&mut self) {
        let _ = crate::postprocess::drain_provider_events(
            &self.provider_event_rx,
            &mut self.coordinator,
        );
    }

    fn drain_workers(&mut self) {
        while let Ok(completion) = self.worker_rx.try_recv() {
            let call_id = completion.ticket.request().call_id.clone();
            self.call_cache.insert(call_id.clone(), completion.cache);
            match self
                .coordinator
                .complete(completion.ticket, completion.result)
            {
                CompletionOutcome::Apply(apply) => self.handle_apply(apply),
                CompletionOutcome::Discarded { call_id, code }
                | CompletionOutcome::Failed { call_id, code } => {
                    self.fail_final_call(&call_id, code)
                }
            }
        }
    }

    fn handle_apply(&mut self, apply: crate::postprocess::ApplyReady) {
        if !self.coordinator.application_is_current(&apply) {
            self.fail_final_call(&apply.call_id, ErrorCode::Superseded);
            return;
        }
        match apply.lane {
            Lane::Live => {
                if let Some(recording_id) = self.current_recording.as_deref()
                    && let Some(rows) = apply.output.rewritten_rows()
                {
                    self.live_view.apply_hosted_rows(
                        recording_id,
                        rows,
                        apply.fence.transcript_revision,
                    );
                }
            }
            Lane::Final => {
                let Some(group_id) = self.final_by_call.get(&apply.call_id).cloned() else {
                    return;
                };
                let Some(group) = self.pending_finals.get_mut(&group_id) else {
                    return;
                };
                if let Some(rows) = apply.output.rewritten_rows() {
                    for row in rows {
                        group.rewritten.insert(row.row_id.clone(), row.text.clone());
                    }
                }
                group.remaining.remove(&apply.call_id);
                if group.remaining.is_empty() {
                    self.complete_final_group(&group_id);
                }
            }
            Lane::PinnedQuestion => {
                if let Some(exchange) = self.pinned_exchange.as_mut()
                    && exchange.call_id == apply.call_id
                {
                    exchange.status = QuestionStatusDto::Completed;
                    exchange.error = None;
                    exchange.answer = apply.output.answer().map(str::to_owned);
                }
            }
            Lane::AdHocQuestion => {}
        }
    }

    fn complete_final_group(&mut self, group_id: &RequestGroupId) {
        let Some(group) = self.pending_finals.remove(group_id) else {
            return;
        };
        for call_id in &group.call_ids {
            self.final_by_call.remove(call_id);
        }
        let mut transcript = group.original.clone();
        for (segment, row_id) in transcript.segments.iter_mut().zip(&group.row_ids) {
            if let Some(text) = group.rewritten.get(row_id) {
                segment.text.clone_from(text);
            }
        }
        let cache = combined_cache(group.call_ids.iter().map(|call_id| {
            self.call_cache
                .remove(call_id)
                .unwrap_or(CacheObservation::None)
        }));
        let applied = AppliedPostprocessProvenance::applied(
            AppliedPostprocessState::Final,
            AppliedPostprocessDetails {
                provider: group.metadata.provider.as_str().to_owned(),
                transport: group.metadata.transport.as_str().to_owned(),
                support_tier: group.metadata.support_tier,
                model: group.metadata.model.as_str().to_owned(),
                adapter_version: group.metadata.adapter_version,
                prompt_version: PROMPT_TEMPLATE_VERSION,
                output_schema_version: OUTPUT_SCHEMA_VERSION,
                word_bank: AppliedWordBankProvenance {
                    revision: group.metadata.word_bank_revision,
                    fingerprint: group.metadata.word_bank_fingerprint,
                    count: group.metadata.word_bank_count,
                },
                steering_fingerprint: group.metadata.steering_fingerprint,
                cache_source: cache,
                live_revision_summary: None,
                final_outcome: Some(FinalPostprocessOutcome::Applied),
            },
        )
        .unwrap_or_else(|_| AppliedPostprocessProvenance::none());
        if !group.live_session {
            self.current_recording = None;
            self.ledger.clear();
            self.ledger_bytes = 0;
        }
        let _ = group.reply.send(SettledFinalTranscript {
            transcript,
            applied_postprocess: applied,
            source_transcript_fingerprint: group.source_fingerprint,
            call_ids: group.call_ids,
            hosted_text_applied: true,
            fallback_code: None,
        });
    }

    fn fail_final_call(&mut self, call_id: &CallId, code: ErrorCode) {
        let Some(group_id) = self.final_by_call.get(call_id).cloned() else {
            return;
        };
        let Some(group) = self.pending_finals.remove(&group_id) else {
            return;
        };
        for peer in &group.call_ids {
            self.final_by_call.remove(peer);
            if peer != call_id {
                self.coordinator
                    .cancel_call(peer, CancellationReason::Superseded);
            }
            self.call_cache.remove(peer);
        }
        if !group.live_session {
            self.current_recording = None;
            self.ledger.clear();
            self.ledger_bytes = 0;
        }
        let _ = group.reply.send(fallback_final(
            group.original,
            code,
            group.call_ids,
            group.source_fingerprint,
        ));
    }

    fn expire_pending_finals(&mut self) {
        let now = self.clock.monotonic_micros();
        let calls: Vec<CallId> = self
            .pending_finals
            .values()
            .filter(|group| now >= group.deadline_micros)
            .filter_map(|group| group.call_ids.first().cloned())
            .collect();
        for call_id in calls {
            self.fail_final_call(&call_id, ErrorCode::Timeout);
        }
    }

    fn cancel_pending_finals(&mut self, code: ErrorCode) {
        let groups: Vec<RequestGroupId> = self.pending_finals.keys().cloned().collect();
        for group_id in groups {
            let call_id = self
                .pending_finals
                .get(&group_id)
                .and_then(|group| group.call_ids.first())
                .cloned();
            if let Some(call_id) = call_id {
                self.fail_final_call(&call_id, code);
            }
        }
    }

    fn publish_events(&mut self, mutation: bool) {
        let events = self.coordinator.take_events();
        // Queued auth/deadline failures settle inside `tick` and therefore do not return a
        // `DispatchOutcome::Failed`. Terminal events are the lossless bridge back to a waiting final group.
        for event in &events {
            let CoordinatorEventDto::Terminal(telemetry) = event else {
                continue;
            };
            if let Some(exchange) = self.pinned_exchange.as_mut()
                && exchange.call_id == telemetry.call_id
            {
                exchange.cost_label = Some(telemetry.cost.render());
                if telemetry.outcome != TerminalOutcomeDto::Completed {
                    exchange.status = if matches!(
                        telemetry.outcome,
                        TerminalOutcomeDto::Canceled | TerminalOutcomeDto::Superseded
                    ) {
                        QuestionStatusDto::Canceled
                    } else {
                        QuestionStatusDto::Failed
                    };
                    exchange.error = telemetry.error;
                }
            }
        }
        let terminal_failures: Vec<(CallId, ErrorCode)> = events
            .iter()
            .filter_map(|event| match event {
                CoordinatorEventDto::Terminal(telemetry)
                    if telemetry.outcome != TerminalOutcomeDto::Completed =>
                {
                    Some((
                        telemetry.call_id.clone(),
                        telemetry.error.unwrap_or(ErrorCode::Provider),
                    ))
                }
                _ => None,
            })
            .collect();
        for (call_id, code) in terminal_failures {
            self.fail_final_call(&call_id, code);
        }
        let control_changed = events.iter().any(|event| {
            matches!(
                event,
                CoordinatorEventDto::ControlChanged(_)
                    | CoordinatorEventDto::PersistenceWarning { .. }
            )
        });
        let provider_changed = events
            .iter()
            .any(|event| matches!(event, CoordinatorEventDto::ProviderState(_)));
        if !mutation && control_changed {
            self.bump_state();
        }
        // Auth/catalog projections refresh the DTO but deliberately do not invalidate a user's observed
        // control revision (Vertex can publish them every five seconds while unarmed).
        let changed = mutation || control_changed || provider_changed;
        for event in &events {
            (self.notifier)(event);
        }
        if changed {
            self.refresh_snapshot();
        }
    }

    fn bump_state(&mut self) {
        self.state_revision = self.state_revision.saturating_add(1);
    }

    fn refresh_snapshot(&self) {
        let providers: Vec<ProviderStateDto> =
            self.coordinator.provider_states().cloned().collect();
        *self.snapshot.lock().unwrap() = settings_snapshot(
            self.state_revision,
            &self.preferences.lock().unwrap(),
            &self.word_bank,
            self.coordinator.control_snapshot(),
            &providers,
        );
    }

    fn current_settings(&self) -> HostedSettingsDto {
        self.refresh_snapshot();
        self.snapshot.lock().unwrap().clone()
    }
}

impl TryFrom<HostedSelectionInput> for LaneSelectionDto {
    type Error = ErrorCode;

    fn try_from(value: HostedSelectionInput) -> Result<Self, Self::Error> {
        let provider = value
            .provider
            .map(ProviderId::new)
            .transpose()
            .map_err(|_| ErrorCode::PolicyBlocked)?;
        let transport = value
            .transport
            .map(TransportId::new)
            .transpose()
            .map_err(|_| ErrorCode::PolicyBlocked)?;
        let model = value
            .model
            .map(ModelId::new)
            .transpose()
            .map_err(|_| ErrorCode::PolicyBlocked)?;
        Ok(Self {
            provider,
            transport,
            model,
            cache_policy: CachePolicy {
                local: value.local_cache,
                provider: value.provider_cache,
            },
        })
    }
}

fn settings_snapshot(
    state_revision: u64,
    preferences: &HostedPreferences,
    word_bank: &WordBankDocument,
    control: &ControlSnapshotDto,
    providers: &[ProviderStateDto],
) -> HostedSettingsDto {
    let values = preferences.values();
    let mut providers = providers.to_vec();
    providers.sort_by(|left, right| {
        (
            left.descriptor.provider.as_str(),
            left.descriptor.transport.as_str(),
        )
            .cmp(&(
                right.descriptor.provider.as_str(),
                right.descriptor.transport.as_str(),
            ))
    });
    HostedSettingsDto {
        state_revision,
        preferences_revision: preferences.revision(),
        control: control.clone(),
        providers,
        scopes: vec![
            scope_dto("google", "vertex_api", &values.providers.vertex),
            scope_dto("openai", "openai_api", &values.providers.openai.scope),
            scope_dto(
                "anthropic",
                "anthropic_api",
                &values.providers.anthropic.scope,
            ),
        ],
        default_steering: values.default_steering.clone(),
        word_bank: HostedWordBankDto {
            revision: word_bank.revision(),
            entries: word_bank.entries().to_vec(),
        },
        final_deadline_seconds: values.final_deadline_seconds,
        show_history_diagnostics: values.show_history_diagnostics,
        show_live_metrics_by_default: values.show_live_metrics_by_default,
    }
}

fn scope_dto(
    provider: &str,
    transport: &str,
    scope: &ProviderScopePreferences,
) -> HostedProviderScopeDto {
    HostedProviderScopeDto {
        provider: provider.to_owned(),
        transport: transport.to_owned(),
        configured: scope.connection_scope_id.is_some(),
        alias: scope.alias.clone(),
        project: scope.project.clone(),
        region: scope.region.clone(),
        quota_project: scope.quota_project.clone(),
    }
}

fn initial_provider_states() -> Vec<ProviderStateDto> {
    crate::postprocess::provider_support_catalog()
        .into_iter()
        .map(|descriptor| ProviderStateDto {
            credential: if descriptor.support_tier == SupportTier::Documented {
                CredentialState::Absent
            } else {
                CredentialState::Unsupported {
                    code: ErrorCode::PolicyBlocked,
                }
            },
            descriptor,
            models: Vec::new(),
            service_error: None,
        })
        .collect()
}

fn lane_enabled(snapshot: &ControlSnapshotDto, family: LaneFamily) -> bool {
    snapshot.master_enabled
        && match family {
            LaneFamily::Live => snapshot.live.enabled,
            LaneFamily::Final => snapshot.final_lane.enabled,
            LaneFamily::Question => snapshot.questions.enabled,
        }
}

fn known_descriptor(provider: &ProviderId, transport: &TransportId) -> Option<ProviderDescriptor> {
    crate::postprocess::provider_support_catalog()
        .into_iter()
        .find(|candidate| &candidate.provider == provider && &candidate.transport == transport)
}

fn adapter_version(transport: &TransportId) -> u32 {
    match transport.as_str() {
        "vertex_api" => VERTEX_REST_ADAPTER_VERSION,
        "openai_api" => OPENAI_RESPONSES_ADAPTER_VERSION,
        "anthropic_api" => ANTHROPIC_MESSAGES_ADAPTER_VERSION,
        _ => 1,
    }
}

fn bounded_optional(value: Option<String>) -> Result<Option<String>, ErrorCode> {
    let value = value.map(|value| value.trim().to_owned());
    let value = value.filter(|value| !value.is_empty());
    if value
        .as_ref()
        .is_some_and(|value| value.len() > 1024 || value.chars().any(char::is_control))
    {
        Err(ErrorCode::PolicyBlocked)
    } else {
        Ok(value)
    }
}

fn validate_recording_id(value: &str) -> Result<(), ErrorCode> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        Err(ErrorCode::PolicyBlocked)
    } else {
        Ok(())
    }
}

fn transcript_rows(transcript: &DiarizedTranscript) -> Vec<TranscriptRow> {
    transcript
        .segments
        .iter()
        .enumerate()
        .filter_map(|(index, segment)| {
            let text = segment.text.trim();
            if text.is_empty() {
                return None;
            }
            Some(TranscriptRow {
                row_id: RowId::new(format!("final-row-{index:08}")).ok()?,
                speaker: segment.speaker.display().to_owned(),
                start_ms: seconds_to_millis(segment.start),
                end_ms: seconds_to_millis(segment.end).max(seconds_to_millis(segment.start)),
                text: text.to_owned(),
            })
        })
        .collect()
}

fn seconds_to_millis(value: f64) -> u64 {
    if !value.is_finite() || value <= 0.0 {
        0
    } else {
        (value * 1000.0).round().clamp(0.0, u64::MAX as f64) as u64
    }
}

type FinalChunk = (Vec<TranscriptRow>, Vec<TranscriptRow>);

fn final_chunks(rows: &[TranscriptRow]) -> Result<Vec<FinalChunk>, ErrorCode> {
    if rows.is_empty() {
        return Ok(vec![(Vec::new(), Vec::new())]);
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start < rows.len() {
        let mut end = start;
        let mut bytes = 0usize;
        while end < rows.len() {
            let row_bytes = rows[end].text.len().saturating_add(rows[end].speaker.len());
            if end > start && bytes.saturating_add(row_bytes) > REQUEST_CHUNK_BYTES {
                break;
            }
            bytes = bytes.saturating_add(row_bytes);
            end += 1;
        }
        if end == start {
            return Err(ErrorCode::PolicyBlocked);
        }
        ranges.push((start, end));
        if ranges.len() > MAX_FINAL_CHUNKS {
            return Err(ErrorCode::PolicyBlocked);
        }
        start = end;
    }
    Ok(ranges
        .into_iter()
        .map(|(start, end)| {
            let mut context = Vec::new();
            context.extend_from_slice(&rows[start.saturating_sub(2)..start]);
            context.extend_from_slice(&rows[end..rows.len().min(end.saturating_add(2))]);
            (rows[start..end].to_vec(), context)
        })
        .collect())
}

fn bounded_question_context(rows: &[TranscriptRow]) -> Vec<TranscriptRow> {
    const MAX_BYTES: usize = 256 * 1024;
    let mut bytes = 0usize;
    let mut start = rows.len();
    while start > 0 {
        let next = rows[start - 1]
            .text
            .len()
            .saturating_add(rows[start - 1].speaker.len());
        if bytes.saturating_add(next) > MAX_BYTES {
            break;
        }
        bytes = bytes.saturating_add(next);
        start -= 1;
    }
    rows[start..].to_vec()
}

fn transcript_fingerprint(
    key: &DigestKey,
    transcript: &DiarizedTranscript,
) -> Result<ProvenanceFingerprint> {
    let bytes =
        serde_json::to_vec(transcript).context("serializing transcript fingerprint input")?;
    ProvenanceFingerprint::new(key.fingerprint(FINGERPRINT_DOMAIN, &bytes).as_str())
        .map_err(anyhow::Error::new)
}

fn combined_cache(values: impl Iterator<Item = CacheObservation>) -> AppliedCacheSource {
    let mut distinct = Vec::new();
    for value in values {
        if !distinct.contains(&value) {
            distinct.push(value);
        }
    }
    if distinct.len() > 1 {
        return AppliedCacheSource::Mixed;
    }
    match distinct.pop().unwrap_or(CacheObservation::None) {
        CacheObservation::Local => AppliedCacheSource::Local,
        CacheObservation::ProviderRead
        | CacheObservation::ProviderWrite
        | CacheObservation::ProviderImplicit => AppliedCacheSource::Provider,
        CacheObservation::None => AppliedCacheSource::Network,
    }
}

fn fallback_final(
    transcript: DiarizedTranscript,
    code: ErrorCode,
    call_ids: Vec<CallId>,
    source_transcript_fingerprint: Option<ProvenanceFingerprint>,
) -> SettledFinalTranscript {
    let outcome = match code {
        ErrorCode::PolicyBlocked | ErrorCode::Canceled => FinalPostprocessOutcome::Disabled,
        ErrorCode::Timeout | ErrorCode::AuthUnarmed => FinalPostprocessOutcome::Timeout,
        ErrorCode::AmbiguousDispatch => FinalPostprocessOutcome::Ambiguous,
        _ => FinalPostprocessOutcome::Failed,
    };
    SettledFinalTranscript {
        transcript,
        applied_postprocess: AppliedPostprocessProvenance::not_applied(outcome)
            .unwrap_or_else(|_| AppliedPostprocessProvenance::none()),
        source_transcript_fingerprint,
        call_ids,
        hosted_text_applied: false,
        fallback_code: Some(code),
    }
}

fn control_error_code(error: ControlError) -> ErrorCode {
    match error {
        ControlError::EgressNotAcknowledged
        | ControlError::IncompleteLaneSelection
        | ControlError::PartialLaneSelection
        | ControlError::ProviderUnavailable
        | ControlError::ProviderBlocked
        | ControlError::ExperimentalProviderOff => ErrorCode::PolicyBlocked,
        ControlError::Persistence => ErrorCode::Cache,
        ControlError::GenerationOverflow => ErrorCode::Internal,
    }
}

fn submit_error_code(error: SubmitError) -> ErrorCode {
    match error {
        SubmitError::Disabled
        | SubmitError::WrongLane
        | SubmitError::SelectionChanged
        | SubmitError::ProviderBlocked
        | SubmitError::NoPinnedTemplate => ErrorCode::PolicyBlocked,
        SubmitError::StaleWatermark | SubmitError::DuplicateCall => ErrorCode::Superseded,
        SubmitError::Deadline => ErrorCode::Timeout,
        SubmitError::AdHocQueueFull | SubmitError::InvalidQuestion => ErrorCode::RateLimited,
        SubmitError::InvalidRecordingId
        | SubmitError::InvalidOutputLimit
        | SubmitError::GenerationOverflow => ErrorCode::Internal,
    }
}

fn coordinator_error_code(error: crate::postprocess::CoordinatorError) -> ErrorCode {
    match error {
        crate::postprocess::CoordinatorError::UnknownCall
        | crate::postprocess::CoordinatorError::EventFenceMismatch
        | crate::postprocess::CoordinatorError::StaleVertexAttempt
        | crate::postprocess::CoordinatorError::StaleApplication => ErrorCode::Superseded,
        crate::postprocess::CoordinatorError::Store => ErrorCode::Cache,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize};

    use corti_core::{OwningApp, RecordingMeta, Speaker, TranscriptSegment};
    use corti_postprocess::{
        AdapterCapabilities, CredentialSourceKind, EventContext, LatencyFields, ModelDescriptor,
        NormalizedUsage, ProviderEvent, ProviderEventKind, ProviderOutput, QuestionOutput,
        QuestionTerminal, Replacement, RewriteOutput,
    };

    use super::*;

    struct FixtureProviders {
        descriptor: ProviderDescriptor,
        model: ModelDescriptor,
    }

    impl FixtureProviders {
        fn openai() -> Self {
            Self::for_transport(corti_postprocess::KnownTransport::OpenAiDirect)
        }

        fn vertex() -> Self {
            Self::for_transport(corti_postprocess::KnownTransport::VertexDirect)
        }

        fn for_transport(transport: corti_postprocess::KnownTransport) -> Self {
            let descriptor = transport.descriptor();
            Self {
                model: ModelDescriptor {
                    provider: descriptor.provider.clone(),
                    transport: descriptor.transport.clone(),
                    support_tier: descriptor.support_tier,
                    exact_model_id: ModelId::new("fixture-model").unwrap(),
                    account_scoped_available: true,
                    region: None,
                    max_context_tokens: 32_000,
                    max_output_tokens: 4_096,
                    capabilities: AdapterCapabilities {
                        text_input: true,
                        text_output: true,
                        streaming: true,
                        structured_output: true,
                        explicit_prefix_cache: false,
                        implicit_cache_may_apply: false,
                    },
                    billing_basis: descriptor.billing_basis,
                    tariff_version: None,
                    deprecated: false,
                    benchmarked_for_live: true,
                },
                descriptor,
            }
        }
    }

    impl ProviderAccess for FixtureProviders {
        fn descriptor(
            &mut self,
            provider: &ProviderId,
            transport: &TransportId,
        ) -> Option<ProviderDescriptor> {
            (&self.descriptor.provider == provider && &self.descriptor.transport == transport)
                .then(|| self.descriptor.clone())
        }

        fn credential_state(
            &mut self,
            provider: &ProviderId,
            transport: &TransportId,
        ) -> CredentialState {
            if &self.descriptor.provider == provider && &self.descriptor.transport == transport {
                CredentialState::Ready {
                    expires_at_unix_ms: None,
                    source: CredentialSourceKind::Keychain,
                }
            } else {
                CredentialState::Unsupported {
                    code: ErrorCode::PolicyBlocked,
                }
            }
        }

        fn catalog(
            &mut self,
            provider: &ProviderId,
            transport: &TransportId,
            _scope: &ProviderScope,
        ) -> Result<ModelCatalog, corti_postprocess::PostprocessError> {
            if &self.descriptor.provider == provider && &self.descriptor.transport == transport {
                Ok(ModelCatalog {
                    models: vec![self.model.clone()],
                })
            } else {
                Err(ErrorCode::PolicyBlocked.into())
            }
        }
    }

    struct RewriteExecutor;

    impl TicketExecutor for RewriteExecutor {
        fn execute(
            &self,
            ticket: &DispatchTicket,
            sink: &dyn ProviderEventSink,
        ) -> Result<ProviderTerminal, corti_postprocess::PostprocessError> {
            sink.emit(ProviderEvent {
                context: event_context(ticket.request()),
                kind: ProviderEventKind::DispatchStarted,
            });
            let output = if ticket.request().lane.is_question() {
                ProviderOutput::Question(QuestionTerminal {
                    output: QuestionOutput {
                        schema: 1,
                        answer: "fixture grounded answer".to_string(),
                        cited_row_ids: ticket
                            .request()
                            .context
                            .first()
                            .map(|row| vec![row.row_id.clone()])
                            .unwrap_or_default(),
                        context_truncated: false,
                    },
                })
            } else {
                ProviderOutput::Rewrite(RewriteOutput {
                    schema: 1,
                    replacements: ticket
                        .request()
                        .targets
                        .iter()
                        .map(|row| Replacement {
                            row_id: row.row_id.clone(),
                            text: "fixture corrected text".to_string(),
                        })
                        .collect(),
                })
            };
            Ok(ProviderTerminal {
                output,
                usage: NormalizedUsage::unknown(),
                latency: LatencyFields::default(),
                cache: CacheObservation::None,
            })
        }
    }

    #[derive(Clone)]
    struct ManualClock {
        monotonic: Arc<AtomicU64>,
        wall: Arc<AtomicI64>,
    }

    impl ManualClock {
        fn new() -> Self {
            Self {
                monotonic: Arc::new(AtomicU64::new(0)),
                wall: Arc::new(AtomicI64::new(1_800_000_000_000)),
            }
        }

        fn set(&self, micros: u64) {
            self.monotonic.store(micros, Ordering::SeqCst);
        }
    }

    impl corti_postprocess_providers::Clock for ManualClock {
        fn monotonic_micros(&self) -> u64 {
            self.monotonic.load(Ordering::SeqCst)
        }
    }

    impl CoordinatorClock for ManualClock {
        fn unix_millis(&self) -> i64 {
            self.wall.load(Ordering::SeqCst)
        }
    }

    struct ArmsOnSecondResolution(AtomicUsize);

    impl VertexResolver for ArmsOnSecondResolution {
        fn resolve(&self, _attempt: &VertexResolutionAttempt) -> VertexResolutionOutcome {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                VertexResolutionOutcome::Unarmed
            } else {
                VertexResolutionOutcome::Ready {
                    expires_at_unix_ms: Some(1_800_003_600_000),
                }
            }
        }
    }

    struct RecordingExecutor {
        target_texts: Mutex<Vec<Vec<String>>>,
    }

    impl RecordingExecutor {
        fn new() -> Self {
            Self {
                target_texts: Mutex::new(Vec::new()),
            }
        }
    }

    impl TicketExecutor for RecordingExecutor {
        fn execute(
            &self,
            ticket: &DispatchTicket,
            sink: &dyn ProviderEventSink,
        ) -> Result<ProviderTerminal, corti_postprocess::PostprocessError> {
            self.target_texts.lock().unwrap().push(
                ticket
                    .request()
                    .targets
                    .iter()
                    .map(|row| row.text.clone())
                    .collect(),
            );
            RewriteExecutor.execute(ticket, sink)
        }
    }

    struct BlockingExecutor {
        started: Mutex<bool>,
        wake: Condvar,
    }

    impl BlockingExecutor {
        fn new() -> Self {
            Self {
                started: Mutex::new(false),
                wake: Condvar::new(),
            }
        }

        fn wait_started(&self) {
            let mut started = self.started.lock().unwrap();
            while !*started {
                started = self.wake.wait(started).unwrap();
            }
        }
    }

    impl TicketExecutor for BlockingExecutor {
        fn execute(
            &self,
            ticket: &DispatchTicket,
            sink: &dyn ProviderEventSink,
        ) -> Result<ProviderTerminal, corti_postprocess::PostprocessError> {
            sink.emit(ProviderEvent {
                context: event_context(ticket.request()),
                kind: ProviderEventKind::DispatchStarted,
            });
            *self.started.lock().unwrap() = true;
            self.wake.notify_all();
            while !ticket.cancellation().is_cancelled() {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(ticket
                .cancellation()
                .reason()
                .map(CancellationReason::error_code)
                .unwrap_or(ErrorCode::Canceled)
                .into())
        }
    }

    fn event_context(request: &HostedRequest) -> EventContext {
        EventContext {
            call_id: request.call_id.clone(),
            group_id: request.group_id.clone(),
            target_id: request.target_id.clone(),
            lane: request.lane,
            fence: request.fence.clone(),
        }
    }

    fn configured_preferences() -> HostedPreferences {
        HostedPreferences::default()
            .revise(|values| {
                let descriptor = corti_postprocess::KnownTransport::OpenAiDirect.descriptor();
                values.egress_acknowledgement_version = Some(EGRESS_DISCLOSURE_VERSION);
                values.master_enabled = true;
                values.final_lane.enabled = true;
                values.final_lane.provider = Some(descriptor.provider);
                values.final_lane.transport = Some(descriptor.transport);
                values.final_lane.model = Some(ModelId::new("fixture-model").unwrap());
                values.questions.enabled = true;
                values.questions.provider = values.final_lane.provider.clone();
                values.questions.transport = values.final_lane.transport.clone();
                values.questions.model = values.final_lane.model.clone();
                values.providers.openai.scope.connection_scope_id =
                    Some(ConnectionScopeId::new("fixture-scope").unwrap());
                values.final_deadline_seconds = 2;
            })
            .unwrap()
    }

    fn vertex_preferences() -> HostedPreferences {
        HostedPreferences::default()
            .revise(|values| {
                let descriptor = corti_postprocess::KnownTransport::VertexDirect.descriptor();
                values.egress_acknowledgement_version = Some(EGRESS_DISCLOSURE_VERSION);
                values.master_enabled = true;
                values.live.enabled = true;
                values.live.provider = Some(descriptor.provider);
                values.live.transport = Some(descriptor.transport);
                values.live.model = Some(ModelId::new("fixture-model").unwrap());
                values.providers.vertex.connection_scope_id =
                    Some(ConnectionScopeId::new("fixture-vertex-scope").unwrap());
            })
            .unwrap()
    }

    fn dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "corti-postprocess-app-{name}-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn start_fixture(
        name: &str,
        executor: Arc<dyn TicketExecutor>,
        events: Arc<Mutex<Vec<CoordinatorEventDto>>>,
    ) -> (HostedHandle, Receiver<PipelineMsg>, PathBuf) {
        let path = dir(name);
        let outbox = Arc::new(TelemetryOutbox::open(path.join("postprocess-outbox.json")).unwrap());
        let (pipeline_tx, pipeline_rx) = std::sync::mpsc::channel();
        let sink = events.clone();
        let notifier: EventNotifier = Arc::new(move |event| {
            sink.lock().unwrap().push(event.clone());
        });
        let (_, handle) = start_with_components(
            configured_preferences(),
            WordBankDocument::empty(),
            LiveTranscriptStore::detached(),
            pipeline_tx,
            outbox,
            executor,
            Box::new(FixtureProviders::openai()),
            Arc::new(NoPricing),
            Arc::new(UnarmedVertex),
            notifier,
            DigestKey::new([7; 32]),
            ProcessEpoch(77),
            false,
            None,
        )
        .unwrap();
        (handle, pipeline_rx, path)
    }

    fn raw_transcript() -> DiarizedTranscript {
        DiarizedTranscript::new(vec![TranscriptSegment {
            speaker: Speaker::Me,
            start: 1.0,
            end: 2.0,
            text: "fixture raw text".to_string(),
        }])
    }

    fn meta(audio: PathBuf) -> RecordingMeta {
        RecordingMeta {
            started_at: chrono::Local::now(),
            ended_at: Some(chrono::Local::now()),
            owning_app: OwningApp::from_bundle_id("fixture.app"),
            audio_path: audio,
        }
    }

    #[test]
    fn settings_selection_is_bound_to_the_refreshed_provider_catalog() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (handle, _pipeline_rx, path) =
            start_fixture("catalog-selection", Arc::new(RewriteExecutor), events);
        let descriptor = corti_postprocess::KnownTransport::OpenAiDirect.descriptor();
        let (reply, receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::RefreshProvider {
                provider: descriptor.provider.clone(),
                transport: descriptor.transport.clone(),
                reply,
            })
            .unwrap();
        assert_eq!(receive.recv().unwrap().unwrap().models.len(), 1);

        let stale = handle.snapshot();
        let rejected = handle.patch_for_test(HostedPatchRequest {
            observed_state_revision: stale.state_revision,
            patch: HostedPatchInput::SetLaneSelection {
                lane: HostedLaneDto::Live,
                selection: HostedSelectionInput {
                    provider: Some(descriptor.provider.as_str().to_owned()),
                    transport: Some(descriptor.transport.as_str().to_owned()),
                    model: Some("stale-model".into()),
                    local_cache: LocalCacheMode::Reusable,
                    provider_cache: ProviderCacheMode::Off,
                },
            },
        });
        assert!(matches!(rejected, Err(ErrorCode::ModelUnavailable)));

        let current = handle.snapshot();
        let accepted = handle
            .patch_for_test(HostedPatchRequest {
                observed_state_revision: current.state_revision,
                patch: HostedPatchInput::SetLaneSelection {
                    lane: HostedLaneDto::Live,
                    selection: HostedSelectionInput {
                        provider: Some(descriptor.provider.as_str().to_owned()),
                        transport: Some(descriptor.transport.as_str().to_owned()),
                        model: Some("fixture-model".into()),
                        local_cache: LocalCacheMode::Reusable,
                        provider_cache: ProviderCacheMode::Off,
                    },
                },
            })
            .unwrap();
        assert!(matches!(accepted, HostedMutationResult::Applied { .. }));
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn final_result_precedes_checkpoint_and_history_import_is_idempotent() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (handle, _pipeline_rx, path) =
            start_fixture("final-order", Arc::new(RewriteExecutor), events);
        let audio = path.join("recording.wav");
        std::fs::write(&audio, b"fixture-audio-not-sent").unwrap();
        let queue = Queue::open_at(path.join("queue.db")).unwrap();
        let id = queue.enqueue(&meta(audio.clone())).unwrap();
        assert_eq!(id, "recording");
        let checkpoint_path = crate::checkpoint::path_for(&audio);
        assert!(!checkpoint_path.exists());

        let settled = handle.finalize(&id, raw_transcript(), false);
        assert!(settled.hosted_text_applied);
        assert_eq!(
            settled.transcript.segments[0].text,
            "fixture corrected text"
        );
        assert!(
            !checkpoint_path.exists(),
            "final must settle before checkpoint"
        );
        let outbox_text =
            String::from_utf8(std::fs::read(path.join("postprocess-outbox.json")).unwrap())
                .unwrap();
        assert!(!outbox_text.contains("fixture raw text"));
        assert!(!outbox_text.contains("fixture corrected text"));
        handle.mark_final_applied(&settled.call_ids).unwrap();

        let mut checkpoint =
            crate::checkpoint::FilingCheckpoint::new(settled.transcript.clone(), None, None);
        checkpoint
            .set_applied_postprocess(settled.applied_postprocess.clone())
            .unwrap();
        checkpoint
            .set_final_attempt_call_ids(settled.call_ids.clone())
            .unwrap();
        checkpoint.store(&audio).unwrap();
        handle.mark_final_checkpointed(&settled.call_ids);
        assert_eq!(
            crate::checkpoint::FilingCheckpoint::load(&audio)
                .unwrap()
                .transcript,
            settled.transcript
        );

        assert_eq!(handle.import_outbox(&queue).unwrap(), 1);
        assert_eq!(handle.import_outbox(&queue).unwrap(), 0);
        let history = queue.postprocess_history(&id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].call_id, settled.call_ids[0]);
        assert!(!history[0].provider_request_sent || history[0].error_code.is_none());
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn disabling_during_dispatch_cancels_and_returns_raw_without_application() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let executor = Arc::new(BlockingExecutor::new());
        let (handle, _pipeline_rx, path) = start_fixture("toggle-cancel", executor.clone(), events);
        let worker_handle = handle.clone();
        let final_thread = std::thread::spawn(move || {
            worker_handle.finalize("recording", raw_transcript(), false)
        });
        executor.wait_started();
        let mutation = loop {
            let before = handle.snapshot();
            let mutation = handle
                .patch_for_test(HostedPatchRequest {
                    observed_state_revision: before.state_revision,
                    patch: HostedPatchInput::SetMaster { enabled: false },
                })
                .unwrap();
            if !matches!(mutation, HostedMutationResult::Conflict { .. }) {
                break mutation;
            }
        };
        assert!(matches!(mutation, HostedMutationResult::Applied { .. }));
        let settled = final_thread.join().unwrap();
        assert!(!settled.hosted_text_applied);
        assert_eq!(settled.transcript, raw_transcript());
        assert!(matches!(
            settled.fallback_code,
            Some(ErrorCode::Canceled | ErrorCode::Superseded)
        ));
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn a_new_session_makes_an_unapplied_final_fence_stale() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (handle, _pipeline_rx, path) =
            start_fixture("stale-fence", Arc::new(RewriteExecutor), events);
        let settled = handle.finalize("old-recording", raw_transcript(), false);
        assert!(settled.hosted_text_applied);
        handle.begin_live_session("new-recording").unwrap();
        assert_eq!(
            handle.mark_final_applied(&settled.call_ids),
            Err(ErrorCode::Superseded)
        );
        std::fs::remove_dir_all(path).ok();
    }

    fn assistant(handle: &HostedHandle) -> AssistantSnapshotDto {
        let (reply, receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::AssistantSnapshot { reply })
            .unwrap();
        receive.recv().unwrap()
    }

    #[test]
    fn sidebar_ad_hoc_and_pinned_commands_keep_bounded_session_content() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (handle, _pipeline_rx, path) =
            start_fixture("assistant", Arc::new(RewriteExecutor), events);
        handle.begin_live_session("recording").unwrap();
        handle
            .try_observe_finalized_rows(
                "recording",
                vec![TranscriptRow {
                    row_id: RowId::new("assistant-row-1").unwrap(),
                    speaker: "Me".into(),
                    start_ms: 0,
                    end_ms: 1_000,
                    text: "fixture context for a grounded answer".into(),
                }],
            )
            .unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let (reply, receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::SubmitAdHoc {
                question: "fixture question".into(),
                reply,
            })
            .unwrap();
        let call_id = receive.recv().unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = assistant(&handle);
            if snapshot.exchanges.iter().any(|exchange| {
                exchange.call_id == call_id && exchange.status == QuestionStatusDto::Completed
            }) {
                let exchange = snapshot
                    .exchanges
                    .iter()
                    .find(|exchange| exchange.call_id == call_id)
                    .unwrap();
                assert_eq!(exchange.answer.as_deref(), Some("fixture grounded answer"));
                break;
            }
            assert!(Instant::now() < deadline, "ad-hoc answer did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }

        let (reply, receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::SetPinnedTemplate {
                template: "fixture pinned question".into(),
                reply,
            })
            .unwrap();
        receive.recv().unwrap().unwrap();
        std::thread::sleep(Duration::from_millis(550));
        let settings = handle.snapshot();
        handle
            .patch_for_test(HostedPatchRequest {
                observed_state_revision: settings.state_revision,
                patch: HostedPatchInput::SetPinnedAuto {
                    enabled: true,
                    acknowledged: true,
                },
            })
            .unwrap();
        let forty_words = (0..40)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        handle
            .try_observe_finalized_rows(
                "recording",
                vec![TranscriptRow {
                    row_id: RowId::new("assistant-row-2").unwrap(),
                    speaker: "Me".into(),
                    start_ms: 1_000,
                    end_ms: 32_000,
                    text: forty_words,
                }],
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = assistant(&handle);
            if snapshot.pinned.as_ref().is_some_and(|exchange| {
                exchange.status == QuestionStatusDto::Completed
                    && exchange.answer.as_deref() == Some("fixture grounded answer")
            }) {
                assert_eq!(snapshot.pinned_run_count, 1);
                break;
            }
            assert!(Instant::now() < deadline, "pinned answer did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.end_live_session("recording");
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn settings_and_events_serialize_without_secret_capability() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (handle, _pipeline_rx, path) =
            start_fixture("secret-free", Arc::new(RewriteExecutor), events.clone());
        let settings = serde_json::to_string(&handle.snapshot()).unwrap();
        let assistant_debug = format!(
            "{:?}",
            AssistantExchangeDto {
                call_id: CallId::new("fixture-assistant").unwrap(),
                as_of_revision: 1,
                status: QuestionStatusDto::Completed,
                error: None,
                question: "fixture private question".into(),
                answer: Some("fixture private answer".into()),
                cost_label: None,
            }
        );
        assert!(!assistant_debug.contains("private question"));
        assert!(!assistant_debug.contains("private answer"));
        for forbidden in [
            "fixture-secret-value",
            "bearer ",
            "access_token",
            "refresh_token",
            "api_key_value",
            "audio_bytes",
        ] {
            assert!(!settings.to_ascii_lowercase().contains(forbidden));
        }
        let settled = handle.finalize("recording", raw_transcript(), false);
        assert!(settled.hosted_text_applied);
        std::thread::sleep(Duration::from_millis(20));
        let serialized_events = serde_json::to_string(&*events.lock().unwrap()).unwrap();
        assert!(!serialized_events.contains("fixture raw text"));
        assert!(!serialized_events.contains("fixture corrected text"));
        assert!(!serialized_events.contains("fixture-secret-value"));
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn queued_unarmed_auth_expires_to_raw_with_call_provenance() {
        let path = dir("auth-timeout");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("postprocess-outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let preferences = configured_preferences()
            .revise(|values| values.final_deadline_seconds = 1)
            .unwrap();
        let (_, handle) = start_with_components(
            preferences,
            WordBankDocument::empty(),
            LiveTranscriptStore::detached(),
            pipeline_tx,
            outbox,
            Arc::new(DenyExecutor),
            Box::new(UnavailableProviders),
            Arc::new(NoPricing),
            Arc::new(UnarmedVertex),
            Arc::new(|_| {}),
            DigestKey::new([9; 32]),
            ProcessEpoch(88),
            false,
            None,
        )
        .unwrap();
        let settled = handle.finalize("recording", raw_transcript(), false);
        assert_eq!(settled.transcript, raw_transcript());
        assert!(!settled.hosted_text_applied);
        assert_eq!(settled.fallback_code, Some(ErrorCode::Timeout));
        assert!(!settled.call_ids.is_empty());
        assert_eq!(
            settled.applied_postprocess.final_outcome(),
            Some(FinalPostprocessOutcome::Timeout)
        );
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn disabled_auth_deadline_error_and_ambiguous_paths_all_preserve_raw() {
        let cases = [
            (ErrorCode::PolicyBlocked, FinalPostprocessOutcome::Disabled),
            (ErrorCode::AuthUnarmed, FinalPostprocessOutcome::Timeout),
            (ErrorCode::Timeout, FinalPostprocessOutcome::Timeout),
            (ErrorCode::Provider, FinalPostprocessOutcome::Failed),
            (
                ErrorCode::AmbiguousDispatch,
                FinalPostprocessOutcome::Ambiguous,
            ),
        ];
        for (code, expected) in cases {
            let raw = raw_transcript();
            let settled = fallback_final(raw.clone(), code, Vec::new(), None);
            assert_eq!(settled.transcript, raw);
            assert!(!settled.hosted_text_applied);
            assert_eq!(settled.applied_postprocess.final_outcome(), Some(expected));
        }
    }

    #[test]
    fn vertex_unarmed_event_and_app_catch_up_dispatch_only_the_newest_live_snapshot() {
        let path = dir("vertex-catch-up");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("postprocess-outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let notifier: EventNotifier = Arc::new(move |event| {
            sink.lock().unwrap().push(event.clone());
        });
        let clock = Arc::new(ManualClock::new());
        let executor = Arc::new(RecordingExecutor::new());
        let resolver = Arc::new(ArmsOnSecondResolution(AtomicUsize::new(0)));
        let (_, handle) = start_with_components(
            vertex_preferences(),
            WordBankDocument::empty(),
            LiveTranscriptStore::detached(),
            pipeline_tx,
            outbox,
            executor.clone(),
            Box::new(FixtureProviders::vertex()),
            Arc::new(NoPricing),
            resolver.clone(),
            notifier,
            DigestKey::new([5; 32]),
            ProcessEpoch(99),
            false,
            Some(clock.clone()),
        )
        .unwrap();
        handle.begin_live_session("recording").unwrap();
        std::thread::sleep(Duration::from_millis(60));

        let row = |id: &str, text: &str| TranscriptRow {
            row_id: RowId::new(id).unwrap(),
            speaker: "Me".into(),
            start_ms: 0,
            end_ms: 1_000,
            text: text.into(),
        };
        handle
            .try_observe_finalized_rows("recording", vec![row("vertex-old", "old snapshot")])
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));
        clock.set(150_000);
        std::thread::sleep(Duration::from_millis(60));
        handle
            .try_observe_finalized_rows("recording", vec![row("vertex-middle", "middle snapshot")])
            .unwrap();
        handle
            .try_observe_finalized_rows("recording", vec![row("vertex-new", "newest snapshot")])
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));
        clock.set(300_000);
        std::thread::sleep(Duration::from_millis(60));
        assert!(executor.target_texts.lock().unwrap().is_empty());

        clock.set(5_000_000);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !executor.target_texts.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Vertex catch-up did not dispatch"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let calls = executor.target_texts.lock().unwrap().clone();
        assert_eq!(calls, vec![vec!["newest snapshot".to_string()]]);
        assert_eq!(resolver.0.load(Ordering::SeqCst), 2);
        let notices: Vec<_> = events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                CoordinatorEventDto::Notice(notice) => Some(notice.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].role, "alert");
        assert_eq!(notices[0].visible_message, "gcloud token isn't armed");
        handle.end_live_session("recording");
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn warning_contract_and_provider_posture_remain_exact() {
        assert_eq!(
            corti_postprocess::VERTEX_UNARMED_WARNING,
            "gcloud token isn't armed"
        );
        let descriptors = crate::postprocess::provider_support_catalog();
        let claude = descriptors
            .iter()
            .find(|descriptor| descriptor.transport.as_str() == "claude_subscription")
            .unwrap();
        assert_eq!(claude.support_tier, SupportTier::Blocked);
        let codex = descriptors
            .iter()
            .find(|descriptor| descriptor.transport.as_str() == "codex_app_server")
            .unwrap();
        assert_eq!(codex.support_tier, SupportTier::Experimental);
        assert!(!codex.adapter_available);
    }
}
