// Deterministic, non-personal product data for the marketing screenshot suite.
// Keys are the Tauri command names consumed by app/ui/src/lib/api.ts.

const liveTranscript = {
  revision: 8,
  session_id: "marketing-zoom-call",
  mode: "call",
  status: "listening",
  title: "Zoom · live transcript",
  detail: "Recording · streaming durable windows to the vagus inbox",
  active: true,
  evicted_lines: 0,
  retained_from_seq: 1,
  lines: [
    {
      seq: 1,
      speaker: "Me",
      start_sec: 3.2,
      end_sec: 7.8,
      text: "The inbox note is already open. Corti is writing this while we talk.",
    },
    {
      seq: 2,
      speaker: "Them 1",
      start_sec: 8.4,
      end_sec: 13.6,
      text: "So if the app closes, the completed transcript windows are still on disk?",
    },
    {
      seq: 3,
      speaker: "Me",
      start_sec: 14.1,
      end_sec: 20.3,
      text: "Exactly. Each bounded chunk is synced before its memory is reused.",
    },
    {
      seq: 4,
      speaker: "Them 1",
      start_sec: 22.0,
      end_sec: 28.7,
      text: "And this meeting gets tagged as Zoom without a calendar integration or a bot?",
    },
    {
      seq: 5,
      speaker: "Me",
      start_sec: 29.1,
      end_sec: 35.9,
      text: "Corti sees which process owns the microphone and carries that source into the note.",
    },
    {
      seq: 6,
      speaker: "Them 2",
      start_sec: 38.3,
      end_sec: 42.5,
      text: "Then vagus can find the decisions later by wording or by meaning.",
    },
  ],
};

const recordings = [
  {
    id: "20260818-193105-zoom",
    app: "Zoom",
    mode: "call",
    started_at: "2026-08-18T19:31:05Z",
    ended_at: "2026-08-18T19:54:11Z",
    duration_secs: 1386,
    status: "done",
    error: null,
    transcribe_secs: 5.8,
    note_path: "00-Inbox/zoom-call-2026-08-18.md",
    note_exists: true,
    audio_exists: true,
    recovery_exists: true,
    audio_bytes: 18_720_000,
    retry_pending: false,
    retry_attempts: null,
  },
  {
    id: "20260818-181944-slack",
    app: "Slack",
    mode: "call",
    started_at: "2026-08-18T18:19:44Z",
    ended_at: "2026-08-18T18:52:02Z",
    duration_secs: 1938,
    status: "done",
    error: null,
    transcribe_secs: 7.1,
    note_path: "10-Projects/demo/slack-huddle.md",
    note_exists: false,
    audio_exists: false,
    recovery_exists: false,
    audio_bytes: null,
    retry_pending: false,
    retry_attempts: null,
  },
  {
    id: "20260818-174206-google-chrome",
    app: "Google Chrome",
    mode: "webinar",
    started_at: "2026-08-18T17:42:06Z",
    ended_at: null,
    duration_secs: null,
    status: "recording",
    error: null,
    transcribe_secs: null,
    note_path: null,
    note_exists: false,
    audio_exists: true,
    recovery_exists: true,
    audio_bytes: 9_400_000,
    retry_pending: false,
    retry_attempts: null,
  },
];

const openAiDirect = {
  descriptor: {
    provider: "openai",
    transport: "openai_api",
    support_tier: "documented",
    billing_basis: "metered_estimate",
    adapter_available: true,
  },
  credential: {
    state: "ready",
    expires_at_unix_ms: null,
    source: "keychain",
  },
  models: [
    {
      provider: "openai",
      transport: "openai_api",
      support_tier: "documented",
      exact_model_id: "gpt-5.6-luna",
      account_scoped_available: true,
      region: null,
      max_context_tokens: 1_050_000,
      max_output_tokens: 128_000,
      capabilities: {
        text_input: true,
        text_output: true,
        streaming: true,
        structured_output: true,
        explicit_prefix_cache: true,
        implicit_cache_may_apply: false,
      },
      billing_basis: "metered_estimate",
      tariff_version: null,
      deprecated: false,
      benchmarked_for_live: false,
    },
  ],
  service_error: null,
};

