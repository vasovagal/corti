// Typed wrappers over the Tauri `invoke` bridge — the single place the settings DTO shapes are mirrored
// from the Rust `app/src/settings.rs` commands. Keep these in sync with that file.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Mirror of Rust `settings::SettingsDto`. `env_managed` lists the fields a `CORTI_*` env var pins. */
export interface SettingsDto {
  transcribe_backend: string; // "aws" | "local"
  aws_bucket: string | null;
  language: string;
  aws_profile: string | null;
  aws_region: string | null;
  local_threads: number;
  local_asr_engine: string; // "sherpa" | "ggml"
  local_ggml_available: boolean; // build capability; not persisted
  local_diarize_far_end: boolean;
  local_embedding_model: string; // a corti-transcribe-local EMBEDDING_IDS id, e.g. "titanet"
  aec_enabled: boolean;
  retention_days: number;
  live_filing: boolean;
  live_buffer_minutes: number;
  env_managed: string[];
}

/** Mirror of Rust `settings::BackendInfo`. */
export interface BackendInfo {
  id: string;
  label: string;
  compiled_in: boolean;
}

export const getConfig = (): Promise<SettingsDto> => invoke<SettingsDto>("get_config");

export const getBackends = (): Promise<BackendInfo[]> => invoke<BackendInfo[]>("get_backends");

export const setConfig = (dto: SettingsDto): Promise<void> => invoke<void>("set_config", { dto });

// ----- AWS credential-chain status + verification -----

/** Mirror of Rust `settings::AwsStatus`. Never carries the secret/session-token values — booleans only. */
export interface AwsStatus {
  profiles: string[];
  selected_profile: string | null;
  configured_region: string | null;
  profile_locked: boolean;
  region_locked: boolean;
  env_access_key_id: string | null;
  env_has_secret: boolean;
  env_session_token: boolean;
  env_profile: string | null;
  env_region: string | null;
  source: string;
}

/** Mirror of Rust `settings::AwsIdentity` (STS GetCallerIdentity). */
export interface AwsIdentity {
  account: string | null;
  arn: string | null;
  user_id: string | null;
}

export const getAwsStatus = (): Promise<AwsStatus> => invoke<AwsStatus>("get_aws_status");

export const verifyAws = (): Promise<AwsIdentity> => invoke<AwsIdentity>("verify_aws");

// ----- Path + Model sections -----

/** Mirror of Rust `settings::PathsDto`. `models_*` is null when the local backend isn't compiled in. */
export interface PathsDto {
  recordings_dir: string;
  recordings_bytes: number;
  models_dir: string | null;
  models_bytes: number | null;
}

/** Mirror of Rust `settings::ModelStatus`. */
export interface ModelStatus {
  id: string;
  label: string;
  present: boolean;
  on_disk_bytes: number;
  download_bytes: number;
  diarize_only: boolean;
}

export const getPaths = (): Promise<PathsDto> => invoke<PathsDto>("get_paths");

export const revealPath = (which: "recordings" | "models"): Promise<void> =>
  invoke<void>("reveal_path", { which });

export const setModelsDir = (dir: string): Promise<void> => invoke<void>("set_models_dir", { dir });

export const getModelsStatus = (asrEngine?: string): Promise<ModelStatus[]> =>
  invoke<ModelStatus[]>("get_models_status", { asrEngine: asrEngine ?? null });

/** Install state of the selectable English speaker-embedding models (the Transcription dropdown). */
export const getEmbeddingModels = (): Promise<ModelStatus[]> =>
  invoke<ModelStatus[]>("get_embedding_models");

export const downloadModel = (id: string): Promise<void> => invoke<void>("download_model", { id });

/** Payload of the `model-download-progress` event emitted while a model downloads. */
export interface DownloadProgress {
  id: string;
  received: number;
  total: number;
}

export const onDownloadProgress = (cb: (p: DownloadProgress) => void): Promise<UnlistenFn> =>
  listen<DownloadProgress>("model-download-progress", (e) => cb(e.payload));

// ----- Diagnostics console (mirror of Rust `console::ConsoleEntry`) -----

