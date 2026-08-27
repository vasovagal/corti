//! Tauri/backend wiring for the hosted post-processing coordinator.
//!
//! The coordinator and every content-bearing request remain on a dedicated thread. Live ASR publishes raw
//! rows first, then uses the coordinator's bounded `try_send` ingress. Batch/live final callers may wait only
//! after ASR has completed and the raw transcript is already recoverable. Production provider execution is
//! deliberately deny-by-default until an approved credential/store factory is installed; this wiring still
//! exposes truthful provider posture, Vertex arming/catch-up, controls, history, and hermetic injection seams.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use corti_core::DiarizedTranscript;
use corti_postprocess::{
    BillingBasis, CacheObservation, CachePolicy, CallId, CancellationReason, CanonicalPrompt,
    ConnectionScopeId, CostEstimate, CredentialState, DigestKey, ErrorCode, HostedRequest, Lane,
    LocalCacheMode, ModelCatalog, ModelId, MonotonicDeadline, OUTPUT_SCHEMA_VERSION,
    PROMPT_TEMPLATE_VERSION, PricingCatalog, PricingError, PricingQuery, ProcessEpoch, PromptTask,
    ProviderAdapter, ProviderCacheKey, ProviderCacheKeyMaterial, ProviderCacheMode,
    ProviderDescriptor, ProviderEventSink, ProviderId, ProviderScope, ProviderTerminal,
    RequestFence, RequestGroupId, RequestKey, RequestKeyMaterial, RowId, SupportTier, TargetId,
    TranscriptRow, TransportId, WordBankDocument,
};
use corti_postprocess_providers::{
    ANTHROPIC_MESSAGES_ADAPTER_VERSION, AnthropicMessagesAdapter, ApiKey, ApiKeySource,
    BEDROCK_CONVERSE_ADAPTER_VERSION, BedrockConverseAdapter, CHATGPT_DEVICE_VERIFICATION_URL,
    CHATGPT_SUBSCRIPTION_ADAPTER_VERSION, ChatGptAuthError, ChatGptCredentialStore,
    ChatGptLoginPoll, ChatGptStoreError, ChatGptSubscriptionAdapter, ChatGptSubscriptionAuth,
    CredentialError, HttpTransport, OPENAI_RESPONSES_ADAPTER_VERSION, OpenAiResponsesAdapter,
    SystemClock as ProviderSystemClock, UreqTransport, VERTEX_REST_ADAPTER_VERSION, VertexModel,
    VertexProjectMetadata, VertexResolutionAttempt, VertexResolutionOutcome, VertexRestAdapter,
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

use crate::bedrock_creds::{
    BedrockAdapterCredentials, BedrockCredentialConfig, BedrockCredentialResolver,
};
use crate::live_view::LiveTranscriptStore;
use crate::pipeline::PipelineMsg;
use crate::postprocess::{
    CompletionOutcome, ControlError, ControlPatch, ControlPersistence, ControlSnapshotDto,
    CoordinatorClock, CoordinatorEventDto, CoordinatorIngress, DispatchOutcome, DispatchTicket,
    EncryptedPostprocessStore, ExactLookup, FinalJournalBoundary, FinalJournalState,
    FinalRecoveryDirective, FinalRecoveryRecord, HotPathCommand, IngressError, LaneControlDto,
    LaneFamily, LaneSelectionDto, PatchOutcome, PostprocessCoordinator, ProviderAccess,
    ProviderPreparation, ProviderPrepareSpec, ProviderStateDto, QuestionStatusDto,
    QueuedProviderPrepare, RequestSubmission, StoreCommit, SubmitError, TerminalOutcomeDto,
    TerminalTelemetryDto, TranscriptWatermark, prepare_provider,
};
use crate::postprocess_config::{
    AwsCredentialMode, BedrockProviderPreferences, EGRESS_DISCLOSURE_VERSION, HostedPreferences,
    MAX_VERTEX_MODELS, PINNED_AUTO_DISCLOSURE_VERSION, ProviderScopePreferences, SecretPurpose,
};
use crate::private_file::{atomic_write_private, read_private};
use crate::vertex_creds::{VertexAdapterCredentials, VertexAdcResolver, VertexConnectionConfig};

pub(crate) const HOSTED_STATE_CHANGED_EVENT: &str = "hosted-state-changed";
const SERVICE_COMMAND_CAPACITY: usize = 256;
const SERVICE_TICK: Duration = Duration::from_millis(20);
const SERVICE_IDLE_POLL: Duration = Duration::from_millis(2);
const MAX_PRIORITY_DRAIN: usize = 32;
const MAX_COMMAND_DRAIN: usize = 32;
const MAX_INGRESS_DRAIN: usize = 64;
const MAX_PROVIDER_EVENT_DRAIN: usize = 64;
const MAX_WORKER_DRAIN: usize = 32;
const MAX_PROVIDER_PREPARE_DRAIN: usize = 32;
const MAX_CONCURRENT_PROVIDER_PREPARES: usize = 8;
const MAX_CONCURRENT_PREPARES_PER_PROVIDER: usize = 2;
const MAX_VERTEX_DRAIN: usize = 8;
const MAX_DISPATCH_DRAIN: usize = 16;
const MAX_FINAL_CHUNKS: usize = 64;
const MAX_FINAL_INPUT_TOKENS: u64 = 16 * 1024;
const DEFAULT_INPUT_TOKEN_BUDGET: u64 = 8 * 1024;
const PROMPT_TOKEN_RESERVE: u64 = 2 * 1024;
const MAX_LIVE_TARGET_BYTES: usize = 4 * 1024;
const MAX_LIVE_TARGET_ROWS: usize = 8;
const MAX_CONTEXT_ROWS: usize = 8;
const MAX_SESSION_LEDGER_BYTES: usize = 16 * 1024 * 1024;
const OUTBOX_SCHEMA: u32 = 1;
const MAX_OUTBOX_BYTES: usize = 16 * 1024 * 1024;
const STORE_SCHEMA: u32 = 1;
const STORE_MAGIC: &[u8] = b"CORTIPPE1";
const STORE_AAD: &[u8] = b"corti-hosted-store-v1";
const STORE_NONCE_BYTES: usize = 12;
const MAX_STORE_BYTES: usize = 64 * 1024 * 1024;
const MAX_STORE_JOURNALS: usize = 1_024;
const MAX_STORE_CACHE_ENTRIES: usize = 1_024;
const REUSABLE_CACHE_MAX_AGE_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const MAX_MEMORY_CACHE_ENTRIES: usize = 128;
const MAX_MEMORY_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MEMORY_CACHE_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1_000;
const OPENAI_API_KEY_ACCOUNT: &str = SecretPurpose::OpenAiApiKey.slot_name();
const ANTHROPIC_API_KEY_ACCOUNT: &str = SecretPurpose::AnthropicApiKey.slot_name();
const FINGERPRINT_DOMAIN: &[u8] = b"corti-app-provenance-v1\0";
const CHATGPT_PROVIDER: &str = "openai";
const CHATGPT_TRANSPORT: &str = "chatgpt_subscription";
const VERTEX_PROVIDER: &str = "google";
const VERTEX_TRANSPORT: &str = "vertex_api";

/// Managed Tauri state. The handle is cloneable; the coordinator itself never leaves its owner thread.
pub(crate) struct HostedState {
    handle: HostedHandle,
    chatgpt_auth: Option<ChatGptSubscriptionAuth>,
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
    priority_tx: Sender<ServiceCommand>,
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
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send_priority(ServiceCommand::BeginSession {
            recording_id: recording_id.to_owned(),
            reply: reply_tx,
        })?;
        let result = reply_rx.recv().unwrap_or(Err(ErrorCode::Internal));
        if result.is_err() {
            self.ingress_incomplete.store(true, Ordering::Release);
        }
        result
    }

    #[cfg_attr(not(feature = "local"), allow(dead_code))]
    pub(crate) fn end_live_session(&self, recording_id: &str) -> Result<(), ErrorCode> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send_priority(ServiceCommand::EndSession {
            recording_id: recording_id.to_owned(),
            reply: reply_tx,
        })?;
        reply_rx.recv().unwrap_or(Err(ErrorCode::Internal))
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

    /// Off-hot-path FIFO fence for the ASR tail. Real calls and microphone tests use it only after capture
    /// has closed, so the final row cannot be overtaken by Final or EndSession.
    pub(crate) fn flush_finalized_rows(&self) -> Result<(), IngressError> {
        self.ingress.barrier()
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
        // Live/test rows use a nonblocking channel. Once ASR has finished it is safe—and necessary—to wait
        // for a FIFO barrier so Final cannot overtake the tail row and observe it twice or fence an old ledger.
        if live_session && self.flush_finalized_rows().is_err() {
            self.ingress_incomplete.store(true, Ordering::Release);
            return fallback_final(raw, ErrorCode::Internal, Vec::new(), None);
        }
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
        self.send_priority(ServiceCommand::MarkFinalApplied {
            call_ids: call_ids.to_vec(),
            reply: reply_tx,
        })?;
        reply_rx.recv().unwrap_or(Err(ErrorCode::Internal))
    }

    pub(crate) fn abandon_final_result(&self, call_ids: &[CallId]) -> Result<(), ErrorCode> {
        if call_ids.is_empty() {
            return Ok(());
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send_priority(ServiceCommand::AbandonFinal {
            call_ids: call_ids.to_vec(),
            reply: reply_tx,
        })?;
        reply_rx.recv().unwrap_or(Err(ErrorCode::Internal))
    }

    /// Acknowledge only after the FilingCheckpoint or live-note final state is durable.
    pub(crate) fn mark_final_checkpointed(&self, call_ids: &[CallId]) -> Result<(), ErrorCode> {
        if call_ids.is_empty() {
            return Ok(());
        }
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send_priority(ServiceCommand::MarkFinalCheckpointed {
            call_ids: call_ids.to_vec(),
            reply: reply_tx,
        })?;
        reply_rx.recv().unwrap_or(Err(ErrorCode::Internal))
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

    fn sync_chatgpt_credential(&self) -> Result<ProviderStateDto, ErrorCode> {
        let provider = ProviderId::new(CHATGPT_PROVIDER).map_err(|_| ErrorCode::Internal)?;
        let transport = TransportId::new(CHATGPT_TRANSPORT).map_err(|_| ErrorCode::Internal)?;
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send(ServiceCommand::SyncProviderCredential {
            provider,
            transport,
            reply: reply_tx,
        })?;
        reply_rx.recv().unwrap_or(Err(ErrorCode::Internal))
    }

    fn install_chatgpt_scope(
        &self,
        scope_id: Option<ConnectionScopeId>,
        refresh_catalog: bool,
    ) -> Result<ProviderStateDto, ErrorCode> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send(ServiceCommand::InstallChatGptScope {
            scope_id,
            refresh_catalog,
            reply: reply_tx,
        })?;
        reply_rx.recv().unwrap_or(Err(ErrorCode::Internal))
    }

    fn notify_provider_secret_changed(&self, request: SecretSlotRequest) -> Result<(), ErrorCode> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send(ServiceCommand::ProviderSecretChanged {
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

    fn send_priority(&self, command: ServiceCommand) -> Result<(), ErrorCode> {
        self.priority_tx
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

/// Bedrock's credential configuration as the pane sees it: the mode plus its non-secret companions, and
/// booleans for the secret slots. No key, token, or account id is representable.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BedrockCredentialDto {
    pub(crate) mode: crate::postprocess_config::AwsCredentialMode,
    pub(crate) profile: Option<String>,
    pub(crate) role_arn: Option<String>,
    pub(crate) has_access_key_id: bool,
    pub(crate) has_secret_access_key: bool,
    pub(crate) has_session_token: bool,
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
    /// Exact Vertex model ids the operator typed, in save order.
    pub(crate) vertex_models: Vec<String>,
    pub(crate) bedrock: BedrockCredentialDto,
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
    Invalid {
        settings: HostedSettingsDto,
        field: HostedMutationInvalidField,
        reason: HostedMutationInvalidReason,
    },
    DisabledForSession {
        settings: HostedSettingsDto,
        code: ErrorCode,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostedMutationInvalidField {
    Profile,
    RoleArn,
    Region,
    SetupName,
    KeyPair,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostedMutationInvalidReason {
    Required,
    NotFound,
    Invalid,
    KeysMissing,
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
pub(crate) struct PinnedQuestionUpdateRequest {
    pub(crate) observed_state_revision: u64,
    pub(crate) template: String,
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
pub(crate) fn get_hosted_settings(
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<HostedSettingsDto, String> {
    require_hosted_window(&window, &["live", "settings"])?;
    Ok(state.handle.snapshot())
}

#[tauri::command]
pub(crate) fn patch_hosted_settings(
    request: HostedPatchRequest,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<HostedMutationResult, String> {
    require_hosted_window(&window, &["live", "settings"])?;
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
    window: tauri::WebviewWindow,
) -> Result<HostedMutationResult, String> {
    require_hosted_window(&window, &["live", "settings"])?;
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
    window: tauri::WebviewWindow,
) -> Result<HostedMutationResult, String> {
    require_hosted_window(&window, &["settings"])?;
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
    window: tauri::WebviewWindow,
) -> Result<HostedMutationResult, String> {
    require_hosted_window(&window, &["settings"])?;
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
pub(crate) fn purge_hosted_reusable_cache(
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<usize, String> {
    require_hosted_window(&window, &["settings"])?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::PurgeReusableCache { reply: reply_tx })
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
    window: tauri::WebviewWindow,
) -> Result<ProviderStateDto, String> {
    require_hosted_window(&window, &["settings"])?;
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
    request: PinnedQuestionUpdateRequest,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<HostedMutationResult, String> {
    require_hosted_window(&window, &["live", "settings"])?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::SetPinnedTemplate {
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

fn configured_chatgpt_auth(state: &HostedState) -> Result<ChatGptSubscriptionAuth, String> {
    state
        .chatgpt_auth
        .clone()
        .ok_or_else(|| "ChatGPT subscription authentication is unavailable".to_string())
}

#[tauri::command]
pub(crate) fn start_chatgpt_device_login(
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<ProviderStateDto, String> {
    require_hosted_window(&window, &["settings"])?;
    let auth = configured_chatgpt_auth(&state)?;
    let authorization = auth
        .start_device_login()
        .map_err(|error| error.to_string())?;
    let login_id = authorization.login_id().to_owned();
    let retained_scope = auth.connection_scope_id().ok();
    let provider_state = match state
        .handle
        .install_chatgpt_scope(retained_scope.clone(), false)
    {
        Ok(provider_state) => provider_state,
        Err(error) => {
            let _ = auth.cancel_device_login(&login_id);
            let _ = reconcile_chatgpt_after_login(&auth, &state.handle);
            return Err(sanitized_error(error));
        }
    };
    let worker_auth = auth.clone();
    let worker_handle = state.handle.clone();
    let spawn = std::thread::Builder::new()
        .name("corti-chatgpt-login".into())
        .spawn({
            let login_id = login_id.clone();
            move || drive_chatgpt_device_login(worker_auth, worker_handle, login_id)
        });
    if spawn.is_err() {
        let _ = auth.cancel_device_login(&login_id);
        let _ = reconcile_chatgpt_after_login(&auth, &state.handle);
        return Err("the ChatGPT authorization worker could not start".to_string());
    }
    Ok(provider_state)
}

fn reconcile_chatgpt_after_login(
    auth: &ChatGptSubscriptionAuth,
    handle: &HostedHandle,
) -> Result<ProviderStateDto, ErrorCode> {
    let scope = auth.connection_scope_id().ok();
    let refresh_catalog = matches!(auth.credential_state(), CredentialState::Ready { .. });
    handle.install_chatgpt_scope(scope, refresh_catalog)
}

fn drive_chatgpt_device_login(
    auth: ChatGptSubscriptionAuth,
    handle: HostedHandle,
    login_id: String,
) {
    loop {
        let interval = match auth.poll_interval(&login_id) {
            Ok(interval) => interval,
            Err(_) => return,
        };
        std::thread::sleep(interval);
        match auth.poll_device_login(&login_id) {
            Ok(ChatGptLoginPoll::Pending) => {
                let _ = handle.sync_chatgpt_credential();
            }
            Ok(ChatGptLoginPoll::Authorized { durable }) => {
                let result = auth
                    .connection_scope_id()
                    .map_err(|error| error.error_code())
                    .and_then(|scope| handle.install_chatgpt_scope(Some(scope), durable));
                if let Err(code) = result {
                    tracing::warn!(
                        target: "corti::hosted",
                        error = %code,
                        "ChatGPT authorization completed but its account catalog could not be installed"
                    );
                    let _ = handle.sync_chatgpt_credential();
                }
                return;
            }
            Ok(ChatGptLoginPoll::Denied | ChatGptLoginPoll::Expired) => {
                let _ = reconcile_chatgpt_after_login(&auth, &handle);
                return;
            }
            Err(
                ChatGptAuthError::Network | ChatGptAuthError::Timeout | ChatGptAuthError::Provider,
            ) => {
                // A browser flow should survive a dropped packet or transient 5xx. The auth object owns
                // the hard deadline and bounded backoff; this worker simply tries the next poll.
                let _ = handle.sync_chatgpt_credential();
            }
            Err(_) => {
                let _ = auth.cancel_device_login(&login_id);
                let _ = reconcile_chatgpt_after_login(&auth, &handle);
                return;
            }
        }
    }
}

#[tauri::command]
pub(crate) fn cancel_chatgpt_device_login(
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<ProviderStateDto, String> {
    require_hosted_window(&window, &["settings"])?;
    let auth = configured_chatgpt_auth(&state)?;
    let login_id = auth.current_login_id().map_err(|error| error.to_string())?;
    auth.cancel_device_login(&login_id)
        .map_err(|error| error.to_string())?;
    reconcile_chatgpt_after_login(&auth, &state.handle).map_err(sanitized_error)
}

#[tauri::command]
pub(crate) fn sign_out_chatgpt_subscription(
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<ProviderStateDto, String> {
    require_hosted_window(&window, &["settings"])?;
    let auth = configured_chatgpt_auth(&state)?;
    let retained_scope = auth.connection_scope_id().ok();
    state
        .handle
        .install_chatgpt_scope(retained_scope, false)
        .map_err(sanitized_error)?;
    if let Err(error) = auth.sign_out() {
        let _ = reconcile_chatgpt_after_login(&auth, &state.handle);
        return Err(error.to_string());
    }
    state
        .handle
        .install_chatgpt_scope(None, false)
        .map_err(sanitized_error)
}

#[tauri::command]
pub(crate) fn open_chatgpt_device_login(window: tauri::WebviewWindow) -> Result<(), String> {
    require_hosted_window(&window, &["settings"])?;
    std::process::Command::new("open")
        .arg(CHATGPT_DEVICE_VERIFICATION_URL)
        .spawn()
        .map(|_| ())
        .map_err(|_| "the ChatGPT authorization page could not be opened".to_string())
}

/// The `~/.aws` profile names the Bedrock pane offers. Secret presence deliberately lives only on the
/// settings document, so the pane has one source of truth for it rather than two that can disagree.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct AwsCredentialOptionsDto {
    pub(crate) profiles: Vec<String>,
}

/// Which stored AWS secret a command refers to. The webview can name a slot but never read or write one.
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AwsKeySlotDto {
    AccessKeyId,
    SecretAccessKey,
    SessionToken,
}

impl AwsKeySlotDto {
    const fn purpose(self) -> SecretPurpose {
        match self {
            Self::AccessKeyId => SecretPurpose::AwsAccessKeyId,
            Self::SecretAccessKey => SecretPurpose::AwsSecretAccessKey,
            Self::SessionToken => SecretPurpose::AwsSessionToken,
        }
    }

    const fn prompt(self) -> (&'static str, &'static str) {
        match self {
            Self::AccessKeyId => (
                "AWS access key ID",
                "Stored in Corti's private secret store and never shown to the Corti window.",
            ),
            Self::SecretAccessKey => (
                "AWS secret access key",
                "Stored in Corti's private secret store and never shown to the Corti window.",
            ),
            Self::SessionToken => (
                "AWS session token",
                "Optional. Only temporary credentials need a session token.",
            ),
        }
    }
}

/// Which secret a key-entry command targets. Direct providers hold one API key each; Bedrock's static
/// mode holds an AWS keypair.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub(crate) enum SecretSlotRequest {
    OpenAi,
    Anthropic,
    Aws { slot: AwsKeySlotDto },
}

impl SecretSlotRequest {
    const fn purpose(self) -> SecretPurpose {
        match self {
            Self::OpenAi => SecretPurpose::OpenAiApiKey,
            Self::Anthropic => SecretPurpose::AnthropicApiKey,
            Self::Aws { slot } => slot.purpose(),
        }
    }

    const fn prompt(self) -> (&'static str, &'static str) {
        match self {
            Self::OpenAi => (
                "OpenAI API key",
                "Stored in Corti's private secret store and never shown to the Corti window.",
            ),
            Self::Anthropic => (
                "Anthropic API key",
                "Stored in Corti's private secret store and never shown to the Corti window.",
            ),
            Self::Aws { slot } => slot.prompt(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SecretEntryResultDto {
    Stored,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SaveBedrockSetupRequest {
    pub(crate) observed_state_revision: u64,
    pub(crate) mode: AwsCredentialMode,
    pub(crate) profile: Option<String>,
    pub(crate) role_arn: Option<String>,
    pub(crate) region: String,
    pub(crate) setup_name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ClearBedrockSetupRequest {
    pub(crate) observed_state_revision: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BedrockCredentialModeRequest {
    pub(crate) observed_state_revision: u64,
    pub(crate) mode: AwsCredentialMode,
    pub(crate) profile: Option<String>,
    pub(crate) role_arn: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VertexModelsRequest {
    pub(crate) observed_state_revision: u64,
    pub(crate) models: Vec<String>,
}

/// Replace the operator-typed Vertex model list. Vertex has no catalog API to discover these from, so the
/// typed ids are the catalog; saving rebuilds the adapter on the next lookup.
#[tauri::command]
pub(crate) fn set_hosted_vertex_models(
    request: VertexModelsRequest,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<HostedMutationResult, String> {
    require_hosted_window(&window, &["settings"])?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::SetVertexModels {
            request,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

fn discovered_aws_profiles() -> Vec<String> {
    // Profile discovery belongs to the Transcribe backend's `aws` feature; a build without it can still
    // reach Bedrock through a key pair, so offer an empty profile list rather than failing the command.
    #[cfg(feature = "aws")]
    {
        crate::settings::list_aws_profiles()
    }
    #[cfg(not(feature = "aws"))]
    {
        Vec::new()
    }
}

#[tauri::command]
pub(crate) fn list_aws_credential_options(
    window: tauri::WebviewWindow,
) -> Result<AwsCredentialOptionsDto, String> {
    require_hosted_window(&window, &["settings"])?;
    Ok(AwsCredentialOptionsDto {
        profiles: discovered_aws_profiles(),
    })
}

#[tauri::command]
pub(crate) fn save_bedrock_setup(
    request: SaveBedrockSetupRequest,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<HostedMutationResult, String> {
    require_hosted_window(&window, &["settings"])?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::SaveBedrockSetup {
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
pub(crate) fn clear_bedrock_setup(
    request: ClearBedrockSetupRequest,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<HostedMutationResult, String> {
    require_hosted_window(&window, &["settings"])?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::ClearBedrockSetup {
            request,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

/// Compatibility command retained for one release. It reuses the persisted region and setup name and is
/// rejected when that legacy document is incomplete; it can no longer persist only the authentication half.
#[tauri::command]
pub(crate) fn set_bedrock_credential_mode(
    request: BedrockCredentialModeRequest,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<HostedMutationResult, String> {
    require_hosted_window(&window, &["settings"])?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    state
        .handle
        .send(ServiceCommand::SetBedrockCredentialMode {
            request,
            reply: reply_tx,
        })
        .map_err(sanitized_error)?;
    reply_rx
        .recv()
        .map_err(|_| "hosted coordinator stopped".to_string())?
        .map_err(sanitized_error)
}

/// Open the native secure-entry sheet and store the typed value. The secret never crosses IPC in either
/// direction; the reply says only what happened.
#[tauri::command]
pub(crate) fn prompt_for_provider_secret(
    request: SecretSlotRequest,
    app: AppHandle,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<SecretEntryResultDto, String> {
    require_hosted_window(&window, &["settings"])?;
    let (title, detail) = request.prompt();
    match crate::secure_entry::prompt_and_store(&app, request.purpose(), title, detail) {
        Ok(crate::secure_entry::SecureEntryOutcome::Stored) => {
            state
                .handle
                .notify_provider_secret_changed(request)
                .map_err(sanitized_error)?;
            Ok(SecretEntryResultDto::Stored)
        }
        Ok(crate::secure_entry::SecureEntryOutcome::Cancelled) => {
            Ok(SecretEntryResultDto::Cancelled)
        }
        Ok(crate::secure_entry::SecureEntryOutcome::Rejected) => Ok(SecretEntryResultDto::Rejected),
        // The anyhow chain can name a secret-store failure but never the value.
        Err(_) => Err("the secure-entry sheet could not store the value".to_string()),
    }
}

#[tauri::command]
pub(crate) fn clear_provider_secret(
    request: SecretSlotRequest,
    state: State<'_, HostedState>,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    require_hosted_window(&window, &["settings"])?;
    crate::secret_store::delete(request.purpose())
        .map_err(|_| "the stored value could not be removed".to_string())?;
    state
        .handle
        .notify_provider_secret_changed(request)
        .map_err(sanitized_error)
}

fn require_live_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    require_hosted_window(window, &["live"])
}

fn require_hosted_window(
    window: &tauri::WebviewWindow,
    allowed_labels: &[&str],
) -> Result<(), String> {
    if hosted_window_allowed(window.label(), allowed_labels) {
        Ok(())
    } else {
        Err("hosted command is unavailable from this window".to_string())
    }
}

fn hosted_window_allowed(label: &str, allowed_labels: &[&str]) -> bool {
    allowed_labels.contains(&label)
}

fn sanitized_error(code: ErrorCode) -> String {
    code.to_string()
}

/// Start production wiring. Paid egress remains deny-by-default and is enabled only by the exact explicit
/// persisted disclosure/lane controls. The approved factory supports direct OpenAI/Anthropic API keys,
/// native ChatGPT subscription device auth, Vertex, and Bedrock; Claude subscriptions remain blocked. Tests
/// inject transports and secrets and never use ambient credentials.
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
    let preferences = Arc::new(Mutex::new(preferences));
    let word_bank = crate::word_bank::load().unwrap_or_else(|error| {
        tracing::warn!(
            target: "corti::hosted",
            error = %format!("{error:#}"),
            "word bank is unreadable; hosted egress remains safely unavailable for this run"
        );
        WordBankDocument::empty()
    });
    let outbox = Arc::new(TelemetryOutbox::open(default_outbox_path()?)?);
    let durable = load_or_create_master_keys().and_then(|keys| {
        let path = default_store_path()?;
        if keys.created {
            discard_store_sealed_under_lost_key(&path)?;
        }
        let store = RuntimeStore::open_encrypted(
            path,
            keys.encryption,
            outbox.clone(),
            pipeline_tx.clone(),
        )?;
        Ok((
            keys.digest,
            Box::new(store) as Box<dyn EncryptedPostprocessStore>,
        ))
    });
    let (digest_key, store, durable_store_armed) = match durable {
        Ok((digest_key, store)) => (digest_key, store, true),
        Err(_) => {
            // An unreadable secret store or authenticated store must not brick raw capture, but it
            // must make paid egress impossible for this process. Never replace or bypass the suspect store;
            // the one exception is `discard_store_sealed_under_lost_key`, which runs only when no key exists.
            tracing::warn!(
                target: "corti::hosted",
                "durable hosted store is unavailable; hosted egress remains off"
            );
            let mut digest = [0u8; 32];
            random_bytes(&mut digest)?;
            (
                DigestKey::new(digest),
                Box::new(RuntimeStore::memory(outbox.clone(), pipeline_tx.clone()))
                    as Box<dyn EncryptedPostprocessStore>,
                false,
            )
        }
    };
    let notifier: EventNotifier = Arc::new(move |event| {
        let _ = app.emit_to("live", HOSTED_STATE_CHANGED_EVENT, event);
        let _ = app.emit_to("settings", HOSTED_STATE_CHANGED_EVENT, event);
    });
    let chatgpt_auth = ChatGptSubscriptionAuth::new(
        Box::new(UreqTransport::new()),
        Arc::new(ProviderSystemClock::new()),
        Arc::new(FileChatGptStore),
    );
    let approval = durable_store_armed.then_some(ProductionApproval::for_durable_store());
    let vertex = VertexAdcResolver::production(vertex_config_source(preferences.clone()));
    let (executor, providers) = approval.map_or_else(
        || {
            (
                Arc::new(DenyExecutor) as Arc<dyn TicketExecutor>,
                Box::new(UnavailableProviders) as Box<dyn ProviderAccess>,
            )
        },
        |approval| {
            approved_direct_components(
                approval,
                Arc::new(ProductionTransportFactory),
                Arc::new(FileDirectSecretStore),
                chatgpt_auth.clone(),
                BedrockCredentialResolver::new(bedrock_config_source(preferences.clone())),
                vertex.clone(),
                vertex_models_source(preferences.clone()),
            )
        },
    );
    let process_epoch = live_view.process_epoch();
    let (mut state, handle) = start_with_components(
        preferences,
        word_bank,
        live_view,
        pipeline_tx,
        outbox,
        executor,
        providers,
        Arc::new(NoPricing),
        Arc::new(AdcVertexResolver(vertex)),
        notifier,
        digest_key,
        process_epoch,
        true,
        Some(store),
        None,
        Some(Arc::new(crate::secret_store::is_present)),
    )?;
    state.chatgpt_auth = Some(chatgpt_auth);
    Ok((state, handle))
}

type EventNotifier = Arc<dyn Fn(&CoordinatorEventDto) + Send + Sync>;
type SecretPresenceSource = Arc<dyn Fn(SecretPurpose) -> bool + Send + Sync>;

#[allow(clippy::too_many_arguments)]
fn start_with_components(
    preferences: Arc<Mutex<HostedPreferences>>,
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
    store_override: Option<Box<dyn EncryptedPostprocessStore>>,
    clock_override: Option<Arc<dyn CoordinatorClock>>,
    secret_presence_override: Option<SecretPresenceSource>,
) -> Result<(HostedState, HostedHandle)> {
    // Tests default to a hermetic empty projection; production explicitly supplies the private-store source.
    let secret_presence = secret_presence_override.unwrap_or_else(|| Arc::new(|_| false));
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
        false,
        secret_presence.as_ref(),
    );
    let observed_pinned_revision = initial_control.pinned_question_revision;
    let snapshot = Arc::new(Mutex::new(initial_settings));
    let persistence = Box::new(HostedControlPersistence {
        preferences: preferences.clone(),
        persist_to_disk,
    });
    let store = store_override.unwrap_or_else(|| {
        Box::new(RuntimeStore::memory(outbox.clone(), pipeline_tx.clone()))
            as Box<dyn EncryptedPostprocessStore>
    });
    let clock: Arc<dyn CoordinatorClock> =
        clock_override.unwrap_or_else(|| Arc::new(SystemCoordinatorClock::new()));
    let pinned = preferences
        .lock()
        .unwrap()
        .values()
        .pinned_question_template
        .clone();
    let digest_key = Arc::new(digest_key);
    let coordinator = PostprocessCoordinator::new_with_snapshot(
        initial_control,
        (!pinned.trim().is_empty()).then_some(pinned),
        clock.clone(),
        digest_key.clone(),
        persistence,
        store,
        providers,
        pricing,
    );
    let provider_access = coordinator.provider_access();
    let startup_providers = coordinator.provider_states().cloned().collect::<Vec<_>>();
    *snapshot.lock().unwrap() = settings_snapshot(
        1,
        &preferences.lock().unwrap(),
        &word_bank,
        coordinator.control_snapshot(),
        &startup_providers,
        chatgpt_scope_configured(&coordinator),
        secret_presence.as_ref(),
    );
    let (ingress, ingress_rx) = CoordinatorIngress::standard();
    let (command_tx, command_rx) = sync_channel(SERVICE_COMMAND_CAPACITY);
    let (priority_tx, priority_rx) = std::sync::mpsc::channel();
    let (worker_tx, worker_rx) = std::sync::mpsc::channel();
    let (provider_prepare_tx, provider_prepare_rx) = std::sync::mpsc::channel();
    let (vertex_tx, vertex_rx) = std::sync::mpsc::channel();
    let (event_sink, provider_event_rx) = crate::postprocess::BoundedProviderEventSink::channel();
    let ingress_incomplete = Arc::new(AtomicBool::new(false));
    let handle = HostedHandle {
        command_tx,
        priority_tx,
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
        digest_key,
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
        provider_access,
        vertex_resolver,
        worker_tx,
        worker_rx,
        provider_prepare_tx,
        provider_prepare_rx,
        pending_provider_prepares: 0,
        provider_prepares: HashMap::new(),
        provider_prepare_backlog: VecDeque::new(),
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
        secret_presence,
    };
    std::thread::Builder::new()
        .name("corti-hosted-control".into())
        .spawn(move || service.run(command_rx, priority_rx, ingress_rx))
        .context("spawning hosted coordinator")?;
    Ok((
        HostedState {
            handle: handle.clone(),
            chatgpt_auth: None,
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

struct MasterKeys {
    digest: DigestKey,
    encryption: [u8; 32],
    /// The master key was generated on this launch; see `discard_store_sealed_under_lost_key`.
    created: bool,
}

fn load_or_create_master_keys() -> Result<MasterKeys> {
    let master = crate::secret_store::load_or_create_master_key(random_bytes)?;
    let digest = derive_master_subkey(master.bytes.as_slice(), b"corti-hosted-digest-v1");
    let encryption = derive_master_subkey(master.bytes.as_slice(), b"corti-hosted-encryption-v1");
    Ok(MasterKeys {
        digest: DigestKey::new(digest),
        encryption,
        created: master.created,
    })
}

/// A freshly generated master key means no key exists anywhere for ciphertext already on disk: the first
/// launch after secrets moved out of the Keychain (#145), or a deliberately deleted key, which is design
/// 07 §9.1's "rotate encryption key" semantics. Such a store can never be opened again, so it is discarded
/// rather than left to fail authentication on every launch. A store that fails to open under a key that
/// *did* exist is never touched.
fn discard_store_sealed_under_lost_key(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            tracing::warn!(
                target: "corti::hosted",
                path = %path.display(),
                "hosted master key was regenerated; discarded the encrypted hosted store sealed under the previous key"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).context("discarding the encrypted hosted store sealed under a lost key")
        }
    }
}

fn derive_master_subkey(master: &[u8], domain: &[u8]) -> [u8; 32] {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    context.update(domain);
    context.update(&[0]);
    context.update(master);
    context
        .finish()
        .as_ref()
        .try_into()
        .expect("SHA-256 is 32 bytes")
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
        // No recording exists at startup. The first LiveTranscriptStore begin and coordinator begin_session
        // both advance to generation 1 under the shared process epoch.
        session_generation: 0,
        control_revision: revision,
        steering_revision: revision,
        bank_revision,
        pinned_question_revision: (!values.pinned_question_template.trim().is_empty()) as u64,
        master_enabled: values.master_enabled,
        egress_acknowledged: values.egress_acknowledgement_version
            == Some(EGRESS_DISCLOSURE_VERSION),
        pinned_auto_enabled: values.pinned_auto_enabled,
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
            })
            .map_err(|_| ErrorCode::Cache)?;
        if self.persist_to_disk {
            next.save().map_err(|_| ErrorCode::Cache)?;
        }
        *self.preferences.lock().unwrap() = next;
        Ok(())
    }
}

/// A live view of Bedrock's non-secret connection facts, so a mode or profile change in Settings takes
/// effect on the next resolution without restarting the app.
fn bedrock_config_source(
    preferences: Arc<Mutex<HostedPreferences>>,
) -> crate::bedrock_creds::ConfigSource {
    Arc::new(move || {
        let preferences = preferences.lock().unwrap();
        let bedrock = &preferences.values().providers.bedrock;
        BedrockCredentialConfig {
            mode: bedrock.credential_mode,
            profile: bedrock.profile.clone(),
            region: bedrock.scope.region.clone(),
            role_arn: bedrock.role_arn.clone(),
        }
    })
}

/// A live view of Vertex's non-secret connection scope, so a project/region change in Settings routes the
/// next resolution without restarting the app. ADC itself is discovered from disk, not from these fields.
fn vertex_config_source(
    preferences: Arc<Mutex<HostedPreferences>>,
) -> crate::vertex_creds::ConfigSource {
    Arc::new(move || {
        let preferences = preferences.lock().unwrap();
        let vertex = &preferences.values().providers.vertex;
        VertexConnectionConfig {
            project: vertex.project.clone(),
            region: vertex.region.clone(),
            quota_project: vertex.quota_project.clone(),
        }
    })
}

fn vertex_models_source(
    preferences: Arc<Mutex<HostedPreferences>>,
) -> Arc<dyn Fn() -> Vec<String> + Send + Sync> {
    Arc::new(move || {
        preferences
            .lock()
            .unwrap()
            .values()
            .providers
            .vertex_models
            .clone()
    })
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
        &self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> Option<ProviderDescriptor> {
        crate::postprocess::provider_support_catalog()
            .into_iter()
            .find(|candidate| &candidate.provider == provider && &candidate.transport == transport)
    }

    fn credential_state(&self, provider: &ProviderId, transport: &TransportId) -> CredentialState {
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
        &self,
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

struct ProductionApproval(());

impl ProductionApproval {
    const fn for_durable_store() -> Self {
        Self(())
    }

    #[cfg(test)]
    fn for_test() -> Self {
        Self(())
    }
}

trait DirectTransportFactory: Send + Sync {
    fn openai(&self) -> Box<dyn HttpTransport>;
    fn chatgpt(&self) -> Box<dyn HttpTransport>;
    fn anthropic(&self) -> Box<dyn HttpTransport>;
    fn bedrock(&self) -> Box<dyn HttpTransport>;
    fn vertex(&self) -> Box<dyn HttpTransport>;
}

struct ProductionTransportFactory;

impl DirectTransportFactory for ProductionTransportFactory {
    fn openai(&self) -> Box<dyn HttpTransport> {
        Box::new(UreqTransport::new())
    }

    fn chatgpt(&self) -> Box<dyn HttpTransport> {
        Box::new(UreqTransport::new())
    }

    fn anthropic(&self) -> Box<dyn HttpTransport> {
        Box::new(UreqTransport::new())
    }

    fn bedrock(&self) -> Box<dyn HttpTransport> {
        Box::new(UreqTransport::new())
    }

    fn vertex(&self) -> Box<dyn HttpTransport> {
        Box::new(UreqTransport::new())
    }
}

trait DirectSecretStore: Send + Sync {
    fn read(&self, account: &str) -> Result<Option<Vec<u8>>, CredentialError>;
}

struct FileDirectSecretStore;

struct FileChatGptStore;

impl ChatGptCredentialStore for FileChatGptStore {
    fn load(&self) -> Result<Option<Vec<u8>>, ChatGptStoreError> {
        crate::secret_store::read(SecretPurpose::ChatGptSubscriptionCredential)
            .map_err(|_| ChatGptStoreError::Unavailable)
    }

    fn save(&self, document: &[u8]) -> Result<(), ChatGptStoreError> {
        crate::secret_store::write(SecretPurpose::ChatGptSubscriptionCredential, document)
            .map_err(|_| ChatGptStoreError::Unavailable)
    }

    fn clear(&self) -> Result<(), ChatGptStoreError> {
        crate::secret_store::delete(SecretPurpose::ChatGptSubscriptionCredential)
            .map_err(|_| ChatGptStoreError::Unavailable)
    }
}

impl DirectSecretStore for FileDirectSecretStore {
    fn read(&self, account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
        let purpose = match account {
            OPENAI_API_KEY_ACCOUNT => SecretPurpose::OpenAiApiKey,
            ANTHROPIC_API_KEY_ACCOUNT => SecretPurpose::AnthropicApiKey,
            _ => return Err(CredentialError::Unavailable),
        };
        crate::secret_store::read(purpose).map_err(|_| CredentialError::Unavailable)
    }
}

struct DirectCredential {
    secrets: Arc<dyn DirectSecretStore>,
    account: &'static str,
    rejected: AtomicBool,
    generation: std::sync::atomic::AtomicU64,
}

impl DirectCredential {
    fn new(secrets: Arc<dyn DirectSecretStore>, account: &'static str) -> Arc<Self> {
        Arc::new(Self {
            secrets,
            account,
            rejected: AtomicBool::new(false),
            generation: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn state(&self) -> CredentialState {
        use zeroize::Zeroize as _;

        if self.rejected.load(Ordering::Acquire) {
            return CredentialState::Rejected;
        }
        match self.secrets.read(self.account) {
            Ok(Some(mut bytes)) => {
                let ready = !bytes.is_empty();
                bytes.zeroize();
                if ready {
                    CredentialState::Ready {
                        expires_at_unix_ms: None,
                        source: corti_postprocess::CredentialSourceKind::Keychain,
                    }
                } else {
                    CredentialState::Absent
                }
            }
            Ok(None) => CredentialState::Absent,
            Err(_) => CredentialState::Error {
                code: ErrorCode::AuthUnarmed,
            },
        }
    }

    fn invalidate(&self) {
        self.rejected.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn cache_identity(&self) -> Option<[u8; 32]> {
        use zeroize::Zeroize as _;

        let mut bytes = self.secrets.read(self.account).ok().flatten()?;
        let identity = ring::digest::digest(&ring::digest::SHA256, &bytes)
            .as_ref()
            .try_into()
            .ok();
        bytes.zeroize();
        identity
    }

    fn api_key(&self) -> Result<ApiKey, CredentialError> {
        use zeroize::Zeroize as _;

        if self.rejected.load(Ordering::Acquire) {
            return Err(CredentialError::Rejected);
        }
        let bytes = self
            .secrets
            .read(self.account)?
            .ok_or(CredentialError::Absent)?;
        let value = String::from_utf8(bytes).map_err(|error| {
            let mut bytes = error.into_bytes();
            bytes.zeroize();
            CredentialError::Unavailable
        })?;
        ApiKey::new(value).map_err(|_| CredentialError::Unavailable)
    }
}

struct DirectApiKeySource(Arc<DirectCredential>);

impl ApiKeySource for DirectApiKeySource {
    fn resolve(&mut self) -> Result<ApiKey, CredentialError> {
        self.0.api_key()
    }

    fn mark_rejected(&mut self) {
        self.0.rejected.store(true, Ordering::Release);
    }
}

type SharedAdapter = Arc<Mutex<Box<dyn ProviderAdapter>>>;

#[derive(Clone, PartialEq, Eq, Hash)]
struct CatalogCacheKey {
    provider: ProviderId,
    transport: TransportId,
    connection_scope_id: ConnectionScopeId,
    region: Option<String>,
    credential_generation: u64,
}

struct ApprovedProviderDirectory {
    adapters: HashMap<(ProviderId, TransportId), SharedAdapter>,
    credentials: HashMap<(ProviderId, TransportId), Arc<DirectCredential>>,
    catalogs: Mutex<HashMap<CatalogCacheKey, ModelCatalog>>,
    chatgpt: ChatGptSubscriptionAuth,
    /// Bedrock resolves through the AWS chain rather than a single stored secret, so its readiness —
    /// including assumed-role and SSO expiry — comes from its own resolver.
    bedrock: Arc<BedrockCredentialResolver>,
    vertex: VertexAdapterSlot,
}

impl ApprovedProviderDirectory {
    fn adapter(&self, provider: &ProviderId, transport: &TransportId) -> Option<SharedAdapter> {
        if (provider.as_str(), transport.as_str()) == (VERTEX_PROVIDER, VERTEX_TRANSPORT) {
            return self.vertex.current();
        }
        self.adapters
            .get(&(provider.clone(), transport.clone()))
            .cloned()
    }

    fn credential_generation(&self, provider: &ProviderId, transport: &TransportId) -> u64 {
        self.credentials
            .get(&(provider.clone(), transport.clone()))
            .map_or_else(
                || {
                    if (provider.as_str(), transport.as_str()) == ("amazon", "bedrock_runtime") {
                        self.bedrock.generation()
                    } else {
                        0
                    }
                },
                |credential| credential.generation(),
            )
    }

    fn catalog_key(
        &self,
        provider: &ProviderId,
        transport: &TransportId,
        scope: &ProviderScope,
    ) -> CatalogCacheKey {
        CatalogCacheKey {
            provider: provider.clone(),
            transport: transport.clone(),
            connection_scope_id: scope.connection_scope_id.clone(),
            region: scope.region.clone(),
            credential_generation: self.credential_generation(provider, transport),
        }
    }

    fn catalog(
        &self,
        provider: &ProviderId,
        transport: &TransportId,
        scope: &ProviderScope,
        force_refresh: bool,
    ) -> Result<ModelCatalog, corti_postprocess::PostprocessError> {
        let key = self.catalog_key(provider, transport, scope);
        if !force_refresh && let Some(catalog) = self.catalogs.lock().unwrap().get(&key).cloned() {
            return Ok(catalog);
        }
        let adapter = self
            .adapter(provider, transport)
            .ok_or_else(|| corti_postprocess::PostprocessError::from(ErrorCode::AuthUnarmed))?;
        let mut adapter = adapter
            .lock()
            .map_err(|_| corti_postprocess::PostprocessError::from(ErrorCode::Internal))?;
        // A peer may have populated the generation while this caller waited on the per-adapter lock.
        if !force_refresh && let Some(catalog) = self.catalogs.lock().unwrap().get(&key).cloned() {
            return Ok(catalog);
        }
        let catalog = adapter.catalog(scope)?;
        let mut catalogs = self.catalogs.lock().unwrap();
        catalogs.retain(|candidate, _| {
            candidate.provider != *provider
                || candidate.transport != *transport
                || *candidate == key
        });
        catalogs.insert(key, catalog.clone());
        Ok(catalog)
    }

    fn invalidate_catalog(&self, provider: &ProviderId, transport: &TransportId) {
        self.catalogs
            .lock()
            .unwrap()
            .retain(|key, _| key.provider != *provider || key.transport != *transport);
    }
}

/// Vertex is the one transport whose adapter bakes project routing and its model list in at construction.
/// Rebuilding it whenever those inputs change is what lets a scope or model edit take effect without a
/// relaunch — the previous build-once-at-startup wiring left a first-run configuration permanently
/// catalog-less, and a later region edit permanently policy-blocked.
struct VertexAdapterSlot {
    transports: Arc<dyn DirectTransportFactory>,
    resolver: Arc<VertexAdcResolver>,
    models: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
    built: Mutex<Option<(VertexAdapterInputs, SharedAdapter)>>,
}

#[derive(Clone, PartialEq, Eq)]
struct VertexAdapterInputs {
    config: VertexConnectionConfig,
    models: Vec<String>,
}

impl VertexAdapterSlot {
    fn current(&self) -> Option<SharedAdapter> {
        let desired = VertexAdapterInputs {
            config: self.resolver.config(),
            models: (self.models)(),
        };
        let mut built = self.built.lock().ok()?;
        if built.as_ref().map(|(inputs, _)| inputs) != Some(&desired) {
            *built = vertex_adapter(
                self.transports.as_ref(),
                self.resolver.clone(),
                &desired.models,
            )
            .map(|adapter| (desired, Arc::new(Mutex::new(adapter))));
        }
        built.as_ref().map(|(_, adapter)| adapter.clone())
    }
}

struct ApprovedProviderAccess(Arc<ApprovedProviderDirectory>);

impl ProviderAccess for ApprovedProviderAccess {
    fn connection_scope(
        &self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> Option<ProviderScope> {
        if (provider.as_str(), transport.as_str()) != (CHATGPT_PROVIDER, CHATGPT_TRANSPORT) {
            return None;
        }
        self.0
            .chatgpt
            .connection_scope_id()
            .ok()
            .map(|connection_scope_id| ProviderScope {
                connection_scope_id,
                region: None,
            })
    }

    fn descriptor(
        &self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> Option<ProviderDescriptor> {
        known_descriptor(provider, transport)
    }

    fn credential_state(&self, provider: &ProviderId, transport: &TransportId) -> CredentialState {
        let key = (provider.clone(), transport.clone());
        if let Some(credential) = self.0.credentials.get(&key) {
            return credential.state();
        }
        if (provider.as_str(), transport.as_str()) == (CHATGPT_PROVIDER, CHATGPT_TRANSPORT) {
            return self.0.chatgpt.credential_state();
        }
        if (provider.as_str(), transport.as_str()) == ("amazon", "bedrock_runtime") {
            return self.0.bedrock.state();
        }
        match known_descriptor(provider, transport).map(|descriptor| descriptor.support_tier) {
            Some(SupportTier::Blocked | SupportTier::Experimental) | None => {
                CredentialState::Unsupported {
                    code: ErrorCode::PolicyBlocked,
                }
            }
            Some(SupportTier::Documented) => CredentialState::Absent,
        }
    }

    fn resolve_credential(
        &self,
        provider: &ProviderId,
        transport: &TransportId,
    ) -> CredentialState {
        if (provider.as_str(), transport.as_str()) == ("amazon", "bedrock_runtime") {
            self.0.bedrock.resolve_state()
        } else {
            self.credential_state(provider, transport)
        }
    }

    fn credential_cache_identity(
        &self,
        provider: &ProviderId,
        transport: &TransportId,
        scope: &ProviderScope,
    ) -> Option<[u8; 32]> {
        if let Some(credential) = self
            .0
            .credentials
            .get(&(provider.clone(), transport.clone()))
        {
            return credential.cache_identity();
        }
        if (provider.as_str(), transport.as_str()) == ("amazon", "bedrock_runtime") {
            return self.0.bedrock.cache_identity();
        }
        // ChatGPT's dynamic scope is derived from its account id. Vertex's project scope remains the
        // conservative identity until its auth manager exposes a stable credential lease fingerprint.
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        digest.update(provider.as_str().as_bytes());
        digest.update(&[0]);
        digest.update(transport.as_str().as_bytes());
        digest.update(&[0]);
        digest.update(scope.connection_scope_id.as_str().as_bytes());
        digest.finish().as_ref().try_into().ok()
    }

    fn invalidate_credentials(&self, provider: &ProviderId, transport: &TransportId) {
        self.0.invalidate_catalog(provider, transport);
        let key = (provider.clone(), transport.clone());
        if let Some(credential) = self.0.credentials.get(&key) {
            credential.invalidate();
        }
        if (provider.as_str(), transport.as_str()) == ("amazon", "bedrock_runtime") {
            self.0.bedrock.invalidate();
        }
    }

    fn catalog(
        &self,
        provider: &ProviderId,
        transport: &TransportId,
        scope: &ProviderScope,
    ) -> Result<ModelCatalog, corti_postprocess::PostprocessError> {
        self.0.catalog(provider, transport, scope, false)
    }

    fn refresh_catalog(
        &self,
        provider: &ProviderId,
        transport: &TransportId,
        scope: &ProviderScope,
    ) -> Result<ModelCatalog, corti_postprocess::PostprocessError> {
        self.0.catalog(provider, transport, scope, true)
    }

    fn invalidate_catalog(&self, provider: &ProviderId, transport: &TransportId) {
        self.0.invalidate_catalog(provider, transport);
    }
}

struct ApprovedTicketExecutor(Arc<ApprovedProviderDirectory>);

impl TicketExecutor for ApprovedTicketExecutor {
    fn execute(
        &self,
        ticket: &DispatchTicket,
        sink: &dyn ProviderEventSink,
    ) -> Result<ProviderTerminal, corti_postprocess::PostprocessError> {
        let request = ticket.request();
        let adapter = self
            .0
            .adapter(&request.provider, &request.transport)
            .ok_or_else(|| corti_postprocess::PostprocessError::from(ErrorCode::PolicyBlocked))?;
        ticket.execute_with(
            adapter
                .lock()
                .map_err(|_| corti_postprocess::PostprocessError::from(ErrorCode::Internal))?
                .as_mut(),
            sink,
        )
    }
}

fn approved_direct_components(
    _approval: ProductionApproval,
    transports: Arc<dyn DirectTransportFactory>,
    secrets: Arc<dyn DirectSecretStore>,
    chatgpt_auth: ChatGptSubscriptionAuth,
    bedrock: Arc<BedrockCredentialResolver>,
    vertex: Arc<VertexAdcResolver>,
    vertex_models: Arc<dyn Fn() -> Vec<String> + Send + Sync>,
) -> (Arc<dyn TicketExecutor>, Box<dyn ProviderAccess>) {
    let openai_credential = DirectCredential::new(secrets.clone(), OPENAI_API_KEY_ACCOUNT);
    let anthropic_credential = DirectCredential::new(secrets, ANTHROPIC_API_KEY_ACCOUNT);
    let openai: Box<dyn ProviderAdapter> = Box::new(OpenAiResponsesAdapter::new(
        transports.openai(),
        Box::new(ProviderSystemClock::new()),
        Box::new(DirectApiKeySource(openai_credential.clone())),
    ));
    let chatgpt: Box<dyn ProviderAdapter> = Box::new(ChatGptSubscriptionAdapter::new(
        transports.chatgpt(),
        Box::new(ProviderSystemClock::new()),
        chatgpt_auth.clone(),
    ));
    let anthropic: Box<dyn ProviderAdapter> = Box::new(AnthropicMessagesAdapter::new(
        transports.anthropic(),
        Box::new(ProviderSystemClock::new()),
        Box::new(DirectApiKeySource(anthropic_credential.clone())),
    ));
    let bedrock_adapter: Box<dyn ProviderAdapter> = Box::new(BedrockConverseAdapter::new(
        transports.bedrock(),
        Box::new(ProviderSystemClock::new()),
        Box::new(ProviderSystemClock::new()),
        Box::new(BedrockAdapterCredentials::new(bedrock.clone())),
    ));
    let mut adapters = HashMap::new();
    let mut credentials = HashMap::new();
    for (adapter, credential) in [
        (openai, openai_credential),
        (anthropic, anthropic_credential),
    ] {
        let descriptor = adapter.descriptor();
        let key = (descriptor.provider, descriptor.transport);
        adapters.insert(key.clone(), Arc::new(Mutex::new(adapter)));
        credentials.insert(key, credential);
    }
    let chatgpt_descriptor = chatgpt.descriptor();
    adapters.insert(
        (chatgpt_descriptor.provider, chatgpt_descriptor.transport),
        Arc::new(Mutex::new(chatgpt)),
    );
    let bedrock_descriptor = bedrock_adapter.descriptor();
    adapters.insert(
        (bedrock_descriptor.provider, bedrock_descriptor.transport),
        Arc::new(Mutex::new(bedrock_adapter)),
    );
    let directory = Arc::new(ApprovedProviderDirectory {
        adapters,
        credentials,
        catalogs: Mutex::new(HashMap::new()),
        chatgpt: chatgpt_auth,
        bedrock,
        vertex: VertexAdapterSlot {
            transports,
            resolver: vertex,
            models: vertex_models,
            built: Mutex::new(None),
        },
    });
    (
        Arc::new(ApprovedTicketExecutor(directory.clone())),
        Box::new(ApprovedProviderAccess(directory)),
    )
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

#[cfg(test)]
struct UnarmedVertex;

#[cfg(test)]
impl VertexResolver for UnarmedVertex {
    fn resolve(&self, _attempt: &VertexResolutionAttempt) -> VertexResolutionOutcome {
        VertexResolutionOutcome::Unarmed
    }
}

/// Drives the credential-arming state machine off the real ADC resolver. The resolution runs on the
/// `corti-hosted-auth` thread, so its blocking file read and token exchange never touch the pipeline.
struct AdcVertexResolver(Arc<VertexAdcResolver>);

impl VertexResolver for AdcVertexResolver {
    fn resolve(&self, _attempt: &VertexResolutionAttempt) -> VertexResolutionOutcome {
        self.0.resolve_outcome()
    }
}

/// Builds the Vertex adapter when the connection scope names a valid project/region. Returns `None` (rather
/// than a misconfigured adapter) when routing is absent, so resolution can still arm while the user finishes
/// configuring the scope.
fn vertex_adapter(
    transports: &dyn DirectTransportFactory,
    vertex: Arc<VertexAdcResolver>,
    typed_models: &[String],
) -> Option<Box<dyn ProviderAdapter>> {
    let config = vertex.config();
    let metadata =
        VertexProjectMetadata::new(config.project?, config.region?, config.quota_project).ok()?;
    let adapter = VertexRestAdapter::new(
        transports.vertex(),
        Box::new(ProviderSystemClock::new()),
        Box::new(VertexAdapterCredentials::new(vertex)),
        metadata,
        vertex_direct_models(typed_models),
    )
    .ok()?;
    Some(Box::new(adapter))
}

/// The curated models plus whatever exact ids the operator typed in Settings. Vertex exposes no
/// per-project listing of the models a caller may invoke — the publisher list is the whole Model Garden and
/// needs a quota project the caller may not hold — so typing the id is the only discovery Corti can offer.
/// Limits come from the id: the adapter only uses them to refuse an output budget larger than the model
/// allows, and Vertex rejects a genuinely wrong id on the first call with its own error.
fn vertex_direct_models(typed: &[String]) -> Vec<VertexModel> {
    const CURATED: [&str; 3] = ["gemini-2.5-flash", "gemini-2.5-pro", "claude-sonnet-4-5"];
    let mut seen = HashSet::new();
    CURATED
        .into_iter()
        .map(ToOwned::to_owned)
        .chain(typed.iter().cloned())
        // The adapter rejects the whole catalog on a duplicate, so a typed id that repeats a curated one
        // must collapse rather than disarm every model.
        .filter(|id| seen.insert(id.clone()))
        .filter_map(|id| VertexModel::inferred(ModelId::new(id).ok()?).ok())
        .collect()
}

struct NoPricing;

impl PricingCatalog for NoPricing {
    fn estimate(
        &self,
        query: PricingQuery<'_>,
        _usage: &corti_postprocess::NormalizedUsage,
    ) -> Result<CostEstimate, PricingError> {
        Ok(match query.billing_basis {
            BillingBasis::IncludedSubscription => CostEstimate::included_subscription(),
            BillingBasis::NoProviderRequest => CostEstimate::no_provider_request(),
            BillingBasis::MeteredEstimate | BillingBasis::Unknown => CostEstimate::unavailable(),
        })
    }
}

struct RuntimeStore {
    outbox: Arc<TelemetryOutbox>,
    pipeline_tx: Sender<PipelineMsg>,
    state: RuntimeStoreState,
    /// Process-only exact results for `MemoryOnly` and best-effort fallback when optional reusable-cache
    /// persistence fails after a paid non-Final result has already validated.
    memory_cache: HashMap<String, MemoryCacheEntry>,
    persistence: RuntimeStorePersistence,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeStoreState {
    journals: Vec<StoredFinalJournal>,
    exact_cache: HashMap<String, StoredCacheEntry>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeStoreDocument {
    schema: u32,
    state: RuntimeStoreState,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredFinalJournal {
    recording_id: String,
    request_group_id: RequestGroupId,
    call_id: CallId,
    request_key: String,
    fence: RequestFence,
    state: FinalJournalState,
    output: Option<StoredProviderOutput>,
    /// Capacity reserved before egress so a validated paid Final cannot be rejected only because optional
    /// reusable entries consumed the encrypted store while the provider was running.
    #[serde(default)]
    reserved_output_bytes: usize,
    /// Completed Final telemetry is withheld from plaintext history until the user-visible checkpoint is
    /// durable. Legacy journals lack this field and remain readable.
    #[serde(default)]
    terminal: Option<TerminalTelemetryDto>,
}

impl StoredFinalJournal {
    fn from_boundary(boundary: &FinalJournalBoundary, output_reserve_bytes: usize) -> Self {
        Self {
            recording_id: boundary.recording_id.clone(),
            request_group_id: boundary.request_group_id.clone(),
            call_id: boundary.call_id.clone(),
            request_key: boundary.request_key.to_base64url(),
            fence: boundary.fence.clone(),
            state: FinalJournalState::Prepared,
            output: None,
            reserved_output_bytes: output_reserve_bytes,
            terminal: None,
        }
    }

    fn boundary(&self) -> Result<FinalJournalBoundary, ErrorCode> {
        Ok(FinalJournalBoundary {
            recording_id: self.recording_id.clone(),
            request_group_id: self.request_group_id.clone(),
            call_id: self.call_id.clone(),
            request_key: RequestKey::from_base64url(&self.request_key).ok_or(ErrorCode::Cache)?,
            fence: self.fence.clone(),
        })
    }

    fn matches(&self, boundary: &FinalJournalBoundary) -> bool {
        self.recording_id == boundary.recording_id
            && self.request_group_id == boundary.request_group_id
            && self.call_id == boundary.call_id
            && self.request_key == boundary.request_key.to_base64url()
            && self.fence == boundary.fence
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StoredProviderOutput {
    Rewrite(corti_postprocess::RewriteOutput),
    Question(corti_postprocess::QuestionOutput),
}

/// Untagged legacy support keeps schema-v1 stores readable while adding bounded-age/LRU metadata. An older
/// binary will fail closed on the new entry shape rather than deleting an unknown authenticated store.
#[derive(Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum StoredCacheEntry {
    Current {
        output: StoredProviderOutput,
        created_at_unix_ms: i64,
        last_accessed_at_unix_ms: i64,
    },
    Legacy(StoredProviderOutput),
}

impl StoredCacheEntry {
    fn new(output: StoredProviderOutput, now: i64) -> Self {
        Self::Current {
            output,
            created_at_unix_ms: now,
            last_accessed_at_unix_ms: now,
        }
    }

    fn output(&self) -> StoredProviderOutput {
        match self {
            Self::Current { output, .. } | Self::Legacy(output) => output.clone(),
        }
    }

    fn last_accessed_at(&self) -> i64 {
        match self {
            Self::Current {
                last_accessed_at_unix_ms,
                ..
            } => *last_accessed_at_unix_ms,
            Self::Legacy(_) => i64::MIN,
        }
    }

    fn expired_at(&self, now: i64) -> bool {
        match self {
            Self::Current {
                created_at_unix_ms, ..
            } => now.saturating_sub(*created_at_unix_ms) > REUSABLE_CACHE_MAX_AGE_MS,
            // Schema-v1 entries have no trustworthy age. They remain forward-readable but are discarded
            // rather than living forever outside the bounded-age policy.
            Self::Legacy(_) => true,
        }
    }

    fn touch(&mut self, now: i64) {
        let output = self.output();
        let created_at_unix_ms = match self {
            Self::Current {
                created_at_unix_ms, ..
            } => *created_at_unix_ms,
            Self::Legacy(_) => now,
        };
        *self = Self::Current {
            output,
            created_at_unix_ms,
            last_accessed_at_unix_ms: now,
        };
    }
}

struct MemoryCacheEntry {
    output: StoredProviderOutput,
    created_at_unix_ms: i64,
    last_accessed_at_unix_ms: i64,
    approx_bytes: usize,
}

impl MemoryCacheEntry {
    fn new(output: StoredProviderOutput, now: i64) -> Self {
        let approx_bytes =
            serde_json::to_vec(&output).map_or(MAX_MEMORY_CACHE_BYTES, |bytes| bytes.len());
        Self {
            output,
            created_at_unix_ms: now,
            last_accessed_at_unix_ms: now,
            approx_bytes,
        }
    }

    fn expired_at(&self, now: i64) -> bool {
        now.saturating_sub(self.created_at_unix_ms) > MEMORY_CACHE_MAX_AGE_MS
    }
}

impl StoredProviderOutput {
    fn from_output(output: &corti_postprocess::ProviderOutput) -> Self {
        match output {
            corti_postprocess::ProviderOutput::Rewrite(output) => Self::Rewrite(output.clone()),
            corti_postprocess::ProviderOutput::Question(output) => {
                Self::Question(output.output.clone())
            }
        }
    }

    fn into_output(self) -> corti_postprocess::ProviderOutput {
        match self {
            Self::Rewrite(output) => corti_postprocess::ProviderOutput::Rewrite(output),
            Self::Question(output) => {
                corti_postprocess::ProviderOutput::Question(corti_postprocess::QuestionTerminal {
                    output,
                })
            }
        }
    }
}

enum RuntimeStorePersistence {
    Memory,
    Encrypted {
        path: PathBuf,
        cipher: Box<StoreCipher>,
    },
}

struct StoreCipher {
    key: ring::aead::LessSafeKey,
}

impl StoreCipher {
    fn new(key: [u8; 32]) -> Result<Self> {
        let key = ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key)
            .map_err(|_| anyhow::anyhow!("invalid hosted store encryption key"))?;
        Ok(Self {
            key: ring::aead::LessSafeKey::new(key),
        })
    }

    fn seal(&self, mut plaintext: Vec<u8>) -> Result<Vec<u8>> {
        use zeroize::Zeroize as _;

        let mut nonce_bytes = [0u8; STORE_NONCE_BYTES];
        random_bytes(&mut nonce_bytes)?;
        let nonce = ring::aead::Nonce::assume_unique_for_key(nonce_bytes);
        if self
            .key
            .seal_in_place_append_tag(nonce, ring::aead::Aad::from(STORE_AAD), &mut plaintext)
            .is_err()
        {
            plaintext.zeroize();
            bail!("encrypting hosted store failed");
        }
        let mut envelope = Vec::with_capacity(
            STORE_MAGIC
                .len()
                .saturating_add(STORE_NONCE_BYTES)
                .saturating_add(plaintext.len()),
        );
        envelope.extend_from_slice(STORE_MAGIC);
        envelope.extend_from_slice(&nonce_bytes);
        envelope.extend_from_slice(&plaintext);
        plaintext.zeroize();
        Ok(envelope)
    }

    fn open(&self, envelope: &[u8]) -> Result<RuntimeStoreState> {
        use zeroize::Zeroize as _;

        let prefix = STORE_MAGIC.len().saturating_add(STORE_NONCE_BYTES);
        if envelope.len() < prefix.saturating_add(ring::aead::AES_256_GCM.tag_len())
            || !envelope.starts_with(STORE_MAGIC)
        {
            bail!("hosted store envelope is invalid");
        }
        let nonce_bytes: [u8; STORE_NONCE_BYTES] = envelope[STORE_MAGIC.len()..prefix]
            .try_into()
            .context("reading hosted store nonce")?;
        let mut ciphertext = envelope[prefix..].to_vec();
        let plaintext_len = self
            .key
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(nonce_bytes),
                ring::aead::Aad::from(STORE_AAD),
                &mut ciphertext,
            )
            .map_err(|_| anyhow::anyhow!("hosted store authentication failed"))?
            .len();
        let parsed = serde_json::from_slice::<RuntimeStoreDocument>(&ciphertext[..plaintext_len])
            .context("parsing encrypted hosted store")
            .and_then(|document| {
                if document.schema != STORE_SCHEMA {
                    bail!("unsupported hosted store schema");
                }
                Ok(document.state)
            });
        ciphertext.zeroize();
        parsed
    }
}

impl RuntimeStore {
    fn memory(outbox: Arc<TelemetryOutbox>, pipeline_tx: Sender<PipelineMsg>) -> Self {
        Self {
            outbox,
            pipeline_tx,
            state: RuntimeStoreState::default(),
            memory_cache: HashMap::new(),
            persistence: RuntimeStorePersistence::Memory,
        }
    }

    fn open_encrypted(
        path: PathBuf,
        key: [u8; 32],
        outbox: Arc<TelemetryOutbox>,
        pipeline_tx: Sender<PipelineMsg>,
    ) -> Result<Self> {
        let cipher = Box::new(StoreCipher::new(key)?);
        let mut state = match read_private(&path, "encrypted hosted store", MAX_STORE_BYTES)? {
            Some(bytes) => cipher.open(&bytes)?,
            None => RuntimeStoreState::default(),
        };
        Self::prune_expired_cache(&mut state, unix_millis());
        Self::fit_store_bound(&mut state)
            .map_err(|_| anyhow::anyhow!("hosted store exceeds bounds"))?;
        let store = Self {
            outbox,
            pipeline_tx,
            state,
            memory_cache: HashMap::new(),
            persistence: RuntimeStorePersistence::Encrypted { path, cipher },
        };
        store
            .persist_state()
            .map_err(|_| anyhow::anyhow!("persisting bounded hosted store failed"))?;
        Ok(store)
    }

    fn persist_terminal(&self, telemetry: &TerminalTelemetryDto) -> Result<(), ErrorCode> {
        self.outbox.append(telemetry.clone())
    }

    fn notify_terminal(&self) {
        let _ = self.pipeline_tx.send(PipelineMsg::ImportPostprocessOutbox);
    }

    fn terminal(&self, telemetry: &TerminalTelemetryDto) -> Result<(), ErrorCode> {
        self.persist_terminal(telemetry)?;
        self.notify_terminal();
        Ok(())
    }

    fn persist_state(&self) -> Result<(), ErrorCode> {
        let RuntimeStorePersistence::Encrypted { path, cipher } = &self.persistence else {
            return Ok(());
        };
        use zeroize::Zeroize as _;

        let mut plaintext = serde_json::to_vec(&RuntimeStoreDocument {
            schema: STORE_SCHEMA,
            state: self.state.clone(),
        })
        .map_err(|_| ErrorCode::Cache)?;
        if plaintext.len() > MAX_STORE_BYTES {
            plaintext.zeroize();
            return Err(ErrorCode::Cache);
        }
        let envelope = cipher.seal(plaintext).map_err(|_| ErrorCode::Cache)?;
        atomic_write_private(path, &envelope, "encrypted hosted store")
            .map_err(|_| ErrorCode::Cache)
    }

    fn transaction(
        &mut self,
        update: impl FnOnce(&mut RuntimeStoreState) -> Result<(), ErrorCode>,
    ) -> Result<(), ErrorCode> {
        let before = self.state.clone();
        if let Err(error) = update(&mut self.state) {
            self.state = before;
            return Err(error);
        }
        if let Err(error) = self.persist_state() {
            self.state = before;
            return Err(error);
        }
        Ok(())
    }

    fn journal_mut<'a>(
        state: &'a mut RuntimeStoreState,
        boundary: &FinalJournalBoundary,
    ) -> Result<&'a mut StoredFinalJournal, ErrorCode> {
        state
            .journals
            .iter_mut()
            .find(|journal| journal.call_id == boundary.call_id && journal.matches(boundary))
            .ok_or(ErrorCode::Cache)
    }

    fn prune_expired_cache(state: &mut RuntimeStoreState, now: i64) {
        state.exact_cache.retain(|_, entry| !entry.expired_at(now));
    }

    fn prune_memory_cache(&mut self, now: i64) {
        self.memory_cache.retain(|_, entry| !entry.expired_at(now));
    }

    fn insert_memory_cache(&mut self, key: String, output: StoredProviderOutput) {
        let now = unix_millis();
        self.prune_memory_cache(now);
        self.memory_cache
            .insert(key, MemoryCacheEntry::new(output, now));
        loop {
            let bytes = self
                .memory_cache
                .values()
                .map(|entry| entry.approx_bytes)
                .fold(0usize, usize::saturating_add);
            if self.memory_cache.len() <= MAX_MEMORY_CACHE_ENTRIES
                && bytes <= MAX_MEMORY_CACHE_BYTES
            {
                break;
            }
            let Some(oldest) = self
                .memory_cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_accessed_at_unix_ms)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.memory_cache.remove(&oldest);
        }
    }

    fn evict_lru_cache_entry(state: &mut RuntimeStoreState) -> bool {
        let Some(key) = state
            .exact_cache
            .iter()
            .min_by_key(|(_, entry)| entry.last_accessed_at())
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        state.exact_cache.remove(&key);
        true
    }

    fn fit_store_bound(state: &mut RuntimeStoreState) -> Result<(), ErrorCode> {
        use zeroize::Zeroize as _;

        loop {
            let mut plaintext = serde_json::to_vec(&RuntimeStoreDocument {
                schema: STORE_SCHEMA,
                state: state.clone(),
            })
            .map_err(|_| ErrorCode::Cache)?;
            let reserved = state
                .journals
                .iter()
                .map(|journal| journal.reserved_output_bytes)
                .fold(0usize, usize::saturating_add);
            let fits = plaintext.len().saturating_add(reserved) <= MAX_STORE_BYTES;
            plaintext.zeroize();
            if fits {
                return Ok(());
            }
            // Reusable entries are optional. Active recovery journals are never candidates.
            if !Self::evict_lru_cache_entry(state) {
                return Err(ErrorCode::Cache);
            }
        }
    }

    fn purge_reusable_cache_inner(&mut self) -> Result<usize, ErrorCode> {
        let removed = self.state.exact_cache.len();
        self.transaction(|state| {
            state.exact_cache.clear();
            Ok(())
        })?;
        Ok(removed)
    }

    fn update_group(
        &mut self,
        boundaries: &[FinalJournalBoundary],
        next: FinalJournalState,
    ) -> Result<(), ErrorCode> {
        if boundaries.is_empty() {
            return Ok(());
        }
        let group_id = &boundaries[0].request_group_id;
        if boundaries
            .iter()
            .any(|boundary| &boundary.request_group_id != group_id)
        {
            return Err(ErrorCode::Cache);
        }
        let expected = match next {
            FinalJournalState::Applied => FinalJournalState::ResultCached,
            FinalJournalState::Checkpointed => FinalJournalState::Applied,
            _ => return Err(ErrorCode::Cache),
        };
        if boundaries.iter().all(|boundary| {
            self.state
                .journals
                .iter()
                .find(|journal| journal.matches(boundary))
                .is_some_and(|journal| journal.state == next)
        }) {
            return Ok(());
        }
        self.transaction(|state| {
            // Validate the complete group and every source state before mutating a single journal row.
            for boundary in boundaries {
                if Self::journal_mut(state, boundary)?.state != expected {
                    return Err(ErrorCode::Cache);
                }
            }
            for boundary in boundaries {
                Self::journal_mut(state, boundary)?.state = next;
            }
            Ok(())
        })
    }
}

impl EncryptedPostprocessStore for RuntimeStore {
    fn purge_reusable_cache(&mut self) -> Result<usize, ErrorCode> {
        self.purge_reusable_cache_inner()
    }

    fn lookup_exact(&mut self, key: RequestKey) -> Result<ExactLookup, ErrorCode> {
        let key = key.to_base64url();
        let now = unix_millis();
        self.prune_memory_cache(now);
        if let Some(entry) = self.memory_cache.get_mut(&key) {
            entry.last_accessed_at_unix_ms = now;
            return Ok(ExactLookup::Hit(entry.output.clone().into_output()));
        }
        let Some(entry) = self.state.exact_cache.get(&key) else {
            // Final journals are recovery-only boundaries, never ordinary exact-cache hits.
            return Ok(ExactLookup::Miss);
        };
        if entry.expired_at(now) {
            self.transaction(|state| {
                state.exact_cache.remove(&key);
                Ok(())
            })?;
            return Ok(ExactLookup::Miss);
        }
        let output = entry.output();
        self.transaction(|state| {
            state
                .exact_cache
                .get_mut(&key)
                .ok_or(ErrorCode::Cache)?
                .touch(now);
            Ok(())
        })?;
        Ok(ExactLookup::Hit(output.into_output()))
    }

    fn evict_corrupt(&mut self, key: RequestKey) -> Result<(), ErrorCode> {
        let key = key.to_base64url();
        self.memory_cache.remove(&key);
        self.transaction(|state| {
            state.exact_cache.remove(&key);
            // A corrupt optional cache entry must not erase an active Final recovery result.
            Ok(())
        })
    }

    fn prepare_final(
        &mut self,
        boundary: &FinalJournalBoundary,
        output_reserve_bytes: usize,
    ) -> Result<(), ErrorCode> {
        self.prepare_final_group(&[(boundary.clone(), output_reserve_bytes)])
    }

    fn prepare_final_group(
        &mut self,
        reservations: &[(FinalJournalBoundary, usize)],
    ) -> Result<(), ErrorCode> {
        if reservations.is_empty() {
            return Ok(());
        }
        self.transaction(|state| {
            if reservations.iter().any(|(boundary, _)| {
                state
                    .journals
                    .iter()
                    .any(|journal| journal.call_id == boundary.call_id)
            }) {
                return Err(ErrorCode::Cache);
            }
            while state.journals.len().saturating_add(reservations.len()) > MAX_STORE_JOURNALS {
                let removable = state.journals.iter().position(|journal| {
                    matches!(
                        journal.state,
                        FinalJournalState::Checkpointed | FinalJournalState::Abandoned
                    )
                });
                let Some(index) = removable else {
                    return Err(ErrorCode::Cache);
                };
                state.journals.remove(index);
            }
            for (boundary, output_reserve_bytes) in reservations {
                // JSON row metadata can exceed the provider body slightly. Reserve twice the validated body
                // cap plus fixed envelope slack, bounded by the complete store.
                let reserve = output_reserve_bytes
                    .saturating_mul(2)
                    .saturating_add(64 * 1024)
                    .min(MAX_STORE_BYTES);
                state
                    .journals
                    .push(StoredFinalJournal::from_boundary(boundary, reserve));
            }
            Self::prune_expired_cache(state, unix_millis());
            Self::fit_store_bound(state)
        })
    }

    fn mark_final_dispatched(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode> {
        self.transaction(|state| {
            let journal = Self::journal_mut(state, boundary)?;
            if journal.state != FinalJournalState::Prepared {
                return Err(ErrorCode::Cache);
            }
            journal.state = FinalJournalState::Dispatched;
            Ok(())
        })
    }

    fn commit_validated(&mut self, commit: StoreCommit<'_>) -> Result<(), ErrorCode> {
        let key = commit.request_key.to_base64url();
        let output = StoredProviderOutput::from_output(commit.cache_output);
        let needs_durable_transaction =
            commit.final_boundary.is_some() || commit.local_cache_mode == LocalCacheMode::Reusable;
        if needs_durable_transaction {
            let now = unix_millis();
            let durable = self.transaction(|state| {
                Self::prune_expired_cache(state, now);
                if commit.local_cache_mode == LocalCacheMode::Reusable {
                    while state.exact_cache.len() >= MAX_STORE_CACHE_ENTRIES
                        && !state.exact_cache.contains_key(&key)
                    {
                        if !Self::evict_lru_cache_entry(state) {
                            return Err(ErrorCode::Cache);
                        }
                    }
                    state
                        .exact_cache
                        .insert(key.clone(), StoredCacheEntry::new(output.clone(), now));
                }
                if let Some(boundary) = commit.final_boundary {
                    let journal = Self::journal_mut(state, boundary)?;
                    journal.output = Some(output.clone());
                    journal.reserved_output_bytes = 0;
                    journal.terminal = Some(commit.telemetry.clone());
                    journal.state = FinalJournalState::ResultCached;
                }
                Self::fit_store_bound(state)
            });
            if let Err(error) = durable {
                if commit.final_boundary.is_some() {
                    // Final recovery is mandatory; never claim success across a failed crash boundary.
                    return Err(error);
                }
                // Reusable caching is optional for Live/questions. Preserve the already-paid validated
                // result in process memory rather than converting it into a user-visible provider failure.
                self.insert_memory_cache(key.clone(), output.clone());
                tracing::warn!(
                    target: "corti::hosted",
                    call_id = %commit.telemetry.call_id,
                    "reusable hosted cache persistence failed; retained validated result in memory"
                );
            }
        }
        if commit.local_cache_mode == LocalCacheMode::MemoryOnly {
            self.insert_memory_cache(key, output);
        }
        // Terminal publication is owned by the coordinator after Live application acknowledgement, or by
        // the Final checkpoint transaction. Questions publish immediately from coordinator completion.
        Ok(())
    }

    fn abandon_final(&mut self, boundary: &FinalJournalBoundary) -> Result<(), ErrorCode> {
        let mut terminal = self
            .state
            .journals
            .iter()
            .find(|journal| journal.matches(boundary))
            .ok_or(ErrorCode::Cache)?
            .terminal
            .clone();
        if let Some(telemetry) = terminal.as_mut() {
            telemetry.outcome = TerminalOutcomeDto::Failed;
            telemetry.error = Some(ErrorCode::Canceled);
            // Persist first. A crash or write failure leaves the encrypted terminal attached for retry.
            self.persist_terminal(telemetry)?;
        }
        self.transaction(|state| {
            let journal = Self::journal_mut(state, boundary)?;
            if journal.state != FinalJournalState::Checkpointed {
                journal.output = None;
                journal.reserved_output_bytes = 0;
                journal.terminal = None;
                journal.state = FinalJournalState::Abandoned;
            }
            Ok(())
        })?;
        if terminal.is_some() {
            self.notify_terminal();
        }
        Ok(())
    }

    fn mark_final_group_applied(
        &mut self,
        boundaries: &[FinalJournalBoundary],
    ) -> Result<(), ErrorCode> {
        self.update_group(boundaries, FinalJournalState::Applied)
    }

    fn mark_final_group_checkpointed(
        &mut self,
        boundaries: &[FinalJournalBoundary],
    ) -> Result<(), ErrorCode> {
        if boundaries.is_empty() {
            return Ok(());
        }
        let group_id = &boundaries[0].request_group_id;
        if boundaries
            .iter()
            .any(|boundary| &boundary.request_group_id != group_id)
        {
            return Err(ErrorCode::Cache);
        }
        if boundaries.iter().all(|boundary| {
            self.state
                .journals
                .iter()
                .find(|journal| journal.matches(boundary))
                .is_some_and(|journal| journal.state == FinalJournalState::Checkpointed)
        }) {
            return Ok(());
        }
        let mut terminals = Vec::with_capacity(boundaries.len());
        for boundary in boundaries {
            let journal = self
                .state
                .journals
                .iter()
                .find(|journal| journal.matches(boundary))
                .ok_or(ErrorCode::Cache)?;
            if journal.state != FinalJournalState::Applied {
                return Err(ErrorCode::Cache);
            }
            if let Some(terminal) = journal.terminal.clone() {
                terminals.push(terminal);
            }
        }
        // Outbox appends are durable and idempotent by call id. Keep encrypted terminal/output attached until
        // every append succeeds, then acknowledge and reclaim the journal content in one store transaction.
        for terminal in &terminals {
            self.persist_terminal(terminal)?;
        }
        self.transaction(|state| {
            for boundary in boundaries {
                if Self::journal_mut(state, boundary)?.state != FinalJournalState::Applied {
                    return Err(ErrorCode::Cache);
                }
            }
            for boundary in boundaries {
                let journal = Self::journal_mut(state, boundary)?;
                journal.output = None;
                journal.reserved_output_bytes = 0;
                journal.terminal = None;
                journal.state = FinalJournalState::Checkpointed;
            }
            Ok(())
        })?;
        if !terminals.is_empty() {
            self.notify_terminal();
        }
        Ok(())
    }

    fn recover_final(
        &mut self,
        recording_id: &str,
    ) -> Result<Option<FinalRecoveryRecord>, ErrorCode> {
        let Some(latest) = self
            .state
            .journals
            .iter()
            .rev()
            .find(|journal| journal.recording_id == recording_id)
        else {
            return Ok(None);
        };
        let group = self
            .state
            .journals
            .iter()
            .filter(|journal| {
                journal.recording_id == recording_id
                    && journal.request_group_id == latest.request_group_id
            })
            .collect::<Vec<_>>();
        // One ambiguous chunk makes the entire final group ambiguous. Never let a later Prepared sibling
        // hide an earlier paid dispatch and trigger a blind group retry after a crash.
        let selected = [
            FinalJournalState::Dispatched,
            FinalJournalState::Applied,
            FinalJournalState::ResultCached,
            FinalJournalState::Prepared,
            FinalJournalState::Abandoned,
            FinalJournalState::Checkpointed,
        ]
        .into_iter()
        .find_map(|state| {
            group
                .iter()
                .find(|journal| journal.state == state)
                .map(|journal| FinalRecoveryRecord {
                    call_id: journal.call_id.clone(),
                    state,
                })
        });
        Ok(selected)
    }

    fn lookup_final_recovery(
        &mut self,
        recording_id: &str,
        request_key: RequestKey,
    ) -> Result<crate::postprocess::FinalRecoveryLookup, ErrorCode> {
        let Some(group_id) = self
            .state
            .journals
            .iter()
            .rev()
            .find(|journal| journal.recording_id == recording_id)
            .map(|journal| journal.request_group_id.clone())
        else {
            return Ok(crate::postprocess::FinalRecoveryLookup::None);
        };
        let group = self
            .state
            .journals
            .iter()
            .filter(|journal| {
                journal.recording_id == recording_id && journal.request_group_id == group_id
            })
            .collect::<Vec<_>>();
        let recoverable = group.iter().any(|journal| {
            matches!(
                journal.state,
                FinalJournalState::ResultCached | FinalJournalState::Applied
            )
        });
        if !recoverable {
            return Ok(crate::postprocess::FinalRecoveryLookup::None);
        }
        let encoded = request_key.to_base64url();
        let Some(journal) = group.iter().find(|journal| journal.request_key == encoded) else {
            return Ok(crate::postprocess::FinalRecoveryLookup::Mismatch);
        };
        if !matches!(
            journal.state,
            FinalJournalState::ResultCached | FinalJournalState::Applied
        ) {
            return Ok(crate::postprocess::FinalRecoveryLookup::Mismatch);
        }
        let output = journal
            .output
            .clone()
            .ok_or(ErrorCode::Cache)?
            .into_output();
        Ok(crate::postprocess::FinalRecoveryLookup::Hit(Box::new(
            crate::postprocess::FinalRecoveryHit {
                boundary: journal.boundary()?,
                state: journal.state,
                output,
                terminal: journal.terminal.clone(),
            },
        )))
    }

    fn recover_final_boundaries(
        &mut self,
        call_ids: &[CallId],
    ) -> Result<Vec<FinalJournalBoundary>, ErrorCode> {
        if call_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut boundaries = Vec::with_capacity(call_ids.len());
        for call_id in call_ids {
            let journal = self
                .state
                .journals
                .iter()
                .find(|journal| &journal.call_id == call_id)
                .ok_or(ErrorCode::Cache)?;
            if !matches!(
                journal.state,
                FinalJournalState::Applied | FinalJournalState::Checkpointed
            ) {
                return Err(ErrorCode::Cache);
            }
            boundaries.push(journal.boundary()?);
        }
        if boundaries
            .iter()
            .any(|boundary| boundary.request_group_id != boundaries[0].request_group_id)
        {
            return Err(ErrorCode::Cache);
        }
        Ok(boundaries)
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
        let before = entries.clone();
        if let Some(existing) = entries
            .iter_mut()
            .find(|entry| entry.call_id == telemetry.call_id)
        {
            *existing = telemetry;
        } else {
            entries.push(telemetry);
        }
        if self.persist(&entries).is_err() {
            *entries = before;
            return Err(ErrorCode::Cache);
        }
        Ok(())
    }

    fn pending(&self) -> Vec<TerminalTelemetryDto> {
        self.entries.lock().unwrap().clone()
    }

    fn acknowledge(&self, call_ids: &HashSet<CallId>) -> Result<()> {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.clone();
        entries.retain(|entry| !call_ids.contains(&entry.call_id));
        if let Err(error) = self.persist(&entries) {
            *entries = before;
            return Err(error);
        }
        Ok(())
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

fn default_store_path() -> Result<PathBuf> {
    Ok(corti_queue::data_dir()?.join("postprocess-store.enc"))
}

fn import_outbox(queue: &Queue, outbox: &TelemetryOutbox) -> Result<usize> {
    let mut imported = HashSet::new();
    for telemetry in outbox.pending() {
        // The microphone test deliberately has no queue row. Its content-free terminal event remains visible
        // in the current Live window, then is acknowledged here instead of clogging the durable outbox.
        if telemetry
            .recording_id
            .starts_with(crate::live_test::MICROPHONE_TEST_RECORDING_PREFIX)
        {
            imported.insert(telemetry.call_id);
            continue;
        }
        // Live calls can settle before LiveNoteCreated/enqueue publishes the recording row. Keep those
        // entries pending; every later pipeline wake retries idempotently.
        if queue.get(&telemetry.recording_id)?.is_none() {
            continue;
        }
        let record = telemetry_to_record(&telemetry)?;
        queue.upsert_postprocess_call(&record)?;
        // A terminal row is one chunk, not the publication boundary for its final group. The pipeline/live
        // owner projects Complete/Fallback only after the whole group and its checkpoint commit atomically.
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

#[derive(Clone, PartialEq, Eq, Hash)]
struct ProviderPrepareKey {
    provider: ProviderId,
    transport: TransportId,
    connection_scope_id: ConnectionScopeId,
    region: Option<String>,
    force_refresh: bool,
}

impl From<&ProviderPrepareSpec> for ProviderPrepareKey {
    fn from(spec: &ProviderPrepareSpec) -> Self {
        Self {
            provider: spec.provider.clone(),
            transport: spec.transport.clone(),
            connection_scope_id: spec.scope.connection_scope_id.clone(),
            region: spec.scope.region.clone(),
            force_refresh: spec.force_refresh,
        }
    }
}

struct PendingProviderPrepare {
    spec: ProviderPrepareSpec,
    started: bool,
    dispatch: Vec<QueuedProviderPrepare>,
    refresh: Vec<Sender<Result<ProviderStateDto, ErrorCode>>>,
}

enum ProviderPrepareCompletion {
    Completed {
        key: ProviderPrepareKey,
        preparation: ProviderPreparation,
    },
}

#[cfg_attr(not(feature = "local"), allow(dead_code))]
enum ServiceCommand {
    BeginSession {
        recording_id: String,
        reply: Sender<Result<(), ErrorCode>>,
    },
    EndSession {
        recording_id: String,
        reply: Sender<Result<(), ErrorCode>>,
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
    SaveBedrockSetup {
        request: SaveBedrockSetupRequest,
        reply: Sender<Result<HostedMutationResult, ErrorCode>>,
    },
    ClearBedrockSetup {
        request: ClearBedrockSetupRequest,
        reply: Sender<Result<HostedMutationResult, ErrorCode>>,
    },
    SetBedrockCredentialMode {
        request: BedrockCredentialModeRequest,
        reply: Sender<Result<HostedMutationResult, ErrorCode>>,
    },
    SetVertexModels {
        request: VertexModelsRequest,
        reply: Sender<Result<HostedMutationResult, ErrorCode>>,
    },
    PurgeReusableCache {
        reply: Sender<Result<usize, ErrorCode>>,
    },
    RefreshProvider {
        provider: ProviderId,
        transport: TransportId,
        reply: Sender<Result<ProviderStateDto, ErrorCode>>,
    },
    SyncProviderCredential {
        provider: ProviderId,
        transport: TransportId,
        reply: Sender<Result<ProviderStateDto, ErrorCode>>,
    },
    InstallChatGptScope {
        scope_id: Option<ConnectionScopeId>,
        refresh_catalog: bool,
        reply: Sender<Result<ProviderStateDto, ErrorCode>>,
    },
    ProviderSecretChanged {
        request: SecretSlotRequest,
        reply: Sender<Result<(), ErrorCode>>,
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
        reply: Sender<Result<(), ErrorCode>>,
    },
    MarkFinalCheckpointed {
        call_ids: Vec<CallId>,
        reply: Sender<Result<(), ErrorCode>>,
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
        request: PinnedQuestionUpdateRequest,
        reply: Sender<Result<HostedMutationResult, ErrorCode>>,
    },
    AssistantSnapshot {
        reply: Sender<AssistantSnapshotDto>,
    },
}

struct PendingFinal {
    live_session: bool,
    deadline_micros: u64,
    original: DiarizedTranscript,
    /// Original transcript segment index for every non-empty hosted row. Keeping the index alongside the
    /// row id prevents filtered blank segments from shifting a validated replacement onto another segment.
    row_positions: Vec<(usize, RowId)>,
    call_ids: Vec<CallId>,
    /// Runtime call id to durable journal call id. Recovery attempts keep their new scheduler identity while
    /// checkpoint acknowledgement resumes the encrypted boundary created by the earlier process.
    settled_call_ids: HashMap<CallId, CallId>,
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
    provider_access: Arc<dyn ProviderAccess>,
    vertex_resolver: Arc<dyn VertexResolver>,
    worker_tx: Sender<WorkerCompletion>,
    worker_rx: Receiver<WorkerCompletion>,
    provider_prepare_tx: Sender<ProviderPrepareCompletion>,
    provider_prepare_rx: Receiver<ProviderPrepareCompletion>,
    pending_provider_prepares: usize,
    provider_prepares: HashMap<ProviderPrepareKey, PendingProviderPrepare>,
    provider_prepare_backlog: VecDeque<ProviderPrepareKey>,
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
    secret_presence: SecretPresenceSource,
}

impl Service {
    fn run(
        mut self,
        command_rx: Receiver<ServiceCommand>,
        priority_rx: Receiver<ServiceCommand>,
        ingress_rx: Receiver<HotPathCommand>,
    ) {
        let mut command_connected = true;
        let mut priority_connected = true;
        let mut next_tick = Instant::now();
        loop {
            let mut did_work = false;
            for _ in 0..MAX_PRIORITY_DRAIN {
                match priority_rx.try_recv() {
                    Ok(command) => {
                        did_work = true;
                        self.handle_command(command);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        priority_connected = false;
                        break;
                    }
                }
            }
            for _ in 0..MAX_COMMAND_DRAIN {
                match command_rx.try_recv() {
                    Ok(command) => {
                        did_work = true;
                        self.handle_command(command);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        command_connected = false;
                        break;
                    }
                }
            }
            if !did_work && command_connected {
                let until_tick = next_tick.saturating_duration_since(Instant::now());
                let timeout = SERVICE_IDLE_POLL.min(until_tick.max(Duration::from_micros(1)));
                match command_rx.recv_timeout(timeout) {
                    Ok(command) => self.handle_command(command),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => command_connected = false,
                }
            } else if !did_work && !command_connected {
                std::thread::sleep(SERVICE_IDLE_POLL);
            }

            self.drain_ingress(&ingress_rx, MAX_INGRESS_DRAIN);
            self.drain_provider_events(MAX_PROVIDER_EVENT_DRAIN);
            self.drain_workers(MAX_WORKER_DRAIN);
            self.drain_provider_prepares(MAX_PROVIDER_PREPARE_DRAIN);
            self.drain_vertex(MAX_VERTEX_DRAIN);

            let now = Instant::now();
            if now >= next_tick {
                self.coordinator.tick();
                self.expire_pending_finals();
                self.sync_pinned_revision();
                if let Some(attempt) = self.coordinator.drive_vertex() {
                    self.spawn_vertex_resolution(attempt);
                }
                next_tick = now + SERVICE_TICK;
            }
            self.drive_dispatch(MAX_DISPATCH_DRAIN);
            self.publish_events(false);

            if !command_connected && !priority_connected {
                self.cancel_pending_finals(ErrorCode::Canceled);
                return;
            }
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
            ServiceCommand::EndSession {
                recording_id,
                reply,
            } => {
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
                let _ = reply.send(Ok(()));
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
            ServiceCommand::SaveBedrockSetup { request, reply } => {
                let result = self.save_bedrock_setup(request);
                let _ = reply.send(result);
            }
            ServiceCommand::ClearBedrockSetup { request, reply } => {
                let result = self.clear_bedrock_setup(request);
                let _ = reply.send(result);
            }
            ServiceCommand::SetBedrockCredentialMode { request, reply } => {
                let result = self.set_bedrock_credential_mode(request);
                let _ = reply.send(result);
            }
            ServiceCommand::SetVertexModels { request, reply } => {
                let result = self.set_vertex_models(request);
                let _ = reply.send(result);
            }
            ServiceCommand::PurgeReusableCache { reply } => {
                let result = self.coordinator.purge_reusable_cache();
                let _ = reply.send(result);
            }
            ServiceCommand::RefreshProvider {
                provider,
                transport,
                reply,
            } => self.start_provider_refresh(provider, transport, reply),
            ServiceCommand::SyncProviderCredential {
                provider,
                transport,
                reply,
            } => {
                let result = self
                    .coordinator
                    .sync_provider_credential(&provider, &transport);
                if result.is_ok() {
                    self.bump_state();
                    self.refresh_snapshot();
                }
                let _ = reply.send(result);
            }
            ServiceCommand::InstallChatGptScope {
                scope_id,
                refresh_catalog,
                reply,
            } => {
                let result = self.install_chatgpt_scope(scope_id, refresh_catalog);
                let _ = reply.send(result);
            }
            ServiceCommand::ProviderSecretChanged { request, reply } => {
                let result = self.provider_secret_changed(request);
                // Publish the refreshed projection before releasing the secure action's IPC response.
                self.publish_events(true);
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
            ServiceCommand::AbandonFinal { call_ids, reply } => {
                for call_id in call_ids {
                    self.coordinator
                        .cancel_call(&call_id, CancellationReason::Superseded);
                }
                let _ = reply.send(Ok(()));
            }
            ServiceCommand::MarkFinalCheckpointed { call_ids, reply } => {
                let result = self
                    .coordinator
                    .mark_final_group_checkpointed(&call_ids)
                    .map_err(coordinator_error_code);
                let _ = reply.send(result);
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
            ServiceCommand::SetPinnedTemplate { request, reply } => {
                let result = self.set_pinned_template(request);
                let _ = reply.send(result);
            }
            ServiceCommand::AssistantSnapshot { reply } => {
                let _ = reply.send(self.assistant_snapshot());
            }
        }
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
            HostedPatchInput::SetDisplayPreferences { .. } => unreachable!(),
        };
        let outcome = self
            .coordinator
            .apply_patch(patch)
            .map_err(control_error_code)?;
        if !matches!(outcome, PatchOutcome::Unchanged(_)) {
            self.bump_state();
            // A user may save the template, enable Questions, acknowledge Master, and enable automatic
            // updates in any order. Reconsider the transcript already present after every effective control
            // change so that sequence never requires another 40 words merely to start the first answer.
            self.schedule_pinned_from_current();
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
        if (provider.as_str(), transport.as_str()) == ("amazon", "bedrock_runtime") {
            return self.update_bedrock_scope_compat(
                request.observed_state_revision,
                request.alias,
                request.project,
                request.region,
                request.quota_project,
            );
        }
        let alias = bounded_optional(request.alias)?;
        let project = bounded_optional(request.project)?;
        let region = bounded_optional(request.region)?;
        let quota_project = bounded_optional(request.quota_project)?;
        let configured =
            alias.is_some() || project.is_some() || region.is_some() || quota_project.is_some();
        let existing = {
            let preferences = self.preferences.lock().unwrap();
            let values = preferences.values();
            match (provider.as_str(), transport.as_str()) {
                ("google", "vertex_api") => values.providers.vertex.clone(),
                ("openai", "openai_api") => values.providers.openai.scope.clone(),
                ("anthropic", "anthropic_api") => values.providers.anthropic.scope.clone(),
                ("amazon", "bedrock_runtime") => values.providers.bedrock.scope.clone(),
                _ => return Err(ErrorCode::PolicyBlocked),
            }
        };
        if existing.alias == alias
            && existing.project == project
            && existing.region == region
            && existing.quota_project == quota_project
        {
            return Ok(HostedMutationResult::Unchanged {
                settings: self.current_settings(),
            });
        }
        let routing_changed = existing.project != project
            || existing.region != region
            || existing.quota_project != quota_project;
        let generated_id = if configured {
            if !routing_changed {
                existing
                    .connection_scope_id
                    .clone()
                    .map_or_else(new_connection_scope_id, Ok)?
            } else {
                new_connection_scope_id()?
            }
            .into()
        } else {
            None
        };
        let technical_identity_changed = existing.connection_scope_id != generated_id;
        self.revise_preferences(|values| {
            let scope = match (provider.as_str(), transport.as_str()) {
                ("google", "vertex_api") => &mut values.providers.vertex,
                ("openai", "openai_api") => &mut values.providers.openai.scope,
                ("anthropic", "anthropic_api") => &mut values.providers.anthropic.scope,
                ("amazon", "bedrock_runtime") => &mut values.providers.bedrock.scope,
                _ => unreachable!("validated provider transport"),
            };
            scope.connection_scope_id = generated_id;
            scope.alias = alias;
            scope.project = project;
            scope.region = region;
            scope.quota_project = quota_project;
        })?;
        if technical_identity_changed {
            self.coordinator
                .cancel_provider_requests(&provider, &transport);
            self.coordinator
                .invalidate_provider_scope(&provider, &transport);
        }
        self.bump_state();
        self.refresh_snapshot();
        Ok(HostedMutationResult::Applied {
            settings: self.current_settings(),
        })
    }

    fn install_chatgpt_scope(
        &mut self,
        scope_id: Option<ConnectionScopeId>,
        refresh_catalog: bool,
    ) -> Result<ProviderStateDto, ErrorCode> {
        let provider = ProviderId::new(CHATGPT_PROVIDER).map_err(|_| ErrorCode::Internal)?;
        let transport = TransportId::new(CHATGPT_TRANSPORT).map_err(|_| ErrorCode::Internal)?;
        let actual_scope = self
            .coordinator
            .provider_connection_scope(&provider, &transport);
        if actual_scope
            .as_ref()
            .map(|scope| &scope.connection_scope_id)
            != scope_id.as_ref()
        {
            return Err(ErrorCode::Internal);
        }
        // Scope is derived live from the credential account id, never persisted independently. Fence every
        // successful login/logout so an in-flight request from the previous account cannot apply.
        self.coordinator
            .cancel_provider_requests(&provider, &transport);
        self.coordinator
            .invalidate_provider_scope(&provider, &transport);
        let result = if refresh_catalog {
            self.refresh_provider(&provider, &transport)
        } else {
            self.coordinator
                .sync_provider_credential(&provider, &transport)
        }?;
        self.bump_state();
        self.refresh_snapshot();
        Ok(result)
    }

    fn provider_secret_changed(&mut self, request: SecretSlotRequest) -> Result<(), ErrorCode> {
        let (provider_name, transport_name, relevant) = match request {
            SecretSlotRequest::OpenAi => ("openai", "openai_api", true),
            SecretSlotRequest::Anthropic => ("anthropic", "anthropic_api", true),
            SecretSlotRequest::Aws { .. } => {
                let relevant = self
                    .preferences
                    .lock()
                    .unwrap()
                    .values()
                    .providers
                    .bedrock
                    .credential_mode
                    == AwsCredentialMode::StaticKeychain;
                ("amazon", "bedrock_runtime", relevant)
            }
        };
        if !relevant {
            // AWS slots are retained while another authentication method is selected. Their presence still
            // belongs in the snapshot, but dormant keys must not rotate the active profile/account fence.
            self.bump_state();
            self.refresh_snapshot();
            return Ok(());
        }
        let provider = ProviderId::new(provider_name).map_err(|_| ErrorCode::Internal)?;
        let transport = TransportId::new(transport_name).map_err(|_| ErrorCode::Internal)?;
        let configured = {
            let preferences = self.preferences.lock().unwrap();
            let values = preferences.values();
            match (provider_name, transport_name) {
                ("openai", "openai_api") => {
                    values.providers.openai.scope.connection_scope_id.is_some()
                }
                ("anthropic", "anthropic_api") => values
                    .providers
                    .anthropic
                    .scope
                    .connection_scope_id
                    .is_some(),
                ("amazon", "bedrock_runtime") => {
                    values.providers.bedrock.scope.connection_scope_id.is_some()
                }
                _ => false,
            }
        };
        let persistence = if configured {
            let replacement = new_connection_scope_id()?;
            self.revise_preferences(|values| {
                let scope = match (provider_name, transport_name) {
                    ("openai", "openai_api") => &mut values.providers.openai.scope,
                    ("anthropic", "anthropic_api") => &mut values.providers.anthropic.scope,
                    ("amazon", "bedrock_runtime") => &mut values.providers.bedrock.scope,
                    _ => unreachable!("validated secret provider"),
                };
                scope.connection_scope_id = Some(replacement);
            })
        } else {
            Ok(())
        };

        // The secure store has already changed, so always clear sticky rejection and fence in-flight work,
        // even if rotating the durable scope failed. Returning the persistence error remains truthful while
        // ensuring a replacement key can recover without a relaunch.
        self.coordinator
            .notify_credentials_changed(&provider, &transport);
        self.coordinator
            .invalidate_provider_scope(&provider, &transport);
        self.coordinator
            .cancel_provider_requests(&provider, &transport);
        let credential = self
            .coordinator
            .sync_provider_credential(&provider, &transport);
        self.bump_state();
        self.refresh_snapshot();
        persistence?;
        credential.map(|_| ())
    }

    fn set_vertex_models(
        &mut self,
        request: VertexModelsRequest,
    ) -> Result<HostedMutationResult, ErrorCode> {
        if request.observed_state_revision != self.state_revision {
            return Ok(HostedMutationResult::Conflict {
                settings: self.current_settings(),
            });
        }
        let mut models = Vec::new();
        for model in request.models {
            let model = model.trim().to_owned();
            if model.is_empty() || models.contains(&model) {
                continue;
            }
            models.push(model);
        }
        if models.len() > MAX_VERTEX_MODELS {
            return Err(ErrorCode::PolicyBlocked);
        }
        if self
            .preferences
            .lock()
            .unwrap()
            .values()
            .providers
            .vertex_models
            == models
        {
            return Ok(HostedMutationResult::Unchanged {
                settings: self.current_settings(),
            });
        }
        // `revise` runs `validate`, so a malformed model id is rejected before it can disarm the catalog.
        self.revise_preferences(|values| values.providers.vertex_models = models)
            .map_err(|_| ErrorCode::PolicyBlocked)?;
        let provider = ProviderId::new(VERTEX_PROVIDER).map_err(|_| ErrorCode::Internal)?;
        let transport = TransportId::new(VERTEX_TRANSPORT).map_err(|_| ErrorCode::Internal)?;
        self.coordinator
            .invalidate_provider_scope(&provider, &transport);
        self.bump_state();
        self.refresh_snapshot();
        Ok(HostedMutationResult::Applied {
            settings: self.current_settings(),
        })
    }

    fn update_bedrock_scope_compat(
        &mut self,
        observed_state_revision: u64,
        alias: Option<String>,
        project: Option<String>,
        region: Option<String>,
        quota_project: Option<String>,
    ) -> Result<HostedMutationResult, ErrorCode> {
        if bounded_optional(project)?.is_some() || bounded_optional(quota_project)?.is_some() {
            return Err(ErrorCode::PolicyBlocked);
        }
        let alias = bounded_optional(alias)?;
        let region = bounded_optional(region)?;
        if alias.is_none() && region.is_none() {
            return self.clear_bedrock_setup(ClearBedrockSetupRequest {
                observed_state_revision,
            });
        }
        let existing = self
            .preferences
            .lock()
            .unwrap()
            .values()
            .providers
            .bedrock
            .clone();
        self.save_bedrock_setup(SaveBedrockSetupRequest {
            observed_state_revision,
            mode: existing.credential_mode,
            profile: existing.profile,
            role_arn: existing.role_arn,
            // The old label editor omitted region. Preserve the persisted one instead of submitting a
            // guaranteed-invalid partial mutation; the new command always sends the complete draft.
            region: region.or(existing.scope.region).unwrap_or_default(),
            setup_name: alias.or(existing.scope.alias).unwrap_or_default(),
        })
    }

    fn save_bedrock_setup(
        &mut self,
        request: SaveBedrockSetupRequest,
    ) -> Result<HostedMutationResult, ErrorCode> {
        if request.observed_state_revision != self.state_revision {
            return Ok(HostedMutationResult::Conflict {
                settings: self.current_settings(),
            });
        }
        let profiles = if matches!(
            request.mode,
            AwsCredentialMode::Profile | AwsCredentialMode::Sso | AwsCredentialMode::AssumeRole
        ) {
            discovered_aws_profiles()
        } else {
            Vec::new()
        };
        let key_presence = if request.mode == AwsCredentialMode::StaticKeychain {
            BedrockKeyPresence {
                access_key_id: (self.secret_presence)(SecretPurpose::AwsAccessKeyId),
                secret_access_key: (self.secret_presence)(SecretPurpose::AwsSecretAccessKey),
            }
        } else {
            BedrockKeyPresence {
                access_key_id: false,
                secret_access_key: false,
            }
        };
        let setup = match normalize_and_validate_bedrock_setup(request, &profiles, key_presence) {
            Ok(setup) => setup,
            Err(issue) => {
                return Ok(HostedMutationResult::Invalid {
                    settings: self.current_settings(),
                    field: issue.field,
                    reason: issue.reason,
                });
            }
        };
        self.persist_bedrock_setup(setup)
    }

    fn clear_bedrock_setup(
        &mut self,
        request: ClearBedrockSetupRequest,
    ) -> Result<HostedMutationResult, ErrorCode> {
        if request.observed_state_revision != self.state_revision {
            return Ok(HostedMutationResult::Conflict {
                settings: self.current_settings(),
            });
        }
        let existing = self
            .preferences
            .lock()
            .unwrap()
            .values()
            .providers
            .bedrock
            .clone();
        if existing == BedrockProviderPreferences::default() {
            return Ok(HostedMutationResult::Unchanged {
                settings: self.current_settings(),
            });
        }
        self.revise_preferences(|values| {
            values.providers.bedrock = BedrockProviderPreferences::default();
        })?;
        self.fence_bedrock_configuration_change()?;
        Ok(HostedMutationResult::Applied {
            settings: self.current_settings(),
        })
    }

    /// Compatibility command: a mode edit is atomic with the already-persisted region and setup name.
    /// Legacy incomplete documents return a typed validation issue instead of accepting a partial save.
    fn set_bedrock_credential_mode(
        &mut self,
        request: BedrockCredentialModeRequest,
    ) -> Result<HostedMutationResult, ErrorCode> {
        let existing = self
            .preferences
            .lock()
            .unwrap()
            .values()
            .providers
            .bedrock
            .clone();
        self.save_bedrock_setup(SaveBedrockSetupRequest {
            observed_state_revision: request.observed_state_revision,
            mode: request.mode,
            profile: request.profile,
            role_arn: request.role_arn,
            region: existing.scope.region.unwrap_or_default(),
            setup_name: existing.scope.alias.unwrap_or_default(),
        })
    }

    fn persist_bedrock_setup(
        &mut self,
        setup: NormalizedBedrockSetup,
    ) -> Result<HostedMutationResult, ErrorCode> {
        let existing = self
            .preferences
            .lock()
            .unwrap()
            .values()
            .providers
            .bedrock
            .clone();
        let routing_changed = existing.credential_mode != setup.mode
            || existing.profile != setup.profile
            || existing.role_arn != setup.role_arn
            || existing.scope.region.as_deref() != Some(setup.region.as_str());
        let unchanged = !routing_changed
            && existing.scope.alias.as_deref() == Some(setup.setup_name.as_str())
            && existing.scope.connection_scope_id.is_some()
            && existing.scope.project.is_none()
            && existing.scope.quota_project.is_none();
        if unchanged {
            return Ok(HostedMutationResult::Unchanged {
                settings: self.current_settings(),
            });
        }

        // The display name is deliberately excluded from the technical identity. Existing legacy ids are
        // retained on a rename; account-routing edits and first-time setup receive fresh random identities.
        let next_scope_id = if !routing_changed {
            existing.scope.connection_scope_id.clone()
        } else {
            None
        }
        .map_or_else(new_connection_scope_id, Ok)?;
        let technical_identity_created = existing.scope.connection_scope_id.is_none();
        self.revise_preferences(|values| {
            let bedrock = &mut values.providers.bedrock;
            bedrock.credential_mode = setup.mode;
            bedrock.profile = setup.profile;
            bedrock.role_arn = setup.role_arn;
            bedrock.scope.connection_scope_id = Some(next_scope_id);
            bedrock.scope.alias = Some(setup.setup_name);
            bedrock.scope.region = Some(setup.region);
            bedrock.scope.project = None;
            bedrock.scope.quota_project = None;
        })?;
        if routing_changed || technical_identity_created {
            self.fence_bedrock_configuration_change()?;
        } else {
            // A display-only rename neither changes technical identity nor invalidates catalogs/lanes.
            self.bump_state();
            self.refresh_snapshot();
        }
        Ok(HostedMutationResult::Applied {
            settings: self.current_settings(),
        })
    }

    fn fence_bedrock_configuration_change(&mut self) -> Result<(), ErrorCode> {
        let provider = ProviderId::new("amazon").map_err(|_| ErrorCode::Internal)?;
        let transport = TransportId::new("bedrock_runtime").map_err(|_| ErrorCode::Internal)?;
        self.coordinator
            .cancel_provider_requests(&provider, &transport);
        self.coordinator
            .notify_credentials_changed(&provider, &transport);
        self.coordinator
            .invalidate_provider_scope(&provider, &transport);
        self.bump_state();
        self.refresh_snapshot();
        Ok(())
    }

    fn validate_settings_selection(
        &self,
        _family: LaneFamily,
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
        let catalog = self
            .coordinator
            .refresh_provider(provider, transport, &scope);
        // Catalog discovery can refresh/rotate authentication. Re-sample afterward on both success and
        // failure so a rejected or rotated-but-unsaved credential cannot remain projected as Ready.
        let credential = self
            .coordinator
            .sync_provider_credential(provider, transport);
        match catalog {
            Ok(state) => {
                let state = credential.unwrap_or(state);
                self.bump_state();
                self.refresh_snapshot();
                Ok(state)
            }
            Err(error) => {
                if credential.is_ok() {
                    self.bump_state();
                    self.refresh_snapshot();
                }
                Err(error)
            }
        }
    }

    fn set_pinned_template(
        &mut self,
        request: PinnedQuestionUpdateRequest,
    ) -> Result<HostedMutationResult, ErrorCode> {
        if request.observed_state_revision != self.state_revision {
            return Ok(HostedMutationResult::Conflict {
                settings: self.current_settings(),
            });
        }
        let template = request.template;
        if template.len() > crate::postprocess::MAX_QUESTION_TEXT_BYTES
            || template.chars().any(char::is_control)
        {
            return Err(ErrorCode::PolicyBlocked);
        }
        if self
            .preferences
            .lock()
            .unwrap()
            .values()
            .pinned_question_template
            == template
        {
            return Ok(HostedMutationResult::Unchanged {
                settings: self.current_settings(),
            });
        }
        // The frontend is the single debounce owner. Persist and fence one accepted revision atomically on
        // this serial service thread so an older response/edit can never win after a newer revision.
        self.revise_preferences(|values| values.pinned_question_template = template.clone())?;
        self.coordinator
            .edit_pinned_template(template)
            .map_err(submit_error_code)?;
        self.pinned_exchange = None;
        self.bump_state();
        self.refresh_snapshot();
        Ok(HostedMutationResult::Applied {
            settings: self.current_settings(),
        })
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

    fn drain_ingress(&mut self, ingress_rx: &Receiver<HotPathCommand>, limit: usize) {
        for _ in 0..limit {
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
                Ok(HotPathCommand::Barrier { reply }) => {
                    let _ = reply.send(());
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
        let input_token_budget = self.input_token_budget(LaneFamily::Live);
        let mut bytes = 0usize;
        let mut tokens = 0u64;
        let mut targets = Vec::new();
        let incoming = &self.ledger[old_len..];
        for row in incoming {
            let row_tokens = estimated_row_tokens(row);
            let next_bytes = bytes.saturating_add(row.text.len());
            let next_tokens = tokens.saturating_add(row_tokens);
            if row_tokens > input_token_budget
                || next_tokens > input_token_budget
                || (!targets.is_empty()
                    && (targets.len() >= MAX_LIVE_TARGET_ROWS
                        || next_bytes > MAX_LIVE_TARGET_BYTES))
            {
                break;
            }
            bytes = next_bytes;
            tokens = next_tokens;
            targets.push(row.clone());
        }
        if targets.len() != incoming.len() {
            // Never imply complete hosted coverage when a latency/model token budget omitted finalized rows.
            // The raw UI remains complete and the stronger live final safely falls back to raw.
            self.ingress_incomplete.store(true, Ordering::Release);
        }
        if targets.is_empty() {
            return None;
        }
        let context_start = old_len.saturating_sub(MAX_CONTEXT_ROWS);
        let context = bounded_rows_from_end(
            &self.ledger[context_start..old_len],
            input_token_budget.saturating_sub(tokens),
        )
        .0;
        self.build_submission(
            recording_id,
            Lane::Live,
            targets,
            context,
            None,
            false,
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
        self.schedule_pinned_from_current();
    }

    fn schedule_pinned_from_current(&mut self) {
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
        let (context, context_truncated) =
            bounded_question_context(&self.ledger, self.input_token_budget(LaneFamily::Question));
        self.build_submission(
            recording_id,
            Lane::PinnedQuestion,
            Vec::new(),
            context,
            Some(&template),
            context_truncated,
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
        let (context, context_truncated) =
            bounded_question_context(&self.ledger, self.input_token_budget(LaneFamily::Question));
        let submission = self.build_submission(
            &recording_id,
            Lane::AdHocQuestion,
            Vec::new(),
            context,
            Some(&question),
            context_truncated,
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
        let requires_final_recovery = match self.coordinator.recover_final(&recording_id) {
            Ok(FinalRecoveryDirective::None | FinalRecoveryDirective::ResumePrepared { .. }) => {
                false
            }
            Ok(
                FinalRecoveryDirective::ResumeEncryptedResult { .. }
                | FinalRecoveryDirective::ResumeCheckpoint { .. },
            ) => true,
            Ok(FinalRecoveryDirective::Fallback { call_id, code, .. }) => {
                self.release_batch_session(live_session);
                let _ = reply.send(fallback_final(transcript, code, vec![call_id], None));
                return;
            }
            Err(_) => {
                self.release_batch_session(live_session);
                let _ = reply.send(fallback_final(
                    transcript,
                    ErrorCode::Cache,
                    Vec::new(),
                    None,
                ));
                return;
            }
        };
        let indexed_rows = transcript_rows(&transcript);
        let rows: Vec<TranscriptRow> = indexed_rows.iter().map(|(_, row)| row.clone()).collect();
        if rows.is_empty() {
            self.release_batch_session(live_session);
            let _ = reply.send(fallback_final(
                transcript,
                ErrorCode::PolicyBlocked,
                Vec::new(),
                None,
            ));
            return;
        }
        let watermark = if self.coordinator.watermark().transcript_revision == 0 {
            match self.coordinator.observe_finalized_rows(&rows) {
                Ok(value) => value,
                Err(_) => {
                    self.release_batch_session(live_session);
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
        let chunks = match final_chunks(
            &rows,
            self.input_token_budget(LaneFamily::Final)
                .min(MAX_FINAL_INPUT_TOKENS),
        ) {
            Ok(chunks) => chunks,
            Err(code) => {
                self.release_batch_session(live_session);
                let _ = reply.send(fallback_final(transcript, code, Vec::new(), None));
                return;
            }
        };
        let group_id = match self.next_group_id("final") {
            Ok(value) => value,
            Err(code) => {
                self.release_batch_session(live_session);
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
                false,
                watermark,
                Some((group_id.clone(), target_id)),
                deadline,
            ) {
                Ok(mut value) => {
                    value.requires_final_recovery = requires_final_recovery;
                    submissions.push(value);
                }
                Err(code) => {
                    self.release_batch_session(live_session);
                    let _ = reply.send(fallback_final(transcript, code, Vec::new(), None));
                    return;
                }
            }
        }
        let metadata = match self.applied_metadata(LaneFamily::Final) {
            Ok(value) => value,
            Err(code) => {
                self.release_batch_session(live_session);
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
                self.release_batch_session(live_session);
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
                row_positions: indexed_rows
                    .into_iter()
                    .map(|(segment_index, row)| (segment_index, row.row_id))
                    .collect(),
                settled_call_ids: call_ids
                    .iter()
                    .cloned()
                    .map(|call_id| (call_id.clone(), call_id))
                    .collect(),
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
        context_truncated: bool,
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
                context_truncated,
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
        let provider_cache_key = (lane_control.selection.cache_policy.provider
            == ProviderCacheMode::ExplicitStablePrefix)
            .then(|| {
                ProviderCacheKey::derive(
                    &self.digest_key,
                    &ProviderCacheKeyMaterial {
                        provider: &provider,
                        transport: &transport,
                        support_tier: descriptor.support_tier,
                        connection_scope_id: &scope.connection_scope_id,
                        region: scope.region.as_deref(),
                        exact_model_id: &model,
                        adapter_version: adapter_version(&transport),
                        prompt_template_version: PROMPT_TEMPLATE_VERSION,
                        output_schema_version: OUTPUT_SCHEMA_VERSION,
                        prompt_task: if lane.is_question() {
                            PromptTask::Question
                        } else {
                            PromptTask::Rewrite
                        },
                        provider_cache_mode: lane_control.selection.cache_policy.provider,
                        word_bank_canonical_digest: self.word_bank.content_digest(),
                    },
                )
            });
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
            provider_cache_key,
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
                context_truncated,
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
            context_truncated,
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

    fn input_token_budget(&self, family: LaneFamily) -> u64 {
        let lane = match family {
            LaneFamily::Live => &self.coordinator.control_snapshot().live,
            LaneFamily::Final => &self.coordinator.control_snapshot().final_lane,
            LaneFamily::Question => &self.coordinator.control_snapshot().questions,
        };
        let (Some(provider), Some(transport), Some(model_id)) = (
            lane.selection.provider.as_ref(),
            lane.selection.transport.as_ref(),
            lane.selection.model.as_ref(),
        ) else {
            return DEFAULT_INPUT_TOKEN_BUDGET;
        };
        self.coordinator
            .provider_states()
            .filter(|state| {
                &state.descriptor.provider == provider && &state.descriptor.transport == transport
            })
            .flat_map(|state| state.models.iter())
            .find(|model| &model.exact_model_id == model_id)
            .map(|model| {
                model
                    .max_context_tokens
                    .saturating_sub(model.max_output_tokens)
                    .saturating_sub(PROMPT_TOKEN_RESERVE)
            })
            .filter(|budget| *budget > 0)
            .unwrap_or(DEFAULT_INPUT_TOKEN_BUDGET)
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
        if (provider.as_str(), transport.as_str()) == (CHATGPT_PROVIDER, CHATGPT_TRANSPORT) {
            return self
                .coordinator
                .provider_connection_scope(provider, transport)
                .ok_or(ErrorCode::PolicyBlocked);
        }
        let preferences = self.preferences.lock().unwrap();
        let values = preferences.values();
        let scope = match (provider.as_str(), transport.as_str()) {
            ("google", "vertex_api") => &values.providers.vertex,
            ("openai", "openai_api") => &values.providers.openai.scope,
            ("anthropic", "anthropic_api") => &values.providers.anthropic.scope,
            ("amazon", "bedrock_runtime") => &values.providers.bedrock.scope,
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

    fn drive_dispatch(&mut self, limit: usize) {
        for _ in 0..limit {
            match self.coordinator.dispatch_next() {
                DispatchOutcome::Prepare(request) => self.spawn_provider_prepare(request),
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

    fn drain_vertex(&mut self, limit: usize) {
        for _ in 0..limit {
            let Ok((attempt, outcome)) = self.vertex_rx.try_recv() else {
                break;
            };
            let _ = self.coordinator.complete_vertex(attempt, outcome);
        }
    }

    fn start_provider_refresh(
        &mut self,
        provider: ProviderId,
        transport: TransportId,
        reply: Sender<Result<ProviderStateDto, ErrorCode>>,
    ) {
        let scope = match self.scope_for(&provider, &transport) {
            Ok(scope) => scope,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let is_vertex =
            (provider.as_str(), transport.as_str()) == (VERTEX_PROVIDER, VERTEX_TRANSPORT);
        let credential_override = is_vertex.then(|| {
            self.coordinator
                .provider_states()
                .find(|state| {
                    state.descriptor.provider == provider && state.descriptor.transport == transport
                })
                .map_or(CredentialState::Absent, |state| state.credential.clone())
        });
        let spec = ProviderPrepareSpec {
            provider,
            transport,
            scope,
            credential_override,
            force_refresh: true,
        };
        let key = ProviderPrepareKey::from(&spec);
        if let Some(pending) = self.provider_prepares.get_mut(&key) {
            pending.refresh.push(reply);
            return;
        }
        if !is_vertex {
            // One explicit user refresh generation clears a provider-owned rejection latch and bypasses the
            // prior catalog once. Concurrent refresh callers join this same preparation instead of repeatedly
            // invalidating an in-flight AWS lease.
            self.coordinator
                .notify_credentials_changed(&spec.provider, &spec.transport);
        }
        self.spawn_provider_refresh(spec, reply);
    }

    fn spawn_provider_refresh(
        &mut self,
        spec: ProviderPrepareSpec,
        reply: Sender<Result<ProviderStateDto, ErrorCode>>,
    ) {
        let key = ProviderPrepareKey::from(&spec);
        if let Some(pending) = self.provider_prepares.get_mut(&key) {
            pending.refresh.push(reply);
            return;
        }
        self.provider_prepares.insert(
            key.clone(),
            PendingProviderPrepare {
                spec,
                started: false,
                dispatch: Vec::new(),
                refresh: vec![reply],
            },
        );
        self.provider_prepare_backlog.push_back(key);
        self.drive_provider_prepare_workers();
    }

    fn spawn_provider_prepare(&mut self, request: QueuedProviderPrepare) {
        let key = ProviderPrepareKey::from(&request.spec);
        if let Some(pending) = self.provider_prepares.get_mut(&key) {
            pending.dispatch.push(request);
            return;
        }
        self.provider_prepares.insert(
            key.clone(),
            PendingProviderPrepare {
                spec: request.spec.clone(),
                started: false,
                dispatch: vec![request],
                refresh: Vec::new(),
            },
        );
        self.provider_prepare_backlog.push_back(key);
        self.drive_provider_prepare_workers();
    }

    fn failed_provider_preparation() -> ProviderPreparation {
        ProviderPreparation {
            credential: CredentialState::Error {
                code: ErrorCode::Internal,
            },
            projected_credential: CredentialState::Error {
                code: ErrorCode::Internal,
            },
            credential_identity: None,
            catalog: Err(ErrorCode::Internal),
        }
    }

    fn drive_provider_prepare_workers(&mut self) {
        while self.pending_provider_prepares < MAX_CONCURRENT_PROVIDER_PREPARES {
            let mut selected = None;
            let candidates = self.provider_prepare_backlog.len();
            for _ in 0..candidates {
                let Some(key) = self.provider_prepare_backlog.pop_front() else {
                    break;
                };
                let active_for_provider = self
                    .provider_prepares
                    .values()
                    .filter(|pending| {
                        pending.started
                            && pending.spec.provider == key.provider
                            && pending.spec.transport == key.transport
                    })
                    .count();
                if active_for_provider < MAX_CONCURRENT_PREPARES_PER_PROVIDER {
                    selected = Some(key);
                    break;
                }
                self.provider_prepare_backlog.push_back(key);
            }
            let Some(key) = selected else {
                break;
            };
            let remove_stale = {
                let Some(pending) = self.provider_prepares.get_mut(&key) else {
                    continue;
                };
                pending
                    .dispatch
                    .retain(|request| self.coordinator.provider_prepare_is_current(request));
                pending.dispatch.is_empty() && pending.refresh.is_empty()
            };
            if remove_stale {
                self.provider_prepares.remove(&key);
                continue;
            }
            let pending = self
                .provider_prepares
                .get_mut(&key)
                .expect("retained provider prepare exists");
            if pending.started {
                continue;
            }
            pending.started = true;
            let spec = pending.spec.clone();
            let providers = self.provider_access.clone();
            let completed = self.provider_prepare_tx.clone();
            let worker_key = key.clone();
            let spawn = std::thread::Builder::new()
                .name("corti-hosted-prepare".into())
                .spawn(move || {
                    let preparation = prepare_provider(providers.as_ref(), &spec);
                    let _ = completed.send(ProviderPrepareCompletion::Completed {
                        key: worker_key,
                        preparation,
                    });
                });
            if spawn.is_err() {
                self.finish_provider_prepare(key, Self::failed_provider_preparation());
            } else {
                self.pending_provider_prepares += 1;
            }
        }
    }

    fn drain_provider_prepares(&mut self, limit: usize) {
        for _ in 0..limit {
            let Ok(ProviderPrepareCompletion::Completed { key, preparation }) =
                self.provider_prepare_rx.try_recv()
            else {
                break;
            };
            self.pending_provider_prepares = self.pending_provider_prepares.saturating_sub(1);
            self.finish_provider_prepare(key, preparation);
        }
        self.drive_provider_prepare_workers();
    }

    fn finish_provider_prepare(
        &mut self,
        key: ProviderPrepareKey,
        preparation: ProviderPreparation,
    ) {
        let Some(pending) = self.provider_prepares.remove(&key) else {
            return;
        };
        for request in pending.dispatch {
            self.complete_provider_prepare(request, preparation.clone());
        }
        if !pending.refresh.is_empty() {
            let result = match self.scope_for(&pending.spec.provider, &pending.spec.transport) {
                Ok(current_scope) if current_scope == pending.spec.scope => self
                    .coordinator
                    .complete_provider_refresh(&pending.spec, preparation),
                _ => Err(ErrorCode::Superseded),
            };
            if !matches!(&result, Err(ErrorCode::Superseded)) {
                self.bump_state();
                self.refresh_snapshot();
            }
            for reply in pending.refresh {
                let _ = reply.send(result.clone());
            }
        }
    }

    fn complete_provider_prepare(
        &mut self,
        request: QueuedProviderPrepare,
        preparation: ProviderPreparation,
    ) {
        if let DispatchOutcome::Failed { call_id, code } = self
            .coordinator
            .complete_provider_prepare(request, preparation)
        {
            self.fail_final_call(&call_id, code);
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

    fn drain_provider_events(&mut self, limit: usize) {
        for _ in 0..limit {
            let Ok(event) = self.provider_event_rx.try_recv() else {
                break;
            };
            let _ = self.coordinator.on_provider_event(event);
        }
    }

    fn drain_workers(&mut self, limit: usize) {
        for _ in 0..limit {
            let Ok(completion) = self.worker_rx.try_recv() else {
                break;
            };
            let call_id = completion.ticket.request().call_id.clone();
            let provider = completion.ticket.request().provider.clone();
            let transport = completion.ticket.request().transport.clone();
            self.call_cache.insert(call_id.clone(), completion.cache);
            // Refresh auth projection before applying/releasing the provider result. This surfaces a
            // rotated-but-unsaved ChatGPT credential (and direct-provider rejection) before any waiter can
            // observe completion while Preferences still says Ready.
            if self
                .coordinator
                .sync_provider_credential(&provider, &transport)
                .is_ok()
            {
                self.refresh_snapshot();
            }
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
            if apply.lane == Lane::Live {
                let _ = self
                    .coordinator
                    .acknowledge_live_application(&apply.call_id, Err(ErrorCode::Superseded));
            } else {
                self.fail_final_call(&apply.call_id, ErrorCode::Superseded);
            }
            return;
        }
        match apply.lane {
            Lane::Live => {
                let result = match (
                    self.current_recording.as_deref(),
                    apply.output.rewritten_rows(),
                ) {
                    (Some(recording_id), Some(rows)) => match self.live_view.apply_hosted_rows(
                        recording_id,
                        rows,
                        apply.fence.transcript_revision,
                    ) {
                        crate::live_view::HostedRowsApplyOutcome::Applied { .. } => Ok(()),
                        crate::live_view::HostedRowsApplyOutcome::Stale => {
                            tracing::warn!(
                                target: "corti::hosted",
                                call_id = %apply.call_id,
                                "discarded a hosted Live result at the final transcript application fence"
                            );
                            Err(ErrorCode::Superseded)
                        }
                    },
                    _ => Err(ErrorCode::Superseded),
                };
                let _ = self
                    .coordinator
                    .acknowledge_live_application(&apply.call_id, result);
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
                if let Some(journal_call_id) = apply.journal_call_id {
                    group
                        .settled_call_ids
                        .insert(apply.call_id.clone(), journal_call_id);
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
        for (segment_index, row_id) in &group.row_positions {
            if let Some(text) = group.rewritten.get(row_id) {
                transcript.segments[*segment_index].text.clone_from(text);
            }
        }
        let settled_call_ids = group
            .call_ids
            .iter()
            .map(|call_id| {
                group
                    .settled_call_ids
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| call_id.clone())
            })
            .collect::<Vec<_>>();
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
        self.release_batch_session(group.live_session);
        let _ = group.reply.send(SettledFinalTranscript {
            transcript,
            applied_postprocess: applied,
            source_transcript_fingerprint: group.source_fingerprint,
            call_ids: settled_call_ids,
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
            if let Some(durable_call_id) = group.settled_call_ids.get(peer)
                && durable_call_id != peer
            {
                self.coordinator
                    .cancel_call(durable_call_id, CancellationReason::Superseded);
            }
            self.call_cache.remove(peer);
        }
        self.release_batch_session(group.live_session);
        let _ = group.reply.send(fallback_final(
            group.original,
            code,
            group.call_ids,
            group.source_fingerprint,
        ));
    }

    fn release_batch_session(&mut self, live_session: bool) {
        if !live_session {
            self.current_recording = None;
            self.ledger.clear();
            self.ledger_bytes = 0;
        }
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
            chatgpt_scope_configured(&self.coordinator),
            self.secret_presence.as_ref(),
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
    chatgpt_scope_configured: bool,
    secret_presence: &dyn Fn(SecretPurpose) -> bool,
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
            HostedProviderScopeDto {
                provider: CHATGPT_PROVIDER.to_owned(),
                transport: CHATGPT_TRANSPORT.to_owned(),
                configured: chatgpt_scope_configured,
                alias: chatgpt_scope_configured.then(|| "ChatGPT subscription".to_owned()),
                project: None,
                region: None,
                quota_project: None,
            },
            scope_dto(
                "anthropic",
                "anthropic_api",
                &values.providers.anthropic.scope,
            ),
            scope_dto("amazon", "bedrock_runtime", &values.providers.bedrock.scope),
        ],
        vertex_models: values.providers.vertex_models.clone(),
        bedrock: BedrockCredentialDto {
            mode: values.providers.bedrock.credential_mode,
            profile: values.providers.bedrock.profile.clone(),
            role_arn: values.providers.bedrock.role_arn.clone(),
            has_access_key_id: secret_presence(SecretPurpose::AwsAccessKeyId),
            has_secret_access_key: secret_presence(SecretPurpose::AwsSecretAccessKey),
            has_session_token: secret_presence(SecretPurpose::AwsSessionToken),
        },
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

fn chatgpt_scope_configured(coordinator: &PostprocessCoordinator) -> bool {
    let Ok(provider) = ProviderId::new(CHATGPT_PROVIDER) else {
        return false;
    };
    let Ok(transport) = TransportId::new(CHATGPT_TRANSPORT) else {
        return false;
    };
    coordinator
        .provider_connection_scope(&provider, &transport)
        .is_some()
}

fn initial_provider_states() -> Vec<ProviderStateDto> {
    crate::postprocess::provider_support_catalog()
        .into_iter()
        .map(|descriptor| ProviderStateDto {
            credential: if descriptor.support_tier == SupportTier::Blocked
                || !descriptor.adapter_available
            {
                CredentialState::Unsupported {
                    code: ErrorCode::PolicyBlocked,
                }
            } else {
                CredentialState::Absent
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
        "chatgpt_subscription" => CHATGPT_SUBSCRIPTION_ADAPTER_VERSION,
        "anthropic_api" => ANTHROPIC_MESSAGES_ADAPTER_VERSION,
        "bedrock_runtime" => BEDROCK_CONVERSE_ADAPTER_VERSION,
        _ => 1,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NormalizedBedrockSetup {
    mode: AwsCredentialMode,
    profile: Option<String>,
    role_arn: Option<String>,
    region: String,
    setup_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BedrockKeyPresence {
    access_key_id: bool,
    secret_access_key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BedrockValidationIssue {
    field: HostedMutationInvalidField,
    reason: HostedMutationInvalidReason,
}

fn normalize_and_validate_bedrock_setup(
    request: SaveBedrockSetupRequest,
    discovered_profiles: &[String],
    keys: BedrockKeyPresence,
) -> Result<NormalizedBedrockSetup, BedrockValidationIssue> {
    let uses_profile = matches!(
        request.mode,
        AwsCredentialMode::Profile | AwsCredentialMode::Sso | AwsCredentialMode::AssumeRole
    );
    let profile = uses_profile
        .then_some(request.profile)
        .flatten()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let role_arn = (request.mode == AwsCredentialMode::AssumeRole)
        .then_some(request.role_arn)
        .flatten()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let region = request.region.trim().to_owned();
    let setup_name = request.setup_name.trim().to_owned();

    if matches!(
        request.mode,
        AwsCredentialMode::Profile | AwsCredentialMode::Sso
    ) && profile.is_none()
    {
        return Err(BedrockValidationIssue {
            field: HostedMutationInvalidField::Profile,
            reason: HostedMutationInvalidReason::Required,
        });
    }
    if let Some(profile) = profile.as_deref() {
        if profile.len() > 256
            || profile.contains(['[', ']'])
            || profile.chars().any(char::is_control)
        {
            return Err(BedrockValidationIssue {
                field: HostedMutationInvalidField::Profile,
                reason: HostedMutationInvalidReason::Invalid,
            });
        }
        if !discovered_profiles
            .iter()
            .any(|candidate| candidate == profile)
        {
            return Err(BedrockValidationIssue {
                field: HostedMutationInvalidField::Profile,
                reason: HostedMutationInvalidReason::NotFound,
            });
        }
    }
    if request.mode == AwsCredentialMode::AssumeRole {
        let Some(role_arn) = role_arn.as_deref() else {
            return Err(BedrockValidationIssue {
                field: HostedMutationInvalidField::RoleArn,
                reason: HostedMutationInvalidReason::Required,
            });
        };
        if role_arn.len() > 1024 || !valid_iam_role_arn(role_arn) {
            return Err(BedrockValidationIssue {
                field: HostedMutationInvalidField::RoleArn,
                reason: HostedMutationInvalidReason::Invalid,
            });
        }
    }
    if request.mode == AwsCredentialMode::StaticKeychain
        && (!keys.access_key_id || !keys.secret_access_key)
    {
        return Err(BedrockValidationIssue {
            field: HostedMutationInvalidField::KeyPair,
            reason: HostedMutationInvalidReason::KeysMissing,
        });
    }
    if region.is_empty() {
        return Err(BedrockValidationIssue {
            field: HostedMutationInvalidField::Region,
            reason: HostedMutationInvalidReason::Required,
        });
    }
    if !valid_aws_region(&region) {
        return Err(BedrockValidationIssue {
            field: HostedMutationInvalidField::Region,
            reason: HostedMutationInvalidReason::Invalid,
        });
    }
    if setup_name.is_empty() {
        return Err(BedrockValidationIssue {
            field: HostedMutationInvalidField::SetupName,
            reason: HostedMutationInvalidReason::Required,
        });
    }
    if setup_name.len() > 1024 || setup_name.chars().any(char::is_control) {
        return Err(BedrockValidationIssue {
            field: HostedMutationInvalidField::SetupName,
            reason: HostedMutationInvalidReason::Invalid,
        });
    }

    Ok(NormalizedBedrockSetup {
        mode: request.mode,
        profile,
        role_arn,
        region,
        setup_name,
    })
}

fn valid_aws_region(region: &str) -> bool {
    let bytes = region.as_bytes();
    (3..=64).contains(&bytes.len())
        && bytes
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn valid_iam_role_arn(arn: &str) -> bool {
    let mut fields = arn.splitn(6, ':');
    let (Some(prefix), Some(partition), Some(service), Some(region), Some(account), Some(resource)) = (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) else {
        return false;
    };
    let valid_partition = matches!(
        partition,
        "aws" | "aws-us-gov" | "aws-cn" | "aws-iso" | "aws-iso-b"
    );
    let role = resource.strip_prefix("role/").unwrap_or_default();
    prefix == "arn"
        && valid_partition
        && service == "iam"
        && region.is_empty()
        && account.len() == 12
        && account.bytes().all(|byte| byte.is_ascii_digit())
        && !role.is_empty()
        && role.len() <= 512
        && role.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'+' | b'=' | b',' | b'.' | b'@' | b'_' | b'/' | b'-')
        })
}

fn new_connection_scope_id() -> Result<ConnectionScopeId, ErrorCode> {
    let mut random = [0u8; 32];
    random_bytes(&mut random).map_err(|_| ErrorCode::Internal)?;
    let mut encoded = String::with_capacity(9 + random.len() * 2);
    encoded.push_str("scope-v3-");
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in random {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    ConnectionScopeId::new(encoded).map_err(|_| ErrorCode::Internal)
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

fn transcript_rows(transcript: &DiarizedTranscript) -> Vec<(usize, TranscriptRow)> {
    transcript
        .segments
        .iter()
        .enumerate()
        .filter_map(|(index, segment)| {
            let text = segment.text.trim();
            if text.is_empty() {
                return None;
            }
            Some((
                index,
                TranscriptRow {
                    row_id: RowId::new(format!("final-row-{index:08}"))
                        .expect("segment-index final row id is valid"),
                    speaker: segment.speaker.display().to_owned(),
                    start_ms: seconds_to_millis(segment.start),
                    end_ms: seconds_to_millis(segment.end).max(seconds_to_millis(segment.start)),
                    text: text.to_owned(),
                },
            ))
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

fn final_chunks(
    rows: &[TranscriptRow],
    input_token_budget: u64,
) -> Result<Vec<FinalChunk>, ErrorCode> {
    if rows.is_empty() {
        return Ok(vec![(Vec::new(), Vec::new())]);
    }
    if input_token_budget == 0 {
        return Err(ErrorCode::PolicyBlocked);
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    while start < rows.len() {
        let mut end = start;
        let mut tokens = 0u64;
        while end < rows.len() {
            let row_tokens = estimated_row_tokens(&rows[end]);
            if row_tokens > input_token_budget
                || (end > start && tokens.saturating_add(row_tokens) > input_token_budget)
            {
                break;
            }
            tokens = tokens.saturating_add(row_tokens);
            end += 1;
        }
        // A row is an indivisible identity. Never truncate its text or silently discard a remainder.
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
            let remaining = input_token_budget.saturating_sub(
                rows[start..end]
                    .iter()
                    .map(estimated_row_tokens)
                    .fold(0u64, u64::saturating_add),
            );
            let mut neighbors = Vec::new();
            neighbors.extend_from_slice(&rows[start.saturating_sub(2)..start]);
            neighbors.extend_from_slice(&rows[end..rows.len().min(end.saturating_add(2))]);
            let context = bounded_rows_from_end(&neighbors, remaining).0;
            (rows[start..end].to_vec(), context)
        })
        .collect())
}

fn estimated_row_tokens(row: &TranscriptRow) -> u64 {
    // Conservative provider-neutral upper bound: at most one token per UTF-8 byte plus fixed JSON/identity
    // overhead. Exact tokenizers remain adapter-owned; this boundary never claims omitted context was sent.
    let bytes = row
        .text
        .len()
        .saturating_add(row.speaker.len())
        .saturating_add(row.row_id.as_str().len());
    u64::try_from(bytes).unwrap_or(u64::MAX).saturating_add(24)
}

fn bounded_rows_from_end(rows: &[TranscriptRow], token_budget: u64) -> (Vec<TranscriptRow>, bool) {
    let mut tokens = 0u64;
    let mut start = rows.len();
    while start > 0 {
        let next = estimated_row_tokens(&rows[start - 1]);
        if next > token_budget || tokens.saturating_add(next) > token_budget {
            break;
        }
        tokens = tokens.saturating_add(next);
        start -= 1;
    }
    (rows[start..].to_vec(), start > 0)
}

fn bounded_question_context(
    rows: &[TranscriptRow],
    input_token_budget: u64,
) -> (Vec<TranscriptRow>, bool) {
    bounded_rows_from_end(rows, input_token_budget)
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
        | ControlError::ProviderBlocked => ErrorCode::PolicyBlocked,
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
    use std::collections::VecDeque;
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize};

    use corti_core::{OwningApp, RecordingMeta, Speaker, TranscriptSegment};
    use corti_postprocess::{
        AdapterCapabilities, CredentialSourceKind, EventContext, LatencyFields, ModelDescriptor,
        NormalizedUsage, PromptTask, ProviderCacheKey, ProviderCacheKeyMaterial, ProviderEvent,
        ProviderEventKind, ProviderOutput, QuestionOutput, QuestionTerminal, Replacement,
        RewriteOutput,
    };
    use corti_postprocess_providers::{
        HttpRequest, HttpResponse, HttpResponseBody, TransportError, VertexPublisher,
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
                    benchmarked_for_live: false,
                },
                descriptor,
            }
        }
    }

    struct MutatingCatalogProviders {
        fixture: FixtureProviders,
        credential: Arc<Mutex<CredentialState>>,
        mutate_on_catalog: bool,
    }

    impl ProviderAccess for MutatingCatalogProviders {
        fn descriptor(
            &self,
            provider: &ProviderId,
            transport: &TransportId,
        ) -> Option<ProviderDescriptor> {
            (&self.fixture.descriptor.provider == provider
                && &self.fixture.descriptor.transport == transport)
                .then(|| self.fixture.descriptor.clone())
        }

        fn credential_state(
            &self,
            provider: &ProviderId,
            transport: &TransportId,
        ) -> CredentialState {
            if &self.fixture.descriptor.provider == provider
                && &self.fixture.descriptor.transport == transport
            {
                self.credential.lock().unwrap().clone()
            } else {
                CredentialState::Unsupported {
                    code: ErrorCode::PolicyBlocked,
                }
            }
        }

        fn catalog(
            &self,
            provider: &ProviderId,
            transport: &TransportId,
            _scope: &ProviderScope,
        ) -> Result<ModelCatalog, corti_postprocess::PostprocessError> {
            if &self.fixture.descriptor.provider != provider
                || &self.fixture.descriptor.transport != transport
            {
                return Err(ErrorCode::PolicyBlocked.into());
            }
            if self.mutate_on_catalog {
                *self.credential.lock().unwrap() = CredentialState::Error {
                    code: ErrorCode::Cache,
                };
            }
            Ok(ModelCatalog {
                models: vec![self.fixture.model.clone()],
            })
        }
    }

    struct BlockingPrepareProviders {
        started: Arc<(Mutex<bool>, Condvar)>,
        released: Arc<(Mutex<bool>, Condvar)>,
        resolved: AtomicBool,
        resolve_calls: AtomicUsize,
        descriptor: ProviderDescriptor,
        model: ModelDescriptor,
    }

    impl BlockingPrepareProviders {
        fn bedrock() -> Arc<Self> {
            let fixture =
                FixtureProviders::for_transport(corti_postprocess::KnownTransport::BedrockRuntime);
            Arc::new(Self {
                started: Arc::new((Mutex::new(false), Condvar::new())),
                released: Arc::new((Mutex::new(false), Condvar::new())),
                resolved: AtomicBool::new(false),
                resolve_calls: AtomicUsize::new(0),
                descriptor: fixture.descriptor,
                model: fixture.model,
            })
        }

        fn wait_started(&self) {
            let (lock, wake) = self.started.as_ref();
            let started = lock.lock().unwrap();
            let (started, timeout) = wake
                .wait_timeout_while(started, Duration::from_secs(2), |started| !*started)
                .unwrap();
            assert!(
                *started && !timeout.timed_out(),
                "prepare worker did not start"
            );
        }

        fn release(&self) {
            let (lock, wake) = self.released.as_ref();
            *lock.lock().unwrap() = true;
            wake.notify_all();
        }
    }

    impl ProviderAccess for Arc<BlockingPrepareProviders> {
        fn descriptor(
            &self,
            provider: &ProviderId,
            transport: &TransportId,
        ) -> Option<ProviderDescriptor> {
            (&self.descriptor.provider == provider && &self.descriptor.transport == transport)
                .then(|| self.descriptor.clone())
        }

        fn credential_state(
            &self,
            provider: &ProviderId,
            transport: &TransportId,
        ) -> CredentialState {
            if &self.descriptor.provider != provider || &self.descriptor.transport != transport {
                return CredentialState::Unsupported {
                    code: ErrorCode::PolicyBlocked,
                };
            }
            if self.resolved.load(Ordering::Acquire) {
                CredentialState::Ready {
                    expires_at_unix_ms: None,
                    source: CredentialSourceKind::AwsProfile,
                }
            } else {
                CredentialState::Absent
            }
        }

        fn resolve_credential(
            &self,
            _provider: &ProviderId,
            _transport: &TransportId,
        ) -> CredentialState {
            self.resolve_calls.fetch_add(1, Ordering::AcqRel);
            let (started, wake) = self.started.as_ref();
            *started.lock().unwrap() = true;
            wake.notify_all();
            let (released, wake) = self.released.as_ref();
            let mut released = released.lock().unwrap();
            while !*released {
                released = wake.wait(released).unwrap();
            }
            self.resolved.store(true, Ordering::Release);
            CredentialState::Ready {
                expires_at_unix_ms: None,
                source: CredentialSourceKind::AwsProfile,
            }
        }

        fn catalog(
            &self,
            provider: &ProviderId,
            transport: &TransportId,
            scope: &ProviderScope,
        ) -> Result<ModelCatalog, corti_postprocess::PostprocessError> {
            if &self.descriptor.provider != provider || &self.descriptor.transport != transport {
                return Err(ErrorCode::PolicyBlocked.into());
            }
            let mut model = self.model.clone();
            model.region.clone_from(&scope.region);
            Ok(ModelCatalog {
                models: vec![model],
            })
        }
    }

    impl ProviderAccess for FixtureProviders {
        fn descriptor(
            &self,
            provider: &ProviderId,
            transport: &TransportId,
        ) -> Option<ProviderDescriptor> {
            (&self.descriptor.provider == provider && &self.descriptor.transport == transport)
                .then(|| self.descriptor.clone())
        }

        fn credential_state(
            &self,
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
            &self,
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

    #[derive(Clone)]
    struct InjectedTransportFactory {
        openai: Arc<Mutex<VecDeque<HttpResponse>>>,
        sends: Arc<AtomicUsize>,
    }

    impl DirectTransportFactory for InjectedTransportFactory {
        fn openai(&self) -> Box<dyn HttpTransport> {
            Box::new(InjectedTransport {
                responses: self.openai.clone(),
                sends: self.sends.clone(),
            })
        }

        fn chatgpt(&self) -> Box<dyn HttpTransport> {
            Box::new(InjectedTransport {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                sends: self.sends.clone(),
            })
        }

        fn anthropic(&self) -> Box<dyn HttpTransport> {
            Box::new(InjectedTransport {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                sends: self.sends.clone(),
            })
        }

        fn bedrock(&self) -> Box<dyn HttpTransport> {
            Box::new(InjectedTransport {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                sends: self.sends.clone(),
            })
        }

        fn vertex(&self) -> Box<dyn HttpTransport> {
            Box::new(InjectedTransport {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                sends: self.sends.clone(),
            })
        }
    }

    struct InjectedTransport {
        responses: Arc<Mutex<VecDeque<HttpResponse>>>,
        sends: Arc<AtomicUsize>,
    }

    impl HttpTransport for InjectedTransport {
        fn send(
            &mut self,
            _request: &HttpRequest,
            _cancel: &corti_postprocess::CancellationToken,
        ) -> Result<HttpResponse, TransportError> {
            self.sends.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected approved-factory HTTP request"))
        }
    }

    struct FixtureBody(VecDeque<Vec<u8>>);

    impl HttpResponseBody for FixtureBody {
        fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            Ok(self.0.pop_front())
        }
    }

    struct EmptyChatGptStore;

    impl ChatGptCredentialStore for EmptyChatGptStore {
        fn load(&self) -> Result<Option<Vec<u8>>, ChatGptStoreError> {
            Ok(None)
        }

        fn save(&self, _document: &[u8]) -> Result<(), ChatGptStoreError> {
            Ok(())
        }

        fn clear(&self) -> Result<(), ChatGptStoreError> {
            Ok(())
        }
    }

    fn empty_chatgpt_auth() -> ChatGptSubscriptionAuth {
        ChatGptSubscriptionAuth::new(
            Box::new(InjectedTransport {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                sends: Arc::new(AtomicUsize::new(0)),
            }),
            Arc::new(ProviderSystemClock::new()),
            Arc::new(EmptyChatGptStore),
        )
    }

    struct InjectedSecrets;

    impl DirectSecretStore for InjectedSecrets {
        fn read(&self, _account: &str) -> Result<Option<Vec<u8>>, CredentialError> {
            Ok(Some(b"synthetic-injected-api-key".to_vec()))
        }
    }

    fn fixture_http_response(
        content_type: &str,
        chunks: impl IntoIterator<Item = Vec<u8>>,
    ) -> HttpResponse {
        HttpResponse::new(
            200,
            [("content-type".to_string(), content_type.to_string())],
            Box::new(FixtureBody(chunks.into_iter().collect())),
        )
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
                let citation = ticket
                    .request()
                    .context
                    .first()
                    .map(|row| row.row_id.clone());
                ProviderOutput::Question(QuestionTerminal {
                    output: QuestionOutput {
                        schema: 1,
                        answer: citation.as_ref().map_or_else(
                            || corti_postprocess::EXPLICIT_NO_ANSWER.to_string(),
                            |_| "fixture grounded answer".to_string(),
                        ),
                        cited_row_ids: citation.into_iter().collect(),
                        context_truncated: ticket
                            .request()
                            .prompt
                            .messages()
                            .last()
                            .and_then(|message| {
                                serde_json::from_str::<serde_json::Value>(message.content()).ok()
                            })
                            .and_then(|value| value["context_truncated"].as_bool())
                            .unwrap_or(false),
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

    struct CredentialMutatingExecutor {
        credential: Arc<Mutex<CredentialState>>,
    }

    impl TicketExecutor for CredentialMutatingExecutor {
        fn execute(
            &self,
            ticket: &DispatchTicket,
            sink: &dyn ProviderEventSink,
        ) -> Result<ProviderTerminal, corti_postprocess::PostprocessError> {
            *self.credential.lock().unwrap() = CredentialState::Error {
                code: ErrorCode::Cache,
            };
            RewriteExecutor.execute(ticket, sink)
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

    fn shared(preferences: HostedPreferences) -> Arc<Mutex<HostedPreferences>> {
        Arc::new(Mutex::new(preferences))
    }

    fn bedrock_request(mode: AwsCredentialMode) -> SaveBedrockSetupRequest {
        SaveBedrockSetupRequest {
            observed_state_revision: 1,
            mode,
            profile: matches!(
                mode,
                AwsCredentialMode::Profile | AwsCredentialMode::Sso | AwsCredentialMode::AssumeRole
            )
            .then(|| "fixture-profile".to_owned()),
            role_arn: (mode == AwsCredentialMode::AssumeRole)
                .then(|| "arn:aws:iam::123456789012:role/fixture".to_owned()),
            region: "us-east-1".into(),
            setup_name: "Fixture Bedrock".into(),
        }
    }

    #[test]
    fn bedrock_setup_validation_covers_every_mode_and_normalizes_hidden_fields() {
        let profiles = vec!["fixture-profile".to_owned()];
        let keys = BedrockKeyPresence {
            access_key_id: true,
            secret_access_key: true,
        };
        for mode in [
            AwsCredentialMode::DefaultChain,
            AwsCredentialMode::Profile,
            AwsCredentialMode::StaticKeychain,
            AwsCredentialMode::AssumeRole,
            AwsCredentialMode::Sso,
        ] {
            let setup =
                normalize_and_validate_bedrock_setup(bedrock_request(mode), &profiles, keys)
                    .unwrap_or_else(|issue| panic!("{mode:?} was rejected: {issue:?}"));
            assert_eq!(setup.mode, mode);
            if matches!(
                mode,
                AwsCredentialMode::DefaultChain | AwsCredentialMode::StaticKeychain
            ) {
                assert_eq!(setup.profile, None);
            }
            if mode != AwsCredentialMode::AssumeRole {
                assert_eq!(setup.role_arn, None);
            }
        }
    }

    #[test]
    fn bedrock_setup_validation_returns_precise_field_reasons() {
        let profiles = vec!["fixture-profile".to_owned()];
        let keys = BedrockKeyPresence {
            access_key_id: true,
            secret_access_key: true,
        };
        let issue =
            |request| normalize_and_validate_bedrock_setup(request, &profiles, keys).unwrap_err();

        let mut missing_profile = bedrock_request(AwsCredentialMode::Profile);
        missing_profile.profile = None;
        assert_eq!(
            issue(missing_profile),
            BedrockValidationIssue {
                field: HostedMutationInvalidField::Profile,
                reason: HostedMutationInvalidReason::Required,
            }
        );
        let mut stale_profile = bedrock_request(AwsCredentialMode::Sso);
        stale_profile.profile = Some("removed-profile".into());
        assert_eq!(
            issue(stale_profile).reason,
            HostedMutationInvalidReason::NotFound
        );
        let mut bad_role = bedrock_request(AwsCredentialMode::AssumeRole);
        bad_role.role_arn = Some("arn:aws:iam::123:role/nope".into());
        assert_eq!(
            issue(bad_role),
            BedrockValidationIssue {
                field: HostedMutationInvalidField::RoleArn,
                reason: HostedMutationInvalidReason::Invalid,
            }
        );
        let missing_keys = normalize_and_validate_bedrock_setup(
            bedrock_request(AwsCredentialMode::StaticKeychain),
            &profiles,
            BedrockKeyPresence {
                access_key_id: true,
                secret_access_key: false,
            },
        )
        .unwrap_err();
        assert_eq!(missing_keys.field, HostedMutationInvalidField::KeyPair);
        assert_eq!(
            missing_keys.reason,
            HostedMutationInvalidReason::KeysMissing
        );

        let mut missing_region = bedrock_request(AwsCredentialMode::DefaultChain);
        missing_region.region = "  ".into();
        assert_eq!(
            issue(missing_region).reason,
            HostedMutationInvalidReason::Required
        );
        let mut bad_region = bedrock_request(AwsCredentialMode::DefaultChain);
        bad_region.region = "US East 1".into();
        assert_eq!(
            issue(bad_region).reason,
            HostedMutationInvalidReason::Invalid
        );
        let mut missing_name = bedrock_request(AwsCredentialMode::DefaultChain);
        missing_name.setup_name = "\t".into();
        assert_eq!(
            issue(missing_name).field,
            HostedMutationInvalidField::SetupName
        );
        let mut long_name = bedrock_request(AwsCredentialMode::DefaultChain);
        long_name.setup_name = "x".repeat(1025);
        assert_eq!(
            issue(long_name).reason,
            HostedMutationInvalidReason::Invalid
        );
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
            shared(configured_preferences()),
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
            None,
            None,
        )
        .unwrap();
        (handle, pipeline_rx, path)
    }

    fn start_settings_fixture(
        name: &str,
        preferences: HostedPreferences,
        secret_presence: SecretPresenceSource,
    ) -> (HostedHandle, Arc<Mutex<HostedPreferences>>, PathBuf) {
        let path = dir(name);
        let outbox = Arc::new(TelemetryOutbox::open(path.join("postprocess-outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let preferences = shared(preferences);
        let (_, handle) = start_with_components(
            preferences.clone(),
            WordBankDocument::empty(),
            LiveTranscriptStore::detached(),
            pipeline_tx,
            outbox,
            Arc::new(DenyExecutor),
            Box::new(UnavailableProviders),
            Arc::new(NoPricing),
            Arc::new(UnarmedVertex),
            Arc::new(|_| {}),
            DigestKey::new([17; 32]),
            ProcessEpoch(117),
            false,
            None,
            None,
            Some(secret_presence),
        )
        .unwrap();
        (handle, preferences, path)
    }

    fn save_bedrock_for_test(
        handle: &HostedHandle,
        request: SaveBedrockSetupRequest,
    ) -> HostedMutationResult {
        let (reply, receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::SaveBedrockSetup { request, reply })
            .unwrap();
        receive.recv().unwrap().unwrap()
    }

    #[test]
    fn bedrock_setup_save_is_atomic_revision_checked_and_uses_opaque_identity_semantics() {
        let (handle, preferences, path) = start_settings_fixture(
            "bedrock-atomic",
            HostedPreferences::default(),
            Arc::new(|purpose| {
                matches!(
                    purpose,
                    SecretPurpose::AwsAccessKeyId | SecretPurpose::AwsSecretAccessKey
                )
            }),
        );
        let first = save_bedrock_for_test(
            &handle,
            SaveBedrockSetupRequest {
                observed_state_revision: handle.snapshot().state_revision,
                ..bedrock_request(AwsCredentialMode::DefaultChain)
            },
        );
        let HostedMutationResult::Applied { settings } = first else {
            panic!("first complete setup was not applied")
        };
        let scope = settings
            .scopes
            .iter()
            .find(|scope| scope.provider == "amazon")
            .unwrap();
        assert!(scope.configured);
        assert_eq!(scope.alias.as_deref(), Some("Fixture Bedrock"));
        assert_eq!(scope.region.as_deref(), Some("us-east-1"));
        assert!(settings.bedrock.has_access_key_id);
        assert!(settings.bedrock.has_secret_access_key);
        let first_id = preferences
            .lock()
            .unwrap()
            .values()
            .providers
            .bedrock
            .scope
            .connection_scope_id
            .clone()
            .unwrap();
        assert!(first_id.as_str().starts_with("scope-v3-"));

        let before_invalid = preferences.lock().unwrap().clone();
        let invalid = save_bedrock_for_test(
            &handle,
            SaveBedrockSetupRequest {
                observed_state_revision: settings.state_revision,
                setup_name: " ".into(),
                ..bedrock_request(AwsCredentialMode::DefaultChain)
            },
        );
        assert!(matches!(
            invalid,
            HostedMutationResult::Invalid {
                field: HostedMutationInvalidField::SetupName,
                reason: HostedMutationInvalidReason::Required,
                ..
            }
        ));
        assert_eq!(*preferences.lock().unwrap(), before_invalid);

        let control_revision_before_rename = handle.snapshot().control.control_revision;
        let renamed = save_bedrock_for_test(
            &handle,
            SaveBedrockSetupRequest {
                observed_state_revision: handle.snapshot().state_revision,
                setup_name: "Renamed display only".into(),
                ..bedrock_request(AwsCredentialMode::DefaultChain)
            },
        );
        let HostedMutationResult::Applied {
            settings: renamed_settings,
        } = renamed
        else {
            panic!("display-only rename was not applied")
        };
        assert_eq!(
            renamed_settings.control.control_revision, control_revision_before_rename,
            "display-only rename must not fence active lanes"
        );
        let renamed_id = preferences
            .lock()
            .unwrap()
            .values()
            .providers
            .bedrock
            .scope
            .connection_scope_id
            .clone()
            .unwrap();
        assert_eq!(renamed_id, first_id);

        let rotated = save_bedrock_for_test(
            &handle,
            SaveBedrockSetupRequest {
                observed_state_revision: handle.snapshot().state_revision,
                region: "eu-west-1".into(),
                setup_name: "Renamed display only".into(),
                ..bedrock_request(AwsCredentialMode::DefaultChain)
            },
        );
        assert!(matches!(rotated, HostedMutationResult::Applied { .. }));
        let rotated_id = preferences
            .lock()
            .unwrap()
            .values()
            .providers
            .bedrock
            .scope
            .connection_scope_id
            .clone()
            .unwrap();
        assert_ne!(rotated_id, first_id);

        let stale = save_bedrock_for_test(
            &handle,
            SaveBedrockSetupRequest {
                observed_state_revision: settings.state_revision,
                ..bedrock_request(AwsCredentialMode::DefaultChain)
            },
        );
        assert!(matches!(stale, HostedMutationResult::Conflict { .. }));

        let (reply, receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::ClearBedrockSetup {
                request: ClearBedrockSetupRequest {
                    observed_state_revision: handle.snapshot().state_revision,
                },
                reply,
            })
            .unwrap();
        let cleared = receive.recv().unwrap().unwrap();
        let HostedMutationResult::Applied { settings } = cleared else {
            panic!("clear was not applied")
        };
        assert!(
            preferences
                .lock()
                .unwrap()
                .values()
                .providers
                .bedrock
                .scope
                .connection_scope_id
                .is_none()
        );
        // Clearing non-secret setup does not silently delete the independently-managed key slots.
        assert!(settings.bedrock.has_access_key_id);
        assert!(settings.bedrock.has_secret_access_key);
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn bedrock_secret_add_replace_remove_refreshes_projection_and_rotates_identity() {
        let access = Arc::new(AtomicBool::new(false));
        let secret = Arc::new(AtomicBool::new(false));
        let access_source = access.clone();
        let secret_source = secret.clone();
        let presence: SecretPresenceSource = Arc::new(move |purpose| match purpose {
            SecretPurpose::AwsAccessKeyId => access_source.load(Ordering::Acquire),
            SecretPurpose::AwsSecretAccessKey => secret_source.load(Ordering::Acquire),
            _ => false,
        });
        let preferences = HostedPreferences::default()
            .revise(|values| {
                let bedrock = &mut values.providers.bedrock;
                bedrock.credential_mode = AwsCredentialMode::StaticKeychain;
                bedrock.scope.connection_scope_id =
                    Some(ConnectionScopeId::new("fixture-static-scope").unwrap());
                bedrock.scope.alias = Some("Static fixture".into());
                bedrock.scope.region = Some("us-east-1".into());
            })
            .unwrap();
        let (handle, preferences, path) =
            start_settings_fixture("bedrock-secret-projection", preferences, presence);
        let mutate = |request| {
            let (reply, receive) = std::sync::mpsc::channel();
            handle
                .send(ServiceCommand::ProviderSecretChanged { request, reply })
                .unwrap();
            receive
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap();
        };
        let identity = || {
            preferences
                .lock()
                .unwrap()
                .values()
                .providers
                .bedrock
                .scope
                .connection_scope_id
                .clone()
                .unwrap()
        };

        let initial = identity();
        access.store(true, Ordering::Release);
        mutate(SecretSlotRequest::Aws {
            slot: AwsKeySlotDto::AccessKeyId,
        });
        let after_add = identity();
        assert_ne!(after_add, initial);
        assert!(handle.snapshot().bedrock.has_access_key_id);
        assert!(!handle.snapshot().bedrock.has_secret_access_key);

        // A secure replacement has the same presence projection but still changes credential generation and
        // technical account fencing.
        mutate(SecretSlotRequest::Aws {
            slot: AwsKeySlotDto::AccessKeyId,
        });
        let after_replace = identity();
        assert_ne!(after_replace, after_add);
        assert!(handle.snapshot().bedrock.has_access_key_id);

        secret.store(true, Ordering::Release);
        mutate(SecretSlotRequest::Aws {
            slot: AwsKeySlotDto::SecretAccessKey,
        });
        assert!(handle.snapshot().bedrock.has_secret_access_key);
        access.store(false, Ordering::Release);
        mutate(SecretSlotRequest::Aws {
            slot: AwsKeySlotDto::AccessKeyId,
        });
        assert!(!handle.snapshot().bedrock.has_access_key_id);
        assert!(handle.snapshot().bedrock.has_secret_access_key);
        std::fs::remove_dir_all(path).ok();
    }

    fn fixture_request_key() -> RequestKey {
        let provider = ProviderId::new("openai").unwrap();
        let transport = TransportId::new("openai_api").unwrap();
        let scope = ConnectionScopeId::new("fixture-scope").unwrap();
        let model = ModelId::new("fixture-model").unwrap();
        let targets = [TranscriptRow {
            row_id: RowId::new("fixture-boundary-row").unwrap(),
            speaker: "Fixture".into(),
            start_ms: 0,
            end_ms: 1,
            text: "fixture sensitive row".into(),
        }];
        RequestKey::derive(
            &DigestKey::new([91; 32]),
            &RequestKeyMaterial {
                provider: &provider,
                transport: &transport,
                support_tier: SupportTier::Documented,
                connection_scope_id: &scope,
                region: None,
                exact_model_id: &model,
                adapter_version: 1,
                prompt_template_version: PROMPT_TEMPLATE_VERSION,
                output_schema_version: OUTPUT_SCHEMA_VERSION,
                chunker_version: 1,
                lane: Lane::Final,
                billing_basis: BillingBasis::MeteredEstimate,
                cache_policy: CachePolicy {
                    local: LocalCacheMode::Reusable,
                    provider: ProviderCacheMode::Off,
                },
                word_bank_canonical_digest: "fixture-bank",
                effective_steering: "",
                targets: &targets,
                context: &[],
                context_truncated: false,
                question: None,
            },
        )
    }

    fn fixture_boundary(call: &str, group: &str) -> FinalJournalBoundary {
        FinalJournalBoundary {
            recording_id: "fixture-sensitive-recording".into(),
            request_group_id: RequestGroupId::new(group).unwrap(),
            call_id: CallId::new(call).unwrap(),
            request_key: fixture_request_key(),
            fence: RequestFence {
                process_epoch: ProcessEpoch(1),
                session_generation: 1,
                transcript_revision: 1,
                control_revision: 1,
                lane_revision: 1,
                steering_revision: 1,
                bank_revision: 1,
                question_revision: None,
            },
        }
    }

    fn fixture_store_outputs() -> (ProviderOutput, crate::postprocess::ValidatedOutput) {
        let row = TranscriptRow {
            row_id: RowId::new("fixture-boundary-row").unwrap(),
            speaker: "Fixture".into(),
            start_ms: 0,
            end_ms: 1,
            text: "fixture corrected text".into(),
        };
        (
            ProviderOutput::Rewrite(RewriteOutput {
                schema: 1,
                replacements: vec![Replacement {
                    row_id: row.row_id.clone(),
                    text: row.text.clone(),
                }],
            }),
            crate::postprocess::ValidatedOutput::Rewrite { rows: vec![row] },
        )
    }

    fn fixture_telemetry(call_id: CallId, lane: Lane) -> TerminalTelemetryDto {
        TerminalTelemetryDto {
            call_id,
            recording_id: "fixture-recording".into(),
            request_group_id: RequestGroupId::new("fixture-telemetry-group").unwrap(),
            target_id: None,
            lane,
            attempt_no: 1,
            fence: RequestFence {
                process_epoch: ProcessEpoch(1),
                session_generation: 1,
                transcript_revision: 1,
                control_revision: 1,
                lane_revision: 1,
                steering_revision: 1,
                bank_revision: 1,
                question_revision: None,
            },
            provider: ProviderId::new("openai").unwrap(),
            transport: TransportId::new("openai_api").unwrap(),
            model: ModelId::new("fixture-model").unwrap(),
            support_tier: SupportTier::Documented,
            adapter_version: 1,
            prompt_version: PROMPT_TEMPLATE_VERSION,
            output_schema_version: OUTPUT_SCHEMA_VERSION,
            outcome: TerminalOutcomeDto::Completed,
            error: None,
            provider_request_sent: true,
            late_content_discarded: false,
            cache: CacheObservation::None,
            usage: NormalizedUsage::unknown(),
            cost: CostEstimate::no_provider_request(),
            latency: LatencyFields::default(),
            queued_at_unix_ms: 1_800_000_000_000,
            dispatched_at_unix_ms: Some(1_800_000_000_001),
            completed_at_unix_ms: 1_800_000_000_002,
        }
    }

    fn commit_fixture_output(
        store: &mut RuntimeStore,
        key: RequestKey,
        mode: LocalCacheMode,
        lane: Lane,
        boundary: Option<&FinalJournalBoundary>,
    ) -> Result<(), ErrorCode> {
        let (cache_output, output) = fixture_store_outputs();
        let telemetry = fixture_telemetry(
            boundary.map_or_else(
                || CallId::new("fixture-cache-call").unwrap(),
                |boundary| boundary.call_id.clone(),
            ),
            lane,
        );
        store.commit_validated(StoreCommit {
            request_key: key,
            lane,
            local_cache_mode: mode,
            cache_output: &cache_output,
            output: &output,
            final_boundary: boundary,
            telemetry: &telemetry,
        })
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
    fn hosted_command_surfaces_deny_cross_window_content_and_controls() {
        assert!(hosted_window_allowed("live", &["live", "settings"]));
        assert!(hosted_window_allowed("settings", &["live", "settings"]));
        assert!(hosted_window_allowed("settings", &["settings"]));
        for denied in ["queue", "console", "how", "ethics"] {
            assert!(!hosted_window_allowed(denied, &["live", "settings"]));
            assert!(!hosted_window_allowed(denied, &["live"]));
        }
        assert!(!hosted_window_allowed("live", &["settings"]));
        assert!(!hosted_window_allowed("settings", &["live"]));
    }

    #[test]
    fn explicitly_approved_factory_starts_and_dispatches_only_through_injected_transport() {
        let path = dir("approved-factory-startup");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("postprocess-outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let model = corti_postprocess_providers::OPENAI_LUNA_MODEL_ID;
        let preferences = configured_preferences()
            .revise(|values| {
                values.final_lane.model = Some(ModelId::new(model).unwrap());
                values.questions.model = Some(ModelId::new(model).unwrap());
            })
            .unwrap();
        let catalog = format!(
            r#"{{"object":"list","data":[{{"id":"{model}","object":"model","shutdown_date":null}}]}}"#
        );
        let output = r#"{"schema":1,"replacements":[{"row_id":"final-row-00000000","text":"factory corrected text"}]}"#;
        let stream = format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":{delta}}}\n\n\
             event: response.output_text.done\ndata: {{\"type\":\"response.output_text.done\",\"text\":{delta}}}\n\n\
             event: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"model\":\"{model}\",\"status\":\"completed\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":5}}}}}}\n\n",
            delta = serde_json::to_string(output).unwrap(),
        );
        let responses = Arc::new(Mutex::new(VecDeque::from([
            fixture_http_response("application/json", [catalog.into_bytes()]),
            fixture_http_response("text/event-stream", [stream.into_bytes()]),
        ])));
        let sends = Arc::new(AtomicUsize::new(0));
        let transports = Arc::new(InjectedTransportFactory {
            openai: responses,
            sends: sends.clone(),
        });
        let preferences = shared(preferences);
        let (executor, providers) = approved_direct_components(
            ProductionApproval::for_test(),
            transports,
            Arc::new(InjectedSecrets),
            empty_chatgpt_auth(),
            BedrockCredentialResolver::new(bedrock_config_source(preferences.clone())),
            VertexAdcResolver::production(vertex_config_source(preferences.clone())),
            vertex_models_source(preferences.clone()),
        );
        let (_, handle) = start_with_components(
            preferences,
            WordBankDocument::empty(),
            LiveTranscriptStore::detached(),
            pipeline_tx,
            outbox,
            executor,
            providers,
            Arc::new(NoPricing),
            Arc::new(UnarmedVertex),
            Arc::new(|_| {}),
            DigestKey::new([41; 32]),
            ProcessEpoch(123),
            false,
            None,
            None,
            None,
        )
        .unwrap();

        let settled = handle.finalize("recording", raw_transcript(), false);
        assert!(settled.hosted_text_applied);
        assert_eq!(
            settled.transcript.segments[0].text,
            "factory corrected text"
        );
        assert_eq!(
            sends.load(Ordering::SeqCst),
            2,
            "catalog + one injected POST"
        );
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn project_and_quota_changes_rotate_both_local_and_provider_cache_scopes() {
        let key = DigestKey::new([23; 32]);
        let provider = ProviderId::new("google").unwrap();
        let transport = TransportId::new("vertex_api").unwrap();
        let model = ModelId::new("fixture-model").unwrap();
        // Routing edits receive independently generated opaque ids before request/provider keys are derived.
        let first_scope = ConnectionScopeId::new("scope-route-a").unwrap();
        let project_scope = ConnectionScopeId::new("scope-route-b").unwrap();
        let quota_scope = ConnectionScopeId::new("scope-route-c").unwrap();
        assert_ne!(first_scope, project_scope);
        assert_ne!(first_scope, quota_scope);

        let targets = vec![TranscriptRow {
            row_id: RowId::new("scope-row").unwrap(),
            speaker: "Fixture".into(),
            start_ms: 0,
            end_ms: 1,
            text: "synthetic".into(),
        }];
        let request_key = |scope: &ConnectionScopeId| {
            RequestKey::derive(
                &key,
                &RequestKeyMaterial {
                    provider: &provider,
                    transport: &transport,
                    support_tier: SupportTier::Documented,
                    connection_scope_id: scope,
                    region: Some("global"),
                    exact_model_id: &model,
                    adapter_version: 1,
                    prompt_template_version: PROMPT_TEMPLATE_VERSION,
                    output_schema_version: OUTPUT_SCHEMA_VERSION,
                    chunker_version: 1,
                    lane: Lane::Final,
                    billing_basis: BillingBasis::MeteredEstimate,
                    cache_policy: CachePolicy {
                        local: LocalCacheMode::Reusable,
                        provider: ProviderCacheMode::ExplicitStablePrefix,
                    },
                    word_bank_canonical_digest: "bank",
                    effective_steering: "",
                    targets: &targets,
                    context: &[],
                    context_truncated: false,
                    question: None,
                },
            )
        };
        let provider_key = |scope: &ConnectionScopeId| {
            ProviderCacheKey::derive(
                &key,
                &ProviderCacheKeyMaterial {
                    provider: &provider,
                    transport: &transport,
                    support_tier: SupportTier::Documented,
                    connection_scope_id: scope,
                    region: Some("global"),
                    exact_model_id: &model,
                    adapter_version: 1,
                    prompt_template_version: PROMPT_TEMPLATE_VERSION,
                    output_schema_version: OUTPUT_SCHEMA_VERSION,
                    prompt_task: PromptTask::Rewrite,
                    provider_cache_mode: ProviderCacheMode::ExplicitStablePrefix,
                    word_bank_canonical_digest: "bank",
                },
            )
        };
        assert_ne!(request_key(&first_scope), request_key(&project_scope));
        assert_ne!(provider_key(&first_scope), provider_key(&quota_scope));
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
    fn slow_provider_refresh_does_not_block_settings_and_stale_completion_is_discarded() {
        let path = dir("slow-provider-prepare");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let preferences = HostedPreferences::default()
            .revise(|values| {
                let bedrock = &mut values.providers.bedrock;
                bedrock.scope.connection_scope_id =
                    Some(ConnectionScopeId::new("fixture-bedrock-scope").unwrap());
                bedrock.scope.alias = Some("Fixture Bedrock".into());
                bedrock.scope.region = Some("us-east-1".into());
            })
            .unwrap();
        let providers = BlockingPrepareProviders::bedrock();
        let (_, handle) = start_with_components(
            shared(preferences),
            WordBankDocument::empty(),
            LiveTranscriptStore::detached(),
            pipeline_tx,
            outbox,
            Arc::new(DenyExecutor),
            Box::new(providers.clone()),
            Arc::new(NoPricing),
            Arc::new(UnarmedVertex),
            Arc::new(|_| {}),
            DigestKey::new([39; 32]),
            ProcessEpoch(139),
            false,
            None,
            None,
            None,
        )
        .unwrap();

        let descriptor = corti_postprocess::KnownTransport::BedrockRuntime.descriptor();
        let (refresh_reply, refresh_receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::RefreshProvider {
                provider: descriptor.provider.clone(),
                transport: descriptor.transport.clone(),
                reply: refresh_reply,
            })
            .unwrap();
        providers.wait_started();
        let (joined_reply, joined_receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::RefreshProvider {
                provider: descriptor.provider,
                transport: descriptor.transport,
                reply: joined_reply,
            })
            .unwrap();

        let before = handle.snapshot();
        let (patch_reply, patch_receive) = std::sync::mpsc::channel();
        let started = Instant::now();
        handle
            .send(ServiceCommand::Patch {
                request: HostedPatchRequest {
                    observed_state_revision: before.state_revision,
                    patch: HostedPatchInput::SetDisplayPreferences {
                        show_history_diagnostics: true,
                        show_live_metrics_by_default: false,
                    },
                },
                reply: patch_reply,
            })
            .unwrap();
        let patch = patch_receive.recv_timeout(Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(matches!(
            patch.unwrap().unwrap(),
            HostedMutationResult::Applied { .. }
        ));

        let (clear_reply, clear_receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::ClearBedrockSetup {
                request: ClearBedrockSetupRequest {
                    observed_state_revision: handle.snapshot().state_revision,
                },
                reply: clear_reply,
            })
            .unwrap();
        let clear = clear_receive.recv_timeout(Duration::from_millis(200));
        providers.release();
        assert!(matches!(
            clear.unwrap().unwrap(),
            HostedMutationResult::Applied { .. }
        ));
        assert_eq!(
            refresh_receive
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            Err(ErrorCode::Superseded)
        );
        assert_eq!(
            joined_receive.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(ErrorCode::Superseded)
        );
        assert_eq!(
            providers.resolve_calls.load(Ordering::Acquire),
            1,
            "identical refreshes must share one bounded preparation"
        );
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn catalog_refresh_reprojects_credentials_mutated_during_the_provider_call() {
        let path = dir("catalog-auth-mutation");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("postprocess-outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let fixture = FixtureProviders::openai();
        let descriptor = fixture.descriptor.clone();
        let credential = Arc::new(Mutex::new(CredentialState::Ready {
            expires_at_unix_ms: None,
            source: CredentialSourceKind::Keychain,
        }));
        let (_, handle) = start_with_components(
            shared(configured_preferences()),
            WordBankDocument::empty(),
            LiveTranscriptStore::detached(),
            pipeline_tx,
            outbox,
            Arc::new(DenyExecutor),
            Box::new(MutatingCatalogProviders {
                fixture,
                credential,
                mutate_on_catalog: true,
            }),
            Arc::new(NoPricing),
            Arc::new(UnarmedVertex),
            Arc::new(|_| {}),
            DigestKey::new([29; 32]),
            ProcessEpoch(129),
            false,
            None,
            None,
            None,
        )
        .unwrap();
        let (reply, receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::RefreshProvider {
                provider: descriptor.provider.clone(),
                transport: descriptor.transport.clone(),
                reply,
            })
            .unwrap();
        let refreshed = receive.recv().unwrap().unwrap();
        assert!(refreshed.models.is_empty());
        assert!(matches!(
            refreshed.credential,
            CredentialState::Error {
                code: ErrorCode::Cache
            }
        ));
        let snapshot = handle.snapshot();
        let projected = snapshot
            .providers
            .iter()
            .find(|state| state.descriptor == descriptor)
            .unwrap();
        assert!(matches!(
            projected.credential,
            CredentialState::Error {
                code: ErrorCode::Cache
            }
        ));
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn normal_provider_completion_reprojects_auth_mutated_during_execution() {
        let path = dir("request-auth-mutation");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("postprocess-outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let fixture = FixtureProviders::openai();
        let descriptor = fixture.descriptor.clone();
        let credential = Arc::new(Mutex::new(CredentialState::Ready {
            expires_at_unix_ms: None,
            source: CredentialSourceKind::Keychain,
        }));
        let (_, handle) = start_with_components(
            shared(configured_preferences()),
            WordBankDocument::empty(),
            LiveTranscriptStore::detached(),
            pipeline_tx,
            outbox,
            Arc::new(CredentialMutatingExecutor {
                credential: credential.clone(),
            }),
            Box::new(MutatingCatalogProviders {
                fixture,
                credential,
                mutate_on_catalog: false,
            }),
            Arc::new(NoPricing),
            Arc::new(UnarmedVertex),
            Arc::new(|_| {}),
            DigestKey::new([31; 32]),
            ProcessEpoch(131),
            false,
            None,
            None,
            None,
        )
        .unwrap();
        let settled = handle.finalize("recording", raw_transcript(), false);
        assert!(settled.hosted_text_applied);
        let snapshot = handle.snapshot();
        let projected = snapshot
            .providers
            .iter()
            .find(|state| state.descriptor == descriptor)
            .unwrap();
        assert!(matches!(
            projected.credential,
            CredentialState::Error {
                code: ErrorCode::Cache
            }
        ));
        assert!(projected.models.is_empty());
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
        let outbox_path = path.join("postprocess-outbox.json");
        assert!(
            !outbox_path.exists(),
            "Completed history must wait for user-visible checkpoint durability"
        );
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
        handle.mark_final_checkpointed(&settled.call_ids).unwrap();
        let outbox_text = String::from_utf8(std::fs::read(&outbox_path).unwrap()).unwrap();
        assert!(!outbox_text.contains("fixture raw text"));
        assert!(!outbox_text.contains("fixture corrected text"));
        assert_eq!(
            crate::checkpoint::FilingCheckpoint::load(&audio)
                .unwrap()
                .transcript,
            settled.transcript
        );

        queue
            .set_postprocess_state(&id, Some(corti_queue::PostprocessState::Finalizing))
            .unwrap();
        assert_eq!(handle.import_outbox(&queue).unwrap(), 1);
        assert_eq!(
            queue.get(&id).unwrap().unwrap().postprocess_state,
            Some(corti_queue::PostprocessState::Finalizing),
            "one terminal chunk must not publish completion for its whole final group"
        );
        assert_eq!(handle.import_outbox(&queue).unwrap(), 0);
        let history = queue.postprocess_history(&id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].call_id, settled.call_ids[0]);
        assert!(!history[0].provider_request_sent || history[0].error_code.is_none());
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn every_non_reusable_final_state_recovers_after_restart_without_paid_egress() {
        fn assert_restart_recovery(
            name: &str,
            mode: LocalCacheMode,
            mark_applied_before_restart: bool,
            encryption_key: [u8; 32],
        ) {
            let path = dir(name);
            let preferences = configured_preferences()
                .revise(|values| values.final_lane.local_cache = mode)
                .unwrap();
            let store_path = path.join("postprocess-store.enc");
            let outbox_path = path.join("postprocess-outbox.json");
            let first_outbox = Arc::new(TelemetryOutbox::open(outbox_path.clone()).unwrap());
            let (first_pipeline_tx, _first_pipeline_rx) = std::sync::mpsc::channel();
            let first_store = Box::new(
                RuntimeStore::open_encrypted(
                    store_path.clone(),
                    encryption_key,
                    first_outbox.clone(),
                    first_pipeline_tx.clone(),
                )
                .unwrap(),
            );
            let first_executor = Arc::new(RecordingExecutor::new());
            let (_, first_handle) = start_with_components(
                shared(preferences.clone()),
                WordBankDocument::empty(),
                LiveTranscriptStore::detached(),
                first_pipeline_tx,
                first_outbox,
                first_executor.clone(),
                Box::new(FixtureProviders::openai()),
                Arc::new(NoPricing),
                Arc::new(UnarmedVertex),
                Arc::new(|_| {}),
                DigestKey::new([71; 32]),
                ProcessEpoch(701),
                false,
                Some(first_store),
                None,
                None,
            )
            .unwrap();
            let first = first_handle.finalize("restart-recording", raw_transcript(), false);
            assert!(first.hosted_text_applied);
            assert_eq!(first_executor.target_texts.lock().unwrap().len(), 1);
            if mark_applied_before_restart {
                first_handle.mark_final_applied(&first.call_ids).unwrap();
            }
            drop(first_handle);
            std::thread::sleep(Duration::from_millis(30));

            let encrypted = std::fs::read(&store_path).unwrap();
            assert!(
                !encrypted
                    .windows("fixture raw text".len())
                    .any(|window| window == b"fixture raw text")
            );
            assert!(
                !encrypted
                    .windows("fixture corrected text".len())
                    .any(|window| window == b"fixture corrected text")
            );

            let second_outbox = Arc::new(TelemetryOutbox::open(outbox_path).unwrap());
            let (second_pipeline_tx, _second_pipeline_rx) = std::sync::mpsc::channel();
            let second_store = Box::new(
                RuntimeStore::open_encrypted(
                    store_path,
                    encryption_key,
                    second_outbox.clone(),
                    second_pipeline_tx.clone(),
                )
                .unwrap(),
            );
            let second_executor = Arc::new(RecordingExecutor::new());
            let (_, second_handle) = start_with_components(
                shared(preferences),
                WordBankDocument::empty(),
                LiveTranscriptStore::detached(),
                second_pipeline_tx,
                second_outbox,
                second_executor.clone(),
                Box::new(FixtureProviders::openai()),
                Arc::new(NoPricing),
                Arc::new(UnarmedVertex),
                Arc::new(|_| {}),
                DigestKey::new([71; 32]),
                ProcessEpoch(702),
                false,
                Some(second_store),
                None,
                None,
            )
            .unwrap();
            let recovered = second_handle.finalize("restart-recording", raw_transcript(), false);
            assert!(recovered.hosted_text_applied);
            assert_eq!(
                recovered.transcript.segments[0].text,
                "fixture corrected text"
            );
            assert!(
                second_executor.target_texts.lock().unwrap().is_empty(),
                "restart recovery must use the encrypted journal, not another provider request"
            );
            // ResultCached advances and Applied is idempotent through the same public acknowledgement.
            second_handle
                .mark_final_applied(&recovered.call_ids)
                .unwrap();
            std::fs::remove_dir_all(path).ok();
        }

        assert_restart_recovery(
            "recovery-only-result-cached",
            LocalCacheMode::RecoveryOnly,
            false,
            [61; 32],
        );
        assert_restart_recovery(
            "memory-only-applied",
            LocalCacheMode::MemoryOnly,
            true,
            [62; 32],
        );
    }

    #[test]
    fn cache_retention_modes_obey_process_restart_and_recovery_boundaries() {
        let path = dir("cache-retention-modes");
        let make_outbox = |name: &str| {
            Arc::new(TelemetryOutbox::open(path.join(format!("{name}-outbox.json"))).unwrap())
        };

        let reusable_path = path.join("reusable.enc");
        let reusable_outbox = make_outbox("reusable");
        let (reusable_tx, _reusable_rx) = std::sync::mpsc::channel();
        let mut reusable = RuntimeStore::open_encrypted(
            reusable_path.clone(),
            [101; 32],
            reusable_outbox.clone(),
            reusable_tx.clone(),
        )
        .unwrap();
        let key = fixture_request_key();
        commit_fixture_output(
            &mut reusable,
            key,
            LocalCacheMode::Reusable,
            Lane::Live,
            None,
        )
        .unwrap();
        assert!(matches!(
            reusable.lookup_exact(key),
            Ok(ExactLookup::Hit(_))
        ));
        drop(reusable);
        let mut reopened =
            RuntimeStore::open_encrypted(reusable_path, [101; 32], reusable_outbox, reusable_tx)
                .unwrap();
        assert!(matches!(
            reopened.lookup_exact(key),
            Ok(ExactLookup::Hit(_))
        ));

        let memory_path = path.join("memory-only.enc");
        let memory_outbox = make_outbox("memory");
        let (memory_tx, _memory_rx) = std::sync::mpsc::channel();
        let mut memory = RuntimeStore::open_encrypted(
            memory_path.clone(),
            [102; 32],
            memory_outbox.clone(),
            memory_tx.clone(),
        )
        .unwrap();
        commit_fixture_output(
            &mut memory,
            key,
            LocalCacheMode::MemoryOnly,
            Lane::Live,
            None,
        )
        .unwrap();
        assert!(matches!(memory.lookup_exact(key), Ok(ExactLookup::Hit(_))));
        drop(memory);
        let mut reopened =
            RuntimeStore::open_encrypted(memory_path, [102; 32], memory_outbox, memory_tx).unwrap();
        assert!(matches!(reopened.lookup_exact(key), Ok(ExactLookup::Miss)));

        let recovery_path = path.join("recovery-only.enc");
        let recovery_outbox = make_outbox("recovery");
        let (recovery_tx, _recovery_rx) = std::sync::mpsc::channel();
        let mut recovery = RuntimeStore::open_encrypted(
            recovery_path.clone(),
            [103; 32],
            recovery_outbox.clone(),
            recovery_tx.clone(),
        )
        .unwrap();
        let boundary = fixture_boundary("fixture-recovery-call", "fixture-recovery-group");
        recovery.prepare_final(&boundary, 1024).unwrap();
        recovery.mark_final_dispatched(&boundary).unwrap();
        commit_fixture_output(
            &mut recovery,
            boundary.request_key,
            LocalCacheMode::RecoveryOnly,
            Lane::Final,
            Some(&boundary),
        )
        .unwrap();
        assert!(matches!(
            recovery.lookup_exact(boundary.request_key),
            Ok(ExactLookup::Miss)
        ));
        drop(recovery);
        let mut reopened =
            RuntimeStore::open_encrypted(recovery_path, [103; 32], recovery_outbox, recovery_tx)
                .unwrap();
        assert!(matches!(
            reopened.lookup_exact(boundary.request_key),
            Ok(ExactLookup::Miss)
        ));
        assert_eq!(
            reopened
                .recover_final("fixture-sensitive-recording")
                .unwrap()
                .unwrap()
                .state,
            FinalJournalState::ResultCached
        );
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn reusable_cache_evicts_lru_expires_and_purges_without_touching_recovery() {
        let path = dir("cache-lifecycle");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let mut store = RuntimeStore::memory(outbox, pipeline_tx);
        let (provider_output, _) = fixture_store_outputs();
        let output = StoredProviderOutput::from_output(&provider_output);
        let lru_base = unix_millis().saturating_sub(MAX_STORE_CACHE_ENTRIES as i64);
        for index in 0..MAX_STORE_CACHE_ENTRIES {
            store.state.exact_cache.insert(
                format!("fixture-lru-{index:04}"),
                StoredCacheEntry::new(output.clone(), lru_base.saturating_add(index as i64)),
            );
        }
        let key = fixture_request_key();
        commit_fixture_output(&mut store, key, LocalCacheMode::Reusable, Lane::Live, None).unwrap();
        assert_eq!(store.state.exact_cache.len(), MAX_STORE_CACHE_ENTRIES);
        assert!(!store.state.exact_cache.contains_key("fixture-lru-0000"));
        assert!(store.state.exact_cache.contains_key(&key.to_base64url()));

        store.state.exact_cache.insert(
            key.to_base64url(),
            StoredCacheEntry::new(
                output.clone(),
                unix_millis().saturating_sub(REUSABLE_CACHE_MAX_AGE_MS + 1),
            ),
        );
        assert!(matches!(store.lookup_exact(key), Ok(ExactLookup::Miss)));

        let boundary = fixture_boundary("fixture-active-call", "fixture-active-group");
        store.prepare_final(&boundary, 1024).unwrap();
        store.state.exact_cache.insert(
            "fixture-purge-a".into(),
            StoredCacheEntry::new(output.clone(), unix_millis()),
        );
        store.state.exact_cache.insert(
            "fixture-purge-b".into(),
            StoredCacheEntry::new(output.clone(), unix_millis()),
        );
        assert!(store.purge_reusable_cache_inner().unwrap() >= 2);
        assert!(store.state.exact_cache.is_empty());
        assert_eq!(store.state.journals.len(), 1);
        assert_eq!(store.state.journals[0].state, FinalJournalState::Prepared);

        for index in 0..MAX_MEMORY_CACHE_ENTRIES + 32 {
            store.insert_memory_cache(format!("memory-{index}"), output.clone());
        }
        assert!(store.memory_cache.len() <= MAX_MEMORY_CACHE_ENTRIES);
        assert!(
            store
                .memory_cache
                .values()
                .map(|entry| entry.approx_bytes)
                .sum::<usize>()
                <= MAX_MEMORY_CACHE_BYTES
        );
        store
            .state
            .exact_cache
            .insert(key.to_base64url(), StoredCacheEntry::Legacy(output));
        assert!(matches!(store.lookup_exact(key), Ok(ExactLookup::Miss)));
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn final_history_waits_for_checkpoint_and_abandonment_records_failure() {
        let path = dir("final-history-boundary");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let mut store = RuntimeStore::memory(outbox.clone(), pipeline_tx);
        let boundary = fixture_boundary("fixture-history-call", "fixture-history-group");
        store.prepare_final(&boundary, 1024).unwrap();
        store.mark_final_dispatched(&boundary).unwrap();
        commit_fixture_output(
            &mut store,
            boundary.request_key,
            LocalCacheMode::RecoveryOnly,
            Lane::Final,
            Some(&boundary),
        )
        .unwrap();
        assert!(outbox.entries.lock().unwrap().is_empty());
        store
            .mark_final_group_applied(std::slice::from_ref(&boundary))
            .unwrap();
        assert!(outbox.entries.lock().unwrap().is_empty());
        store
            .mark_final_group_checkpointed(std::slice::from_ref(&boundary))
            .unwrap();
        let history = outbox.entries.lock().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].outcome, TerminalOutcomeDto::Completed);
        drop(history);

        let abandoned = fixture_boundary("fixture-abandoned-call", "fixture-abandoned-group");
        store.prepare_final(&abandoned, 1024).unwrap();
        store.mark_final_dispatched(&abandoned).unwrap();
        commit_fixture_output(
            &mut store,
            abandoned.request_key,
            LocalCacheMode::RecoveryOnly,
            Lane::Final,
            Some(&abandoned),
        )
        .unwrap();
        store.abandon_final(&abandoned).unwrap();
        let history = outbox.entries.lock().unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].outcome, TerminalOutcomeDto::Failed);
        assert_eq!(history[1].error, Some(ErrorCode::Canceled));
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn checkpoint_acknowledgement_keeps_applied_output_until_outbox_is_durable() {
        let path = dir("checkpoint-outbox-failure");
        let blocker = path.join("outbox-parent");
        std::fs::create_dir_all(&blocker).unwrap();
        let outbox = Arc::new(TelemetryOutbox::open(blocker.join("outbox.json")).unwrap());
        std::fs::remove_dir(&blocker).unwrap();
        std::fs::write(&blocker, b"block").unwrap();
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let mut store = RuntimeStore::memory(outbox, pipeline_tx);
        let boundary = fixture_boundary("fixture-outbox-call", "fixture-outbox-group");
        store.prepare_final(&boundary, 1024).unwrap();
        store.mark_final_dispatched(&boundary).unwrap();
        commit_fixture_output(
            &mut store,
            boundary.request_key,
            LocalCacheMode::RecoveryOnly,
            Lane::Final,
            Some(&boundary),
        )
        .unwrap();
        store
            .mark_final_group_applied(std::slice::from_ref(&boundary))
            .unwrap();

        assert_eq!(
            store.mark_final_group_checkpointed(std::slice::from_ref(&boundary)),
            Err(ErrorCode::Cache)
        );
        let journal = &store.state.journals[0];
        assert_eq!(journal.state, FinalJournalState::Applied);
        assert!(journal.output.is_some());
        assert!(journal.terminal.is_some());
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn optional_cache_failure_keeps_paid_live_output_but_final_durability_stays_mandatory() {
        let path = dir("cache-failure-policy");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let mut store = RuntimeStore::memory(outbox, pipeline_tx);
        let blocker = path.join("not-a-directory");
        std::fs::write(&blocker, b"block").unwrap();
        store.persistence = RuntimeStorePersistence::Encrypted {
            path: blocker.join("store.enc"),
            cipher: Box::new(StoreCipher::new([104; 32]).unwrap()),
        };
        let key = fixture_request_key();
        commit_fixture_output(&mut store, key, LocalCacheMode::Reusable, Lane::Live, None).unwrap();
        assert!(matches!(store.lookup_exact(key), Ok(ExactLookup::Hit(_))));

        store.persistence = RuntimeStorePersistence::Memory;
        let boundary = fixture_boundary("fixture-mandatory-call", "fixture-mandatory-group");
        store.prepare_final(&boundary, 1024).unwrap();
        store.mark_final_dispatched(&boundary).unwrap();
        store.persistence = RuntimeStorePersistence::Encrypted {
            path: blocker.join("store.enc"),
            cipher: Box::new(StoreCipher::new([104; 32]).unwrap()),
        };
        assert_eq!(
            commit_fixture_output(
                &mut store,
                boundary.request_key,
                LocalCacheMode::RecoveryOnly,
                Lane::Final,
                Some(&boundary),
            ),
            Err(ErrorCode::Cache)
        );
        assert_eq!(store.state.journals[0].state, FinalJournalState::Dispatched);
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn final_group_capacity_is_reserved_atomically_before_any_dispatch() {
        let path = dir("final-group-reservation");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let mut store = RuntimeStore::memory(outbox, pipeline_tx);
        let (provider_output, _) = fixture_store_outputs();
        store.state.exact_cache.insert(
            "optional-entry".into(),
            StoredCacheEntry::new(
                StoredProviderOutput::from_output(&provider_output),
                unix_millis(),
            ),
        );
        let first = fixture_boundary("reserve-call-a", "reserve-group");
        let second = fixture_boundary("reserve-call-b", "reserve-group");
        let before = store.state.exact_cache.len();
        assert_eq!(
            store.prepare_final_group(&[
                (first, MAX_STORE_BYTES / 2),
                (second, MAX_STORE_BYTES / 2),
            ]),
            Err(ErrorCode::Cache)
        );
        assert!(store.state.journals.is_empty());
        assert_eq!(store.state.exact_cache.len(), before);
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn encrypted_intent_and_group_transitions_have_crash_atomicity() {
        let path = dir("encrypted-intent");
        let outbox = Arc::new(TelemetryOutbox::open(path.join("outbox.json")).unwrap());
        let (pipeline_tx, _pipeline_rx) = std::sync::mpsc::channel();
        let store_path = path.join("store.enc");
        let mut store = RuntimeStore::open_encrypted(
            store_path.clone(),
            [81; 32],
            outbox.clone(),
            pipeline_tx.clone(),
        )
        .unwrap();
        let first = fixture_boundary("fixture-call-a", "fixture-group");
        let mut second = fixture_boundary("fixture-call-b", "fixture-group");
        second.request_key = RequestKey::derive(
            &DigestKey::new([92; 32]),
            &RequestKeyMaterial {
                provider: &ProviderId::new("openai").unwrap(),
                transport: &TransportId::new("openai_api").unwrap(),
                support_tier: SupportTier::Documented,
                connection_scope_id: &ConnectionScopeId::new("fixture-scope").unwrap(),
                region: None,
                exact_model_id: &ModelId::new("fixture-model").unwrap(),
                adapter_version: 1,
                prompt_template_version: PROMPT_TEMPLATE_VERSION,
                output_schema_version: OUTPUT_SCHEMA_VERSION,
                chunker_version: 2,
                lane: Lane::Final,
                billing_basis: BillingBasis::MeteredEstimate,
                cache_policy: CachePolicy {
                    local: LocalCacheMode::RecoveryOnly,
                    provider: ProviderCacheMode::Off,
                },
                word_bank_canonical_digest: "fixture-bank",
                effective_steering: "",
                targets: &[],
                context: &[],
                context_truncated: false,
                question: None,
            },
        );
        store.prepare_final(&first, 1024).unwrap();
        store.prepare_final(&second, 1024).unwrap();
        store.mark_final_dispatched(&first).unwrap();
        drop(store);

        let encrypted = std::fs::read(&store_path).unwrap();
        assert!(
            !encrypted
                .windows("fixture-sensitive-recording".len())
                .any(|window| window == b"fixture-sensitive-recording")
        );
        let mut recovered =
            RuntimeStore::open_encrypted(store_path, [81; 32], outbox, pipeline_tx).unwrap();
        assert_eq!(
            recovered
                .recover_final("fixture-sensitive-recording")
                .unwrap()
                .unwrap()
                .state,
            FinalJournalState::Dispatched,
            "one dispatched chunk makes the complete final group ambiguous"
        );
        assert_eq!(
            recovered
                .state
                .journals
                .iter()
                .find(|journal| journal.call_id == first.call_id)
                .unwrap()
                .state,
            FinalJournalState::Dispatched
        );

        for journal in &mut recovered.state.journals {
            journal.state = FinalJournalState::ResultCached;
        }
        recovered.persistence = RuntimeStorePersistence::Memory;
        let blocker = path.join("not-a-directory");
        std::fs::write(&blocker, b"block").unwrap();
        recovered.persistence = RuntimeStorePersistence::Encrypted {
            path: blocker.join("store.enc"),
            cipher: Box::new(StoreCipher::new([81; 32]).unwrap()),
        };
        assert_eq!(
            recovered.mark_final_group_applied(&[first.clone(), second.clone()]),
            Err(ErrorCode::Cache)
        );
        assert!(
            recovered
                .state
                .journals
                .iter()
                .all(|journal| journal.state == FinalJournalState::ResultCached)
        );
        recovered.persistence = RuntimeStorePersistence::Memory;
        recovered
            .mark_final_group_applied(&[first, second])
            .unwrap();
        assert!(
            recovered
                .state
                .journals
                .iter()
                .all(|journal| journal.state == FinalJournalState::Applied)
        );
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn empty_final_transcript_never_dispatches_a_provider_call() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let executor = Arc::new(RecordingExecutor::new());
        let (handle, _pipeline_rx, path) = start_fixture("empty-final", executor.clone(), events);
        let transcript = DiarizedTranscript::new(vec![TranscriptSegment {
            speaker: Speaker::Me,
            start: 0.0,
            end: 0.0,
            text: "   ".into(),
        }]);

        let settled = handle.finalize("recording", transcript.clone(), false);

        assert_eq!(settled.transcript, transcript);
        assert!(!settled.hosted_text_applied);
        assert_eq!(
            settled.applied_postprocess.final_outcome(),
            Some(FinalPostprocessOutcome::Disabled)
        );
        assert!(settled.call_ids.is_empty());
        assert_eq!(settled.fallback_code, Some(ErrorCode::PolicyBlocked));
        assert!(executor.target_texts.lock().unwrap().is_empty());

        let next = handle.finalize("next-recording", raw_transcript(), false);
        assert!(
            next.hosted_text_applied,
            "empty final must release its session"
        );
        assert_eq!(executor.target_texts.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn live_final_waits_for_finish_only_row_ingress_before_fencing() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (handle, _pipeline_rx, path) =
            start_fixture("live-final-barrier", Arc::new(RewriteExecutor), events);
        handle.begin_live_session("recording").unwrap();
        handle
            .try_observe_finalized_rows(
                "recording",
                vec![TranscriptRow {
                    row_id: RowId::new("finish-only-row").unwrap(),
                    speaker: "Me".into(),
                    start_ms: 0,
                    end_ms: 1_000,
                    text: "fixture finish-only context".into(),
                }],
            )
            .unwrap();

        // No sleep: Final must place a FIFO barrier behind the row before its normal service command can
        // overtake ingress. Without the barrier, start_final observes the supplied transcript and the queued
        // row arrives later, advancing the watermark twice.
        let settled = handle.finalize("recording", raw_transcript(), true);
        assert!(settled.hosted_text_applied);

        let (reply, receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::SubmitAdHoc {
                question: "fixture barrier question".into(),
                reply,
            })
            .unwrap();
        let call_id = receive.recv().unwrap().unwrap();
        let snapshot = assistant(&handle);
        let exchange = snapshot
            .exchanges
            .iter()
            .find(|exchange| exchange.call_id == call_id)
            .unwrap();
        assert_eq!(
            exchange.as_of_revision, 1,
            "the finish-only row must be observed exactly once"
        );

        handle.abandon_final_result(&settled.call_ids).unwrap();
        handle.end_live_session("recording").unwrap();
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn live_oversized_first_row_is_not_silently_dropped_or_byte_truncated() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let executor = Arc::new(RecordingExecutor::new());
        let (handle, _pipeline_rx, path) =
            start_fixture("live-oversized-row", executor.clone(), events);
        let descriptor = corti_postprocess::KnownTransport::OpenAiDirect.descriptor();
        let (reply, receive) = std::sync::mpsc::channel();
        handle
            .send(ServiceCommand::RefreshProvider {
                provider: descriptor.provider.clone(),
                transport: descriptor.transport.clone(),
                reply,
            })
            .unwrap();
        receive.recv().unwrap().unwrap();
        let settings = handle.snapshot();
        handle
            .patch_for_test(HostedPatchRequest {
                observed_state_revision: settings.state_revision,
                patch: HostedPatchInput::SetLaneSelection {
                    lane: HostedLaneDto::Live,
                    selection: HostedSelectionInput {
                        provider: Some(descriptor.provider.as_str().into()),
                        transport: Some(descriptor.transport.as_str().into()),
                        model: Some("fixture-model".into()),
                        local_cache: LocalCacheMode::Reusable,
                        provider_cache: ProviderCacheMode::Off,
                    },
                },
            })
            .unwrap();
        let settings = handle.snapshot();
        handle
            .patch_for_test(HostedPatchRequest {
                observed_state_revision: settings.state_revision,
                patch: HostedPatchInput::SetLaneEnabled {
                    lane: HostedLaneDto::Live,
                    enabled: true,
                },
            })
            .unwrap();
        handle.begin_live_session("recording").unwrap();
        let text = "x".repeat(MAX_LIVE_TARGET_BYTES + 512);
        handle
            .try_observe_finalized_rows(
                "recording",
                vec![TranscriptRow {
                    row_id: RowId::new("oversized-live-row").unwrap(),
                    speaker: "Me".into(),
                    start_ms: 0,
                    end_ms: 1_000,
                    text: text.clone(),
                }],
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !executor.target_texts.lock().unwrap().is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "oversized live row was dropped");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(executor.target_texts.lock().unwrap()[0], vec![text]);
        let _ = handle.end_live_session("recording");
        std::fs::remove_dir_all(path).ok();
    }

    #[test]
    fn question_context_reports_token_budget_truncation_and_final_rows_are_indivisible() {
        let rows = vec![
            TranscriptRow {
                row_id: RowId::new("context-old").unwrap(),
                speaker: "Me".into(),
                start_ms: 0,
                end_ms: 1,
                text: "old context".into(),
            },
            TranscriptRow {
                row_id: RowId::new("context-new").unwrap(),
                speaker: "Me".into(),
                start_ms: 1,
                end_ms: 2,
                text: "new context".into(),
            },
        ];
        let newest_budget = estimated_row_tokens(&rows[1]);
        let (bounded, truncated) = bounded_question_context(&rows, newest_budget);
        assert!(truncated);
        assert_eq!(bounded, vec![rows[1].clone()]);

        let too_small = estimated_row_tokens(&rows[0]).saturating_sub(1);
        assert_eq!(
            final_chunks(&rows, too_small),
            Err(ErrorCode::PolicyBlocked)
        );
    }

    #[test]
    fn final_replacements_keep_their_original_segment_indices() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let (handle, _pipeline_rx, path) =
            start_fixture("final-segment-indices", Arc::new(RewriteExecutor), events);
        let transcript = DiarizedTranscript::new(vec![
            TranscriptSegment {
                speaker: Speaker::Me,
                start: 0.0,
                end: 0.0,
                text: String::new(),
            },
            TranscriptSegment {
                speaker: Speaker::Me,
                start: 1.0,
                end: 2.0,
                text: "fixture first raw row".into(),
            },
            TranscriptSegment {
                speaker: Speaker::Other("Them".into()),
                start: 2.0,
                end: 2.0,
                text: "   ".into(),
            },
            TranscriptSegment {
                speaker: Speaker::Other("Them".into()),
                start: 3.0,
                end: 4.0,
                text: "fixture second raw row".into(),
            },
        ]);

        let settled = handle.finalize("recording", transcript, false);

        assert!(settled.hosted_text_applied);
        assert_eq!(settled.transcript.segments[0].text, "");
        assert_eq!(
            settled.transcript.segments[1].text,
            "fixture corrected text"
        );
        assert_eq!(settled.transcript.segments[2].text, "   ");
        assert_eq!(
            settled.transcript.segments[3].text,
            "fixture corrected text"
        );
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
        let recording_id = format!(
            "{}fixture",
            crate::live_test::MICROPHONE_TEST_RECORDING_PREFIX
        );
        handle.begin_live_session(&recording_id).unwrap();
        let forty_words = (0..40)
            .map(|index| format!("word{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        handle
            .try_observe_finalized_rows(
                &recording_id,
                vec![TranscriptRow {
                    row_id: RowId::new("assistant-row-1").unwrap(),
                    speaker: "Me".into(),
                    start_ms: 0,
                    end_ms: 31_000,
                    text: forty_words,
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
        let before_pinned = handle.snapshot();
        handle
            .send(ServiceCommand::SetPinnedTemplate {
                request: PinnedQuestionUpdateRequest {
                    observed_state_revision: before_pinned.state_revision,
                    template: "fixture pinned question".into(),
                },
                reply,
            })
            .unwrap();
        assert!(matches!(
            receive.recv().unwrap().unwrap(),
            HostedMutationResult::Applied { .. }
        ));
        // Let the revision observer see the saved template while auto-run is still off. Enabling auto later
        // must reconsider existing context rather than relying on a lucky same-tick ordering.
        std::thread::sleep(Duration::from_millis(30));
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
        // Saving the template after substantial speech should schedule its initial answer from the transcript
        // already present; requiring another threshold made late onboarding appear to do nothing.
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
        handle.end_live_session(&recording_id).unwrap();
        let queue = Queue::open_at(path.join("queue.db")).unwrap();
        assert_eq!(
            handle.import_outbox(&queue).unwrap(),
            2,
            "ephemeral question calls should be acknowledged without a recording row"
        );
        assert_eq!(handle.import_outbox(&queue).unwrap(), 0);
        assert!(queue.get(&recording_id).unwrap().is_none());
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
    fn lifecycle_and_checkpoint_acknowledgements_bypass_saturated_normal_commands() {
        let path = dir("priority-saturation");
        let (command_tx, _command_rx) = sync_channel(1);
        let (dummy_reply, _dummy_receive) = std::sync::mpsc::channel();
        command_tx
            .send(ServiceCommand::AssistantSnapshot { reply: dummy_reply })
            .unwrap();
        let (priority_tx, priority_rx) = std::sync::mpsc::channel();
        let (ingress, _ingress_rx) = CoordinatorIngress::bounded(1);
        let preferences = configured_preferences();
        let word_bank = WordBankDocument::empty();
        let control =
            control_from_preferences(ProcessEpoch(501), &preferences, word_bank.revision());
        let snapshot = Arc::new(Mutex::new(settings_snapshot(
            1,
            &preferences,
            &word_bank,
            &control,
            &initial_provider_states(),
            false,
            &|_| false,
        )));
        let handle = HostedHandle {
            command_tx,
            priority_tx,
            ingress,
            snapshot,
            ingress_incomplete: Arc::new(AtomicBool::new(false)),
            outbox: Arc::new(TelemetryOutbox::open(path.join("postprocess-outbox.json")).unwrap()),
        };

        let ending = handle.clone();
        let end_thread = std::thread::spawn(move || ending.end_live_session("recording"));
        match priority_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            ServiceCommand::EndSession { reply, .. } => reply.send(Ok(())).unwrap(),
            _ => panic!("lifecycle command did not use the priority channel"),
        }
        assert_eq!(end_thread.join().unwrap(), Ok(()));

        let checkpointing = handle.clone();
        let checkpoint_thread = std::thread::spawn(move || {
            checkpointing.mark_final_checkpointed(&[CallId::new("priority-call").unwrap()])
        });
        match priority_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            ServiceCommand::MarkFinalCheckpointed { reply, .. } => reply.send(Ok(())).unwrap(),
            _ => panic!("checkpoint acknowledgement did not use the priority channel"),
        }
        assert_eq!(checkpoint_thread.join().unwrap(), Ok(()));
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
            shared(preferences),
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
            None,
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
    fn vertex_direct_models_keep_the_curated_ids_and_collapse_repeats() {
        let ids = |models: &[VertexModel]| {
            models
                .iter()
                .map(|model| model.exact_model_id().as_str().to_owned())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            ids(&vertex_direct_models(&[])),
            ["gemini-2.5-flash", "gemini-2.5-pro", "claude-sonnet-4-5"]
        );
        // A repeat of a curated id would make `VertexRestAdapter::new` reject the whole catalog.
        assert_eq!(
            ids(&vertex_direct_models(&[
                "gemini-2.5-pro".to_owned(),
                "gemini-2.5-flash-lite".to_owned(),
            ])),
            [
                "gemini-2.5-flash",
                "gemini-2.5-pro",
                "claude-sonnet-4-5",
                "gemini-2.5-flash-lite"
            ]
        );

        let claude = vertex_direct_models(&["claude-opus-4-5@20251101".to_owned()])
            .into_iter()
            .find(|model| model.exact_model_id().as_str() == "claude-opus-4-5@20251101")
            .unwrap();
        assert_eq!(claude.publisher(), VertexPublisher::Anthropic);
    }

    #[test]
    fn vertex_adapter_slot_rebuilds_when_the_scope_or_model_list_changes() {
        let preferences = shared(HostedPreferences::default());
        let transports = Arc::new(InjectedTransportFactory {
            openai: Arc::new(Mutex::new(VecDeque::new())),
            sends: Arc::new(AtomicUsize::new(0)),
        });
        let slot = VertexAdapterSlot {
            transports,
            resolver: VertexAdcResolver::production(vertex_config_source(preferences.clone())),
            models: vertex_models_source(preferences.clone()),
            built: Mutex::new(None),
        };
        let catalog = |region: &str| {
            let scope = ProviderScope {
                connection_scope_id: ConnectionScopeId::new("fixture-vertex-scope").unwrap(),
                region: Some(region.to_owned()),
            };
            slot.current()
                .ok_or(ErrorCode::AuthUnarmed)?
                .lock()
                .unwrap()
                .catalog(&scope)
                .map(|catalog| {
                    catalog
                        .models
                        .iter()
                        .map(|model| model.exact_model_id.as_str().to_owned())
                        .collect::<Vec<_>>()
                })
                .map_err(|error| error.code)
        };
        let revise = |edit: fn(&mut crate::postprocess_config::HostedPreferenceValues)| {
            let mut guard = preferences.lock().unwrap();
            *guard = guard.clone().revise(edit).unwrap();
        };

        // No routing yet: the scope has not been saved, so there is nothing to build.
        assert_eq!(catalog("global"), Err(ErrorCode::AuthUnarmed));

        // Saving the scope must arm the catalog without a relaunch.
        revise(|values| {
            values.providers.vertex.project = Some("fixture-project".to_owned());
            values.providers.vertex.region = Some("global".to_owned());
        });
        assert_eq!(
            catalog("global"),
            Ok(vec![
                "gemini-2.5-flash".to_owned(),
                "gemini-2.5-pro".to_owned(),
                "claude-sonnet-4-5".to_owned()
            ])
        );

        // An adapter built for the old region rejects the new one outright.
        revise(|values| values.providers.vertex.region = Some("us-east5".to_owned()));
        assert_eq!(catalog("us-east5").map(|models| models.len()), Ok(3));

        revise(|values| {
            values.providers.vertex_models = vec!["gemini-2.5-flash-lite".to_owned()];
        });
        assert_eq!(
            catalog("us-east5"),
            Ok(vec![
                "gemini-2.5-flash".to_owned(),
                "gemini-2.5-pro".to_owned(),
                "claude-sonnet-4-5".to_owned(),
                "gemini-2.5-flash-lite".to_owned(),
            ])
        );
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
            shared(vertex_preferences()),
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
            None,
            Some(clock.clone()),
            None,
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
        handle.end_live_session("recording").unwrap();
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
        assert!(
            descriptors
                .iter()
                .all(|descriptor| descriptor.transport.as_str() != "codex_app_server")
        );
        let chatgpt = descriptors
            .iter()
            .find(|descriptor| descriptor.transport.as_str() == "chatgpt_subscription")
            .unwrap();
        assert_eq!(chatgpt.support_tier, SupportTier::Experimental);
        assert_eq!(chatgpt.billing_basis, BillingBasis::IncludedSubscription);
        assert!(chatgpt.adapter_available);
    }
}