const bedrockModels = [
  {
    provider: "amazon",
    transport: "bedrock_runtime",
    support_tier: "documented",
    exact_model_id: "anthropic.claude-sonnet-4-20250514-v1:0",
    account_scoped_available: true,
    region: "us-east-1",
    max_context_tokens: 32_000,
    max_output_tokens: 4_096,
    capabilities: {
      text_input: true,
      text_output: true,
      streaming: true,
      structured_output: true,
      explicit_prefix_cache: false,
      implicit_cache_may_apply: false,
    },
    billing_basis: "metered_estimate",
    tariff_version: null,
    deprecated: false,
    benchmarked_for_live: false,
  },
  {
    provider: "amazon",
    transport: "bedrock_runtime",
    support_tier: "documented",
    exact_model_id: "us.anthropic.claude-sonnet-4-20250514-v1:0",
    account_scoped_available: true,
    region: "us-east-1",
    max_context_tokens: 32_000,
    max_output_tokens: 4_096,
    capabilities: {
      text_input: true,
      text_output: true,
      streaming: true,
      structured_output: true,
      explicit_prefix_cache: false,
      implicit_cache_may_apply: false,
    },
    billing_basis: "metered_estimate",
    tariff_version: null,
    deprecated: false,
    benchmarked_for_live: false,
  },
];

const bedrockDescriptor = {
  provider: "amazon",
  transport: "bedrock_runtime",
  support_tier: "documented",
  billing_basis: "metered_estimate",
  adapter_available: true,
};

export const hostedSettings = {
  state_revision: 12,
  preferences_revision: 7,
  control: {
    process_epoch: 71,
    session_generation: 3,
    control_revision: 8,
    steering_revision: 4,
    bank_revision: 7,
    pinned_question_revision: 1,
    master_enabled: true,
    egress_acknowledged: true,
    pinned_auto_enabled: false,
    codex_experimental_approved: false,
    live: {
      enabled: false,
      revision: 2,
      selection: {
        provider: null,
        transport: null,
        model: null,
        cache_policy: { local: "reusable", provider: "off" },
      },
    },
    final_lane: {
      enabled: true,
      revision: 5,
      selection: {
        provider: "openai",
        transport: "openai_api",
        model: "gpt-5.6-luna",
        cache_policy: { local: "reusable", provider: "off" },
      },
    },
    questions: {
      enabled: true,
      revision: 4,
      selection: {
        provider: "openai",
        transport: "openai_api",
        model: "gpt-5.6-luna",
        cache_policy: { local: "reusable", provider: "off" },
      },
    },
  },
  providers: [
    {
      descriptor: {
        provider: "anthropic",
        transport: "anthropic_api",
        support_tier: "documented",
        billing_basis: "metered_estimate",
        adapter_available: true,
      },
      credential: { state: "absent" },
      models: [],
      service_error: null,
    },
    {
      descriptor: {
        provider: "anthropic",
        transport: "claude_subscription",
        support_tier: "blocked",
        billing_basis: "unknown",
        adapter_available: false,
      },
      credential: { state: "unsupported", code: "policy_blocked" },
      models: [],
      service_error: null,
    },
    {
      descriptor: {
        provider: "google",
        transport: "vertex_api",
        support_tier: "documented",
        billing_basis: "metered_estimate",
        adapter_available: true,
      },
      credential: { state: "absent" },
      models: [],
      service_error: null,
    },
    {
      descriptor: {
        provider: "openai",
        transport: "codex_app_server",
        support_tier: "experimental",
        billing_basis: "included_subscription",
        adapter_available: false,
      },
      credential: { state: "unsupported", code: "policy_blocked" },
      models: [],
      service_error: null,
    },
    openAiDirect,
    {
      descriptor: bedrockDescriptor,
      credential: { state: "absent" },
      models: [],
      service_error: null,
    },
  ],
  scopes: [
    {
      provider: "google",
      transport: "vertex_api",
      configured: true,
      alias: "Screenshot Vertex",
      project: "corti-screenshot-project",
      region: "global",
      quota_project: null,
    },
    {
      provider: "openai",
      transport: "openai_api",
      configured: true,
      alias: "Direct API billing",
      project: null,
      region: null,
      quota_project: null,
    },
    {
      provider: "anthropic",
      transport: "anthropic_api",
      configured: false,
      alias: null,
      project: null,
      region: null,
      quota_project: null,
    },
    {
      provider: "amazon",
      transport: "bedrock_runtime",
      configured: false,
      alias: null,
      project: null,
      region: null,
      quota_project: null,
    },
  ],
  bedrock: {
    mode: "default_chain",
    profile: null,
    role_arn: null,
    has_access_key_id: false,
    has_secret_access_key: false,
    has_session_token: false,
  },
  default_steering: "Preserve speaker intent; correct only clear recognition errors.",
  word_bank: {
    revision: 7,
    entries: ["Corti", "Parakeet", "Vagus"],
  },
  final_deadline_seconds: 90,
  show_history_diagnostics: false,
  show_live_metrics_by_default: false,
};