/** Mirror of Rust `console::ConsoleEntry`. One captured tracing event. Field names match the Rust struct
 * verbatim (serde serializes them as-is — no specta binding generation in corti). */
export interface ConsoleEntry {
  timestamp: string; // ISO-8601 UTC, e.g. "2026-06-18T17:04:05.123Z"
  level: string; // "ERROR" | "WARN" | "INFO" | "DEBUG" | "TRACE"
  target: string; // event metadata target (usually the module path)
  message: string; // the `message` field, else a " field=value" concatenation
}

/** Snapshot of the in-memory console ring buffer, oldest entry first. */
export const getConsoleLogs = (): Promise<ConsoleEntry[]> =>
  invoke<ConsoleEntry[]>("get_console_logs");

/** The same buffer rendered as plain text (one entry per line). */
export const getConsoleLogsText = (): Promise<string> => invoke<string>("get_console_logs_text");

/** Opens a native save dialog and writes the buffer text. Resolves true if written, false if cancelled;
 * rejects with a string on write failure. */
export const saveConsoleLogs = (): Promise<boolean> => invoke<boolean>("save_console_logs");

// ----- Resource stats (mirror of Rust `stats::*`; serde field names verbatim — no specta in corti) -----

/** Mirror of Rust `stats::ThreadStat`. One OS thread's scheduler-averaged CPU share. */
export interface ThreadStat {
  name: string; // thread name (e.g. "corti-pipeline", "corti-stats"); may be "" for unnamed threads
  cpu_pct: number; // scheduler-averaged CPU %, per-core (Mach cpu_usage permille / 10; decays, not exact)
}

/** Mirror of Rust `stats::StageSample`. One completed coarse pipeline stage. */
export interface StageSample {
  timestamp: string; // ISO-8601 UTC
  stage: string; // "transcribe" | "file"
  backend: string; // "aws" | "local" | "none" | "vagus"
  duration_ms: number;
}

/** Mirror of Rust `stats::StatsSnapshot`. One 1 Hz sample of process health. */
export interface StatsSnapshot {
  timestamp: string; // ISO-8601 UTC, same format as ConsoleEntry.timestamp
  backend: string; // active label ("AWS Transcribe" | "Parakeet / CPU" | "Parakeet / Metal" | "none")
  detector_recording: boolean;
  webinar_recording: boolean;
  phys_mb: number; // phys_footprint, mebibytes (MiB)
  rss_mb: number; // resident set size, mebibytes (MiB)
  threads: ThreadStat[];
}

/** Mirror of Rust `stats::StatsReport`. One coherent read of the stats buffer. */
export interface StatsReport {
  history: StatsSnapshot[]; // 1 Hz memory/thread samples, oldest first (~5 min ring)
  stages: StageSample[]; // process-global completed pipeline-stage timings, oldest first (not per-sample)
}

/** One coherent snapshot of the stats buffer: memory/thread history + global stage timings. */
export const getStats = (): Promise<StatsReport> => invoke<StatsReport>("get_stats");

// ----- Pipeline activity (mirror of Rust `activity::PipelineActivity`) -----

/** Mirror of Rust `activity::PipelineActivity`. `stage` is a stable id (see lib/pipeline.ts); `detail`
 * is the tray status line; `recording` is true while either capture source is live. */
export interface PipelineActivity {
  stage: string;
  detail: string;
  recording: boolean;
}

export const getPipelineActivity = (): Promise<PipelineActivity> =>
  invoke<PipelineActivity>("get_pipeline_activity");

// ----- Timestamped Live Transcript window -----

export type LiveTranscriptMode = "idle" | "call" | "test";
export type LiveTranscriptStatus =
  | "idle"
  | "loading"
  | "listening"
  | "stopping"
  | "complete"
  | "unavailable"
  | "error";

export type HostedRewriteState = "raw" | "clean";

export interface LiveTranscriptLine {
  seq: number;
  /** Stable backend identity. Optional while opening snapshots from the protocol-v1 app. */
  row_id?: string;
  speaker: string;
  start_sec: number;
  end_sec: number;
  /** Immutable ASR text. Clean text must never replace this field. */
  text: string;
  clean_text?: string | null;
  rewrite_state?: HostedRewriteState;
  /** Changes only when a newly accepted rewrite is committed; used for one-shot presentation. */
  commit_epoch?: number;
}

export interface LiveTranscriptSnapshot {
  /** Protocol-v2 monotonic identity is mandatory at the transcript/webview boundary. */
  protocol_version: number;
  process_epoch: number;
  session_generation: number;
  revision: number;
  session_id: string | null;
  mode: LiveTranscriptMode;
  status: LiveTranscriptStatus;
  title: string;
  detail: string | null;
  active: boolean;
  evicted_lines: number;
  retained_from_seq: number;
  lines: LiveTranscriptLine[];
}

export interface LiveTranscriptEvent extends Omit<LiveTranscriptSnapshot, "lines"> {
  /** Must equal the frontend's current revision before the delta is applied. */
  from_revision: number;
  reset: boolean;
  line: LiveTranscriptLine | null;
}

export const getLiveTranscript = (): Promise<LiveTranscriptSnapshot> =>
  invoke<LiveTranscriptSnapshot>("get_live_transcript");

export const getLiveTestWindowGeneration = (): Promise<number> =>
  invoke<number>("get_live_test_window_generation");

export const startLiveTest = (windowGeneration: number): Promise<void> =>
  invoke<void>("start_live_test", { windowGeneration });
export const stopLiveTest = (): Promise<void> => invoke<void>("stop_live_test");

export const onLiveTranscriptChanged = (
  handler: (event: LiveTranscriptEvent) => void,
): Promise<UnlistenFn> =>
  listen<LiveTranscriptEvent>("live-transcript-changed", (event) => handler(event.payload));

// ----- Cross-window Preferences navigation -----

export type PreferencesSection =
  | "transcription"
  | "hosted"
  | "hosted-provider"
  | "hosted-routing"
  | "hosted-language"
  | "hosted-advanced"
  | "storage";

/** Open/focus Preferences at one backend-allowlisted repair destination. */
export const openPreferencesSection = (section: PreferencesSection): Promise<void> =>
  invoke<void>("open_preferences_section", { section });

/** Take the backend-owned latest repair destination. Only the Settings window is allowed to call this. */
export const takePreferencesSectionRequest = (): Promise<PreferencesSection | null> =>
  invoke<PreferencesSection | null>("take_preferences_section_request");

/** Existing Preferences singletons receive only a wake-up; the destination is fetched from Rust. */
export const onPreferencesNavigationRequested = (
  handler: () => void,
): Promise<UnlistenFn> =>
  listen("settings-navigation-requested", handler);

// ----- Hosted post-processing preferences -----

export type HostedSupportTier = "documented" | "experimental" | "blocked";
export type HostedBillingBasis =
  | "metered_estimate"
  | "included_subscription"
  | "no_provider_request"
  | "unknown";
export type HostedErrorCode =
  | "auth_unarmed"
  | "auth_rejected"
  | "permission"
  | "quota"
  | "rate_limited"
  | "model_unavailable"
  | "network"
  | "timeout"
  | "canceled"
  | "superseded"
  | "policy_blocked"
  | "cache"
  | "malformed_output"
  | "provider"
  | "broker_exited"
  | "ambiguous_dispatch"
  | "internal";
export type HostedCredentialSource =
  | "keychain"
  | "workload_identity"
  | "application_default_credentials"
  | "broker_keyring"
  | "chat_gpt_device"
  | "aws_default_chain"
  | "aws_profile"
  | "aws_static_keychain"
  | "aws_assumed_role"
  | "aws_sso";
export type HostedLocalCacheMode = "reusable" | "recovery_only" | "memory_only";
export type HostedProviderCacheMode =
  | "off"
  | "explicit_stable_prefix"
  | "unavoidable_implicit"
  | "unavailable";
export type HostedLane = "live" | "final" | "question";