export const syntheticVertexReadySettings = {
  ...hostedSettings,
  state_revision: 13,
  providers: hostedSettings.providers.map((provider) =>
    provider.descriptor.transport === "vertex_api"
      ? {
          ...provider,
          credential: {
            state: "ready",
            expires_at_unix_ms: 1_787_000_900_000,
            source: "application_default_credentials",
          },
          models: [
            {
              provider: "google",
              transport: "vertex_api",
              support_tier: "documented",
              exact_model_id: "gemini-synthetic-live-001",
              account_scoped_available: true,
              region: "global",
              max_context_tokens: 1_000_000,
              max_output_tokens: 65_536,
              capabilities: {
                text_input: true,
                text_output: true,
                streaming: true,
                structured_output: true,
                explicit_prefix_cache: false,
                implicit_cache_may_apply: true,
              },
              billing_basis: "metered_estimate",
              tariff_version: null,
              deprecated: false,
              benchmarked_for_live: false,
            },
          ],
        }
      : provider,
  ),
};

/** Bedrock connected through a named `~/.aws` profile. */
export const syntheticBedrockProfileSettings = {
  ...hostedSettings,
  state_revision: 14,
  providers: hostedSettings.providers.map((provider) =>
    provider.descriptor.transport === "bedrock_runtime"
      ? {
          ...provider,
          credential: {
            state: "ready",
            expires_at_unix_ms: null,
            source: "aws_profile",
          },
          models: bedrockModels,
        }
      : provider,
  ),
  scopes: hostedSettings.scopes.map((scope) =>
    scope.transport === "bedrock_runtime"
      ? { ...scope, configured: true, alias: "Screenshot Bedrock", region: "us-east-1" }
      : scope,
  ),
  bedrock: {
    mode: "profile",
    profile: "corti-screenshot",
    role_arn: null,
    has_access_key_id: false,
    has_secret_access_key: false,
    has_session_token: false,
  },
};

/** The static key-pair mode, with both Keychain slots filled and the session token left unset. */
export const syntheticBedrockKeypairSettings = {
  ...syntheticBedrockProfileSettings,
  state_revision: 15,
  bedrock: {
    mode: "static_keychain",
    profile: null,
    role_arn: null,
    has_access_key_id: true,
    has_secret_access_key: true,
    has_session_token: false,
  },
  providers: syntheticBedrockProfileSettings.providers.map((provider) =>
    provider.descriptor.transport === "bedrock_runtime"
      ? {
          ...provider,
          credential: {
            state: "ready",
            expires_at_unix_ms: null,
            source: "aws_static_keychain",
          },
        }
      : provider,
  ),
};