/** Mirrors `corti_postprocess::ProviderDescriptor`; support status always comes from Rust. */
export interface HostedProviderDescriptor {
  provider: string;
  transport: string;
  support_tier: HostedSupportTier;
  billing_basis: HostedBillingBasis;
  adapter_available: boolean;
}

export interface HostedModelCapabilities {
  text_input: boolean;
  text_output: boolean;
  streaming: boolean;
  structured_output: boolean;
  explicit_prefix_cache: boolean;
  implicit_cache_may_apply: boolean;
}

/** One exact account/region-scoped model returned by the backend catalog. */
export interface HostedModelDescriptor {
  provider: string;
  transport: string;
  support_tier: HostedSupportTier;
  exact_model_id: string;
  account_scoped_available: boolean;
  region: string | null;
  max_context_tokens: number;
  max_output_tokens: number;
  capabilities: HostedModelCapabilities;
  billing_basis: HostedBillingBasis;
  tariff_version: string | null;
  deprecated: boolean;
  benchmarked_for_live: boolean;
}

/** Secret-free credential projection. No key or token value can be represented by this union. */
export type HostedCredentialState =
  | { state: "absent" }
  | { state: "resolving" }
  | {
      state: "ready";
      expires_at_unix_ms: number | null;
      source: HostedCredentialSource;
    }
  | { state: "awaiting_user" }
  | {
      state: "device_authorization";
      verification_url: string;
      user_code: string;
      login_id: string;
    }
  | { state: "refreshing" }
  | { state: "rejected" }
  | { state: "unsupported"; code: HostedErrorCode }
  | { state: "error"; code: HostedErrorCode };

export interface HostedProviderState {
  descriptor: HostedProviderDescriptor;
  credential: HostedCredentialState;
  models: HostedModelDescriptor[];
  service_error: HostedErrorCode | null;
}

export interface HostedLaneSelection {
  provider: string | null;
  transport: string | null;
  model: string | null;
  cache_policy: {
    local: HostedLocalCacheMode;
    provider: HostedProviderCacheMode;
  };
}

export interface HostedLaneControl {
  enabled: boolean;
  revision: number;
  selection: HostedLaneSelection;
}

export interface HostedControlSnapshot {
  process_epoch: number;
  session_generation: number;
  control_revision: number;
  steering_revision: number;
  bank_revision: number;
  pinned_question_revision: number;
  master_enabled: boolean;
  egress_acknowledged: boolean;
  pinned_auto_enabled: boolean;
  live: HostedLaneControl;
  final_lane: HostedLaneControl;
  questions: HostedLaneControl;
}

export interface HostedProviderScope {
  provider: string;
  transport: string;
  configured: boolean;
  alias: string | null;
  project: string | null;
  region: string | null;
  quota_project: string | null;
}

/** Mirror of Rust `postprocess_config::AwsCredentialMode`. */
export type AwsCredentialMode =
  | "default_chain"
  | "profile"
  | "static_keychain"
  | "assume_role"
  | "sso";

/** Mirror of Rust `postprocess_app::BedrockCredentialDto`. The `has_*` flags are presence only — no key
 * material can be represented here, and none crosses the IPC boundary in either direction. */
export interface BedrockCredentialDto {
  mode: AwsCredentialMode;
  profile: string | null;
  role_arn: string | null;
  has_access_key_id: boolean;
  has_secret_access_key: boolean;
  has_session_token: boolean;
}

/** Mirror of Rust `postprocess_app::AwsCredentialOptionsDto`. Secret presence is not here: it comes
 * from `HostedSettingsDto.bedrock`, which refreshes on every coordinator event. */
export interface AwsCredentialOptionsDto {
  profiles: string[];
}

/** Mirror of Rust `postprocess_app::AwsKeySlotDto`. */
export type AwsKeySlot = "access_key_id" | "secret_access_key" | "session_token";

/** Mirror of Rust `postprocess_app::SecretSlotRequest`. */
export type SecretSlotRequest =
  | { provider: "open_ai" }
  | { provider: "anthropic" }
  | { provider: "aws"; slot: AwsKeySlot };

/** Mirror of Rust `postprocess_app::SecretEntryResultDto`. */
export type SecretEntryResult = "stored" | "cancelled" | "rejected";

/** Mirror of Rust `postprocess_app::HostedSettingsDto`; it is deliberately secret-free. */
export interface HostedSettingsDto {
  state_revision: number;
  preferences_revision: number;
  control: HostedControlSnapshot;
  providers: HostedProviderState[];
  scopes: HostedProviderScope[];
  bedrock: BedrockCredentialDto;
  /** Exact Vertex model ids the operator typed, in save order. */
  vertex_models: string[];
  default_steering: string;
  word_bank: {
    revision: number;
    entries: string[];
  };
  final_deadline_seconds: number;
  show_history_diagnostics: boolean;
  show_live_metrics_by_default: boolean;
}

export interface HostedSelectionInput {
  provider: string | null;
  transport: string | null;
  model: string | null;
  local_cache: HostedLocalCacheMode;
  provider_cache: HostedProviderCacheMode;
}

export type HostedPatchInput =
  | { kind: "set_egress_acknowledged"; acknowledged: boolean }
  | { kind: "set_master"; enabled: boolean }
  | { kind: "set_lane_enabled"; lane: HostedLane; enabled: boolean }
  | { kind: "set_lane_selection"; lane: HostedLane; selection: HostedSelectionInput }
  | { kind: "set_pinned_auto"; enabled: boolean; acknowledged: boolean }
  | {
      kind: "set_display_preferences";
      show_history_diagnostics: boolean;
      show_live_metrics_by_default: boolean;
    };

export type HostedMutationResult =
  | { status: "applied"; settings: HostedSettingsDto }
  | { status: "unchanged"; settings: HostedSettingsDto }
  | { status: "conflict"; settings: HostedSettingsDto }
  | {
      status: "disabled_for_session";
      settings: HostedSettingsDto;
      code: HostedErrorCode;
    };

export interface HostedProviderScopeUpdate {
  provider: string;
  transport: string;
  alias: string | null;
  project: string | null;
  region: string | null;
  quota_project: string | null;
}

export type HostedCallLane = "live" | "final" | "ad_hoc_question" | "pinned_question";
export type HostedLaneState =
  | "disabled"
  | "waiting_for_phrase"
  | "debouncing"
  | "queued"
  | "arming"
  | "catching_up"
  | "rewriting"
  | "finalizing"
  | "clean"
  | "using_raw"
  | "failed";
export type HostedQuestionStatus =
  | "queued"
  | "waiting_for_credential"
  | "running"
  | "completed"
  | "canceled"
  | "failed";
export type HostedCacheObservation =
  | "none"
  | "local"
  | "provider_read"
  | "provider_write"
  | "provider_implicit";

export interface HostedNormalizedUsage {
  input_tokens: number | null;
  output_tokens: number | null;
  cached_read_tokens: number | null;
  cached_write_tokens: number | null;
  reasoning_tokens: number | null;
  usage_complete: boolean;
}

export interface HostedCostEstimate {
  billing_basis: HostedBillingBasis;
  cost_micros: number | null;
  currency: string | null;
  pricing_catalog_version: string | null;
  tariff_id: string | null;
  tariff_effective_at_unix_ms: number | null;
}

export interface HostedLatencyFields {
  queue_us: number | null;
  auth_us: number | null;
  cache_lookup_us: number | null;
  connect_us: number | null;
  ttfb_us: number | null;
  ttft_us: number | null;
  stream_us: number | null;
  parse_us: number | null;
  cache_commit_us: number | null;
  total_us: number | null;
}

export interface HostedRequestFence {
  process_epoch: number;
  session_generation: number;
  transcript_revision: number;
  control_revision: number;
  lane_revision: number;
  steering_revision: number;
  bank_revision: number;
  question_revision: number | null;
}

export interface HostedAccountingEvent {
  event: "accounting";
  call_id: string;
  recording_id: string;
  lane: HostedCallLane;
  fence: HostedRequestFence;
  finality: "provisional" | "final";
  usage: HostedNormalizedUsage;
  cost: HostedCostEstimate;
  late: boolean;
}