/** An assumed role whose session is counting down. The fixed capture clock makes the label stable. */
export const syntheticBedrockAssumedRoleSettings = {
  ...syntheticBedrockProfileSettings,
  state_revision: 16,
  bedrock: {
    mode: "assume_role",
    profile: "corti-screenshot",
    role_arn: "arn:aws:iam::123456789012:role/corti-bedrock-invoke",
    has_access_key_id: false,
    has_secret_access_key: false,
    has_session_token: false,
  },
  providers: syntheticBedrockProfileSettings.providers.map((provider) =>
    provider.descriptor.transport === "bedrock_runtime"
      ? {
          ...provider,
          credential: {
            // 47 minutes after the capture clock's fixed instant.
            state: "ready",
            expires_at_unix_ms: new Date("2026-08-18T20:47:00-04:00").getTime(),
            source: "aws_assumed_role",
          },
        }
      : provider,
  ),
};

/** An expired IAM Identity Center session — the case that must read as recoverable, not broken. */
export const syntheticBedrockRejectedSsoSettings = {
  ...syntheticBedrockProfileSettings,
  state_revision: 17,
  bedrock: {
    mode: "sso",
    profile: "corti-sso",
    role_arn: null,
    has_access_key_id: false,
    has_secret_access_key: false,
    has_session_token: false,
  },
  providers: syntheticBedrockProfileSettings.providers.map((provider) =>
    provider.descriptor.transport === "bedrock_runtime"
      ? { ...provider, credential: { state: "rejected" }, models: [] }
      : provider,
  ),
};

export const fixtures: Record<string, unknown> = {
  get_live_transcript: liveTranscript,
  start_live_test: null,
  stop_live_test: null,
  list_recordings: recordings,
  retry_recording: null,
  open_note: null,
  reveal_audio: null,
  get_pipeline_activity: {
    stage: "transcribing",
    detail: "Zoom · transcribing locally · syncing a durable inbox window",
    recording: true,
  },
  get_config: {
    transcribe_backend: "local",
    aws_bucket: null,
    language: "en-US",
    aws_profile: null,
    aws_region: "us-east-1",
    local_threads: 8,
    local_asr_engine: "ggml",
    local_ggml_available: true,
    local_diarize_far_end: true,
    local_embedding_model: "titanet",
    aec_enabled: true,
    retention_days: 7,
    live_filing: true,
    live_buffer_minutes: 1,
    env_managed: [],
  },
  get_backends: [
    { id: "local", label: "Local · private", compiled_in: true },
    { id: "aws", label: "AWS Transcribe", compiled_in: true },
  ],
  set_config: null,
  get_hosted_settings: hostedSettings,
  patch_hosted_settings: { status: "unchanged", settings: hostedSettings },
  update_hosted_steering: { status: "unchanged", settings: hostedSettings },
  replace_hosted_word_bank: { status: "unchanged", settings: hostedSettings },
  update_hosted_provider_scope: { status: "unchanged", settings: hostedSettings },
  refresh_hosted_provider: openAiDirect,
  list_aws_credential_options: {
    profiles: ["default", "corti-screenshot", "corti-sso"],
  },
  set_bedrock_credential_mode: { status: "unchanged", settings: hostedSettings },
  prompt_for_provider_secret: "stored",
  clear_provider_secret: null,
  set_hosted_pinned_question: { status: "unchanged", settings: hostedSettings },
  get_hosted_assistant: {
    pinned_run_count: 0,
    pinned: null,
    exchanges: [],
  },
  submit_hosted_question: "synthetic-question-call",
  cancel_hosted_question: null,
  get_aws_status: {
    profiles: ["default"],
    selected_profile: null,
    configured_region: "us-east-1",
    profile_locked: false,
    region_locked: false,
    env_access_key_id: null,
    env_has_secret: false,
    env_session_token: false,
    env_profile: null,
    env_region: null,
    source: "AWS SDK credential chain",
  },
  get_embedding_models: [
    {
      id: "titanet",
      label: "TitaNet speaker embedding",
      present: true,
      on_disk_bytes: 24_400_000,
      download_bytes: 24_400_000,
      diarize_only: true,
    },
  ],
  get_paths: {
    recordings_dir: "~/Library/Caches/corti/recordings",
    recordings_bytes: 28_120_000,
    models_dir: "~/Library/Caches/corti/models",
    models_bytes: 804_300_000,
  },
  get_models_status: [
    {
      id: "parakeet-tdt-0.6b-v3-ggml-q8",
      label: "Parakeet TDT 0.6B v3 · Metal Q8",
      present: true,
      on_disk_bytes: 742_000_000,
      download_bytes: 742_000_000,
      diarize_only: false,
    },
    {
      id: "silero-vad",
      label: "Silero VAD",
      present: true,
      on_disk_bytes: 2_300_000,
      download_bytes: 2_300_000,
      diarize_only: false,
    },
    {
      id: "titanet",
      label: "TitaNet speaker embedding",
      present: true,
      on_disk_bytes: 24_400_000,
      download_bytes: 24_400_000,
      diarize_only: true,
    },
  ],
  reveal_path: null,
  set_models_dir: null,
  download_model: null,
};