export interface HostedTerminalEvent {
  event: "terminal";
  call_id: string;
  recording_id: string;
  request_group_id: string;
  target_id: string | null;
  lane: HostedCallLane;
  attempt_no: number;
  fence: HostedRequestFence;
  provider: string;
  transport: string;
  model: string;
  support_tier: HostedSupportTier;
  adapter_version: number;
  prompt_version: number;
  output_schema_version: number;
  outcome: "completed" | "failed" | "canceled" | "superseded" | "timeout";
  error: HostedErrorCode | null;
  provider_request_sent: boolean;
  late_content_discarded: boolean;
  cache: HostedCacheObservation;
  usage: HostedNormalizedUsage;
  cost: HostedCostEstimate;
  latency: HostedLatencyFields;
  queued_at_unix_ms: number;
  dispatched_at_unix_ms: number | null;
  completed_at_unix_ms: number;
}

export type HostedCoordinatorEvent =
  | ({ event: "control_changed" } & HostedControlSnapshot)
  | ({ event: "provider_state" } & HostedProviderState)
  | {
      event: "lane_state";
      lane: HostedCallLane;
      state: HostedLaneState;
      code: HostedErrorCode | null;
      fence: Pick<
        HostedRequestFence,
        "process_epoch" | "session_generation" | "control_revision" | "lane_revision"
      >;
    }
  | {
      event: "notice";
      role: "alert";
      visible_message: string;
      episode: number;
    }
  | HostedAccountingEvent
  | HostedTerminalEvent
  | { event: "persistence_warning"; code: HostedErrorCode }
  | { event: string; [field: string]: unknown };

export interface HostedAssistantExchange {
  call_id: string;
  as_of_revision: number;
  status: HostedQuestionStatus;
  error: HostedErrorCode | null;
  question: string;
  answer: string | null;
  cost_label: string | null;
  /** Protocol-v2 additions are optional against an older coordinator. */
  context_truncated?: boolean;
  usage?: HostedNormalizedUsage | null;
  cache?: HostedCacheObservation | null;
}

export interface HostedAssistantSnapshot {
  pinned_run_count: number;
  pinned: HostedAssistantExchange | null;
  exchanges: HostedAssistantExchange[];
}

export const getHostedSettings = (): Promise<HostedSettingsDto> =>
  invoke<HostedSettingsDto>("get_hosted_settings");

export const patchHostedSettings = (
  observedStateRevision: number,
  patch: HostedPatchInput,
): Promise<HostedMutationResult> =>
  invoke<HostedMutationResult>("patch_hosted_settings", {
    request: { observed_state_revision: observedStateRevision, patch },
  });

export const updateHostedSteering = (
  observedStateRevision: number,
  text: string,
  persistAsDefault: boolean,
): Promise<HostedMutationResult> =>
  invoke<HostedMutationResult>("update_hosted_steering", {
    request: {
      observed_state_revision: observedStateRevision,
      text,
      persist_as_default: persistAsDefault,
    },
  });

export const replaceHostedWordBank = (
  observedStateRevision: number,
  entries: string[],
): Promise<HostedMutationResult> =>
  invoke<HostedMutationResult>("replace_hosted_word_bank", {
    request: { observed_state_revision: observedStateRevision, entries },
  });

export const updateHostedProviderScope = (
  observedStateRevision: number,
  update: HostedProviderScopeUpdate,
): Promise<HostedMutationResult> =>
  invoke<HostedMutationResult>("update_hosted_provider_scope", {
    request: { observed_state_revision: observedStateRevision, ...update },
  });

export const refreshHostedProvider = (
  provider: string,
  transport: string,
): Promise<HostedProviderState> =>
  invoke<HostedProviderState>("refresh_hosted_provider", {
    request: { provider, transport },
  });

export const setHostedPinnedQuestion = (
  observedStateRevision: number,
  template: string,
): Promise<HostedMutationResult> =>
  invoke<HostedMutationResult>("set_hosted_pinned_question", {
    request: { observed_state_revision: observedStateRevision, template },
  });