export const syntheticLiveSettings = {
  ...hostedSettings,
  state_revision: 24,
  show_live_metrics_by_default: true,
  control: {
    ...hostedSettings.control,
    process_epoch: 71,
    session_generation: 3,
    master_enabled: true,
    live: {
      enabled: true,
      revision: 6,
      selection: {
        provider: "openai",
        transport: "openai_api",
        model: "fixture-live-v1",
        cache_policy: { local: "reusable", provider: "off" },
      },
    },
    final_lane: {
      ...hostedSettings.control.final_lane,
      enabled: true,
    },
    questions: {
      ...hostedSettings.control.questions,
      enabled: true,
    },
  },
};

export const syntheticLiveTranscript = {
  protocol_version: 2,
  process_epoch: 71,
  session_generation: 3,
  revision: 42,
  session_id: "synthetic-live-session",
  mode: "call",
  status: "listening",
  title: "Synthetic planning call · live transcript",
  detail: "Recording · raw rows publish before optional cleanup",
  active: true,
  evicted_lines: 0,
  retained_from_seq: 1,
  lines: [
    {
      seq: 1,
      row_id: "synthetic-row-1",
      speaker: "Me",
      start_sec: 3.2,
      end_sec: 7.8,
      text: "The synthetic inbox note is ready for this fixture.",
      clean_text: "The synthetic inbox note is ready for this fixture.",
      rewrite_state: "clean",
      commit_epoch: 38,
    },
    {
      seq: 2,
      row_id: "synthetic-row-2",
      speaker: "Them 1",
      start_sec: 8.4,
      end_sec: 13.6,
      text: "We should ship teh fixture Friday.",
      clean_text: "We should ship the fixture on Friday.",
      rewrite_state: "clean",
      commit_epoch: 42,
    },
    {
      seq: 3,
      row_id: "synthetic-row-3",
      speaker: "Me",
      start_sec: 14.1,
      end_sec: 20.3,
      text: "Raw text appears first while the next cleanup is queued.",
      clean_text: null,
      rewrite_state: "raw",
      commit_epoch: 0,
    },
  ],
};

export const syntheticAssistant = {
  pinned_run_count: 3,
  pinned: {
    call_id: "synthetic-pinned-3",
    as_of_revision: 42,
    status: "completed",
    error: null,
    question: "What decision is currently supported?",
    answer: "The fixture supports a Friday release after the deterministic checks pass.",
    cost_label: "Estimated $0.0012",
    context_truncated: false,
    usage: {
      input_tokens: 140,
      output_tokens: 18,
      cached_read_tokens: 96,
      cached_write_tokens: null,
      reasoning_tokens: null,
      usage_complete: true,
    },
    cache: "provider_read",
  },
  exchanges: [
    {
      call_id: "synthetic-question-1",
      as_of_revision: 39,
      status: "completed",
      error: null,
      question: "What remains before release?",
      answer: "Run the deterministic UI checks and review the raw fallback.",
      cost_label: "Local cache · no provider request",
      context_truncated: false,
      cache: "local",
    },
    {
      call_id: "synthetic-question-2",
      as_of_revision: 42,
      status: "running",
      error: null,
      question: "Is the newest row clean yet?",
      answer: null,
      cost_label: null,
      context_truncated: false,
      cache: "none",
    },
    {
      call_id: "synthetic-question-3",
      as_of_revision: 40,
      status: "canceled",
      error: "canceled",
      question: "Cancel this synthetic question.",
      answer: null,
      cost_label: "Cost unavailable",
      context_truncated: false,
      cache: "none",
    },
    {
      call_id: "synthetic-question-4",
      as_of_revision: 41,
      status: "failed",
      error: "timeout",
      question: "Exercise the safe fallback state.",
      answer: null,
      cost_label: "Cost unavailable",
      context_truncated: true,
      cache: "none",
    },
  ],
};

export const syntheticLiveOverrides: Record<string, unknown> = {
  get_live_transcript: syntheticLiveTranscript,
  get_hosted_settings: syntheticLiveSettings,
  get_hosted_assistant: syntheticAssistant,
  patch_hosted_settings: { status: "unchanged", settings: syntheticLiveSettings },
  update_hosted_steering: { status: "unchanged", settings: syntheticLiveSettings },
  set_hosted_pinned_question: { status: "unchanged", settings: syntheticLiveSettings },
};

export const syntheticLiveTerminal = {
  event: "terminal",
  call_id: "synthetic-live-call-42",
  recording_id: "synthetic-live-session",
  request_group_id: "synthetic-live-group-42",
  target_id: "synthetic-target-42",
  lane: "live",
  attempt_no: 1,
  fence: {
    process_epoch: 71,
    session_generation: 3,
    transcript_revision: 42,
    control_revision: 8,
    lane_revision: 6,
    steering_revision: 4,
    bank_revision: 7,
    question_revision: null,
  },
  provider: "openai",
  transport: "openai_api",
  model: "fixture-live-v1",
  support_tier: "documented",
  adapter_version: 1,
  prompt_version: 2,
  output_schema_version: 1,
  outcome: "completed",
  error: null,
  provider_request_sent: true,
  late_content_discarded: false,
  cache: "provider_read",
  usage: {
    input_tokens: 180,
    output_tokens: 21,
    cached_read_tokens: 128,
    cached_write_tokens: null,
    reasoning_tokens: null,
    usage_complete: true,
  },
  cost: {
    billing_basis: "metered_estimate",
    cost_micros: 1200,
    currency: "USD",
    pricing_catalog_version: "synthetic-tariff-v1",
    tariff_id: "synthetic-live-rate",
    tariff_effective_at_unix_ms: 1787000000000,
  },
  latency: {
    queue_us: 5000,
    auth_us: null,
    cache_lookup_us: 900,
    connect_us: 14000,
    ttfb_us: 45000,
    ttft_us: 80000,
    stream_us: 210000,
    parse_us: 1500,
    cache_commit_us: 2200,
    total_us: 313600,
  },
  queued_at_unix_ms: 1787000000000,
  dispatched_at_unix_ms: 1787000000005,
  completed_at_unix_ms: 1787000000314,
};

export const syntheticGapSnapshot = {
  ...syntheticLiveTranscript,
  revision: 44,
  lines: [
    ...syntheticLiveTranscript.lines,
    {
      seq: 4,
      row_id: "synthetic-row-4",
      speaker: "Them 1",
      start_sec: 21,
      end_sec: 23,
      text: "The repaired synthetic row is contiguous again.",
      clean_text: null,
      rewrite_state: "raw",
      commit_epoch: 0,
    },
  ],
};

export const syntheticGapEvent = {
  ...syntheticGapSnapshot,
  from_revision: 43,
  reset: false,
  line: syntheticGapSnapshot.lines[3],
  lines: undefined,
};

export const syntheticVertexNotice = {
  event: "notice",
  role: "alert",
  visible_message: "gcloud token isn't armed",
  episode: 4,
};