export const startChatGptDeviceLogin = (): Promise<HostedProviderState> =>
  invoke<HostedProviderState>("start_chatgpt_device_login");

export const cancelChatGptDeviceLogin = (): Promise<HostedProviderState> =>
  invoke<HostedProviderState>("cancel_chatgpt_device_login");

export const signOutChatGptSubscription = (): Promise<HostedProviderState> =>
  invoke<HostedProviderState>("sign_out_chatgpt_subscription");

export const openChatGptDeviceLogin = (): Promise<void> =>
  invoke<void>("open_chatgpt_device_login");

/** The `~/.aws` profile names available on this machine. */
export const listAwsCredentialOptions = (): Promise<AwsCredentialOptionsDto> =>
  invoke<AwsCredentialOptionsDto>("list_aws_credential_options");

export const setBedrockCredentialMode = (
  observedStateRevision: number,
  mode: AwsCredentialMode,
  profile: string | null,
  roleArn: string | null,
): Promise<HostedMutationResult> =>
  invoke<HostedMutationResult>("set_bedrock_credential_mode", {
    request: {
      observed_state_revision: observedStateRevision,
      mode,
      profile,
      role_arn: roleArn,
    },
  });

/** Vertex publishes no per-project listing of the models a caller may invoke, so the typed ids are the
 * catalog. Saving rebuilds the Vertex adapter. */
export const setHostedVertexModels = (
  observedStateRevision: number,
  models: string[],
): Promise<HostedMutationResult> =>
  invoke<HostedMutationResult>("set_hosted_vertex_models", {
    request: { observed_state_revision: observedStateRevision, models },
  });

/** Opens the native secure-entry sheet. The typed value goes straight to the private secret store; this call only
 * ever learns whether it was stored, cancelled, or rejected. */
export const promptForProviderSecret = (request: SecretSlotRequest): Promise<SecretEntryResult> =>
  invoke<SecretEntryResult>("prompt_for_provider_secret", { request });

export const clearProviderSecret = (request: SecretSlotRequest): Promise<void> =>
  invoke<void>("clear_provider_secret", { request });

export const getHostedAssistant = (): Promise<HostedAssistantSnapshot> =>
  invoke<HostedAssistantSnapshot>("get_hosted_assistant");

export const submitHostedQuestion = (question: string): Promise<string> =>
  invoke<string>("submit_hosted_question", { question });

export const cancelHostedQuestion = (callId: string): Promise<void> =>
  invoke<void>("cancel_hosted_question", { callId });

export const onHostedStateChanged = (
  handler: (event: HostedCoordinatorEvent) => void,
): Promise<UnlistenFn> =>
  listen<HostedCoordinatorEvent>("hosted-state-changed", (event) => handler(event.payload));

// ----- Recording Queue window -----

/** Mirror of Rust `queue_ui::RecordingDto`. */
export interface RecordingDto {
  id: string;
  app: string;
  mode: string; // "call" | "webinar"
  started_at: string;
  ended_at: string | null;
  duration_secs: number | null;
  status: string; // JobStatus wire form: "recording" | "pending_transcription" | ... | "done" | "failed"
  error: string | null;
  transcribe_secs: number | null;
  note_path: string | null;
  note_exists: boolean;
  audio_exists: boolean;
  recovery_exists: boolean;
  audio_bytes: number | null;
  retry_pending: boolean;
  retry_attempts: number | null;
}

export const listRecordings = (): Promise<RecordingDto[]> =>
  invoke<RecordingDto[]>("list_recordings");

export const retryRecording = (id: string): Promise<void> =>
  invoke<void>("retry_recording", { id });

export const openNote = (path: string): Promise<void> => invoke<void>("open_note", { path });

export const revealAudio = (id: string): Promise<void> => invoke<void>("reveal_audio", { id });

/** Subscribe to the pipeline's coarse "something changed" signal; returns the unlisten fn. */
export const onQueueChanged = (handler: () => void): Promise<UnlistenFn> =>
  listen("queue-changed", handler);
