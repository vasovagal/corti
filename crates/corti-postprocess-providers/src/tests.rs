use std::{
    collections::VecDeque,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use corti_postprocess::{
    CachePolicy, CancellationReason, ConnectionScopeId, DigestKey, HostedRequest, KnownTransport,
    Lane, LocalCacheMode, ModelId, MonotonicDeadline, ProcessEpoch, PromptTask, ProviderCacheKey,
    ProviderCacheKeyMaterial, ProviderCacheMode, ProviderEvent, ProviderEventKind, ProviderId,
    RequestFence, RequestGroupId, RowId, SupportTier, TranscriptRow, TransportId, WordBankDocument,
};
use serde_json::{Value, json};

use super::*;

const OPENAI_MODEL_LIST: &str = r#"{
  "object":"list",
  "data":[
    {"id":"gpt-5.6-luna","object":"model","created":1,"owned_by":"openai","shutdown_date":null},
    {"id":"gpt-5.6","object":"model","created":1,"owned_by":"openai","shutdown_date":null}
  ]
}"#;

const ANTHROPIC_MODEL_ID: &str = "claude-haiku-4-5-20251001";
const VERTEX_MODEL_ID: &str = "gemini-synthetic-001";
const VERTEX_PROJECT_ID: &str = "synthetic-project";
const VERTEX_REGION: &str = "us-central1";
const VERTEX_QUOTA_PROJECT_ID: &str = "synthetic-quota-project";
const ANTHROPIC_MODEL_LIST: &str = r#"{
  "data":[{
    "id":"claude-haiku-4-5-20251001",
    "type":"model",
    "display_name":"Claude Haiku 4.5",
    "created_at":"2025-10-01T00:00:00Z",
    "max_input_tokens":200000,
    "max_tokens":64000,
    "capabilities":{"structured_outputs":{"supported":true}}
  }],
  "first_id":"claude-haiku-4-5-20251001",
  "has_more":false,
  "last_id":"claude-haiku-4-5-20251001"
}"#;

#[derive(Clone)]
struct FakeClock {
    now: Arc<AtomicU64>,
}

impl FakeClock {
    fn new(now: u64) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(now)),
        }
    }
}

impl Clock for FakeClock {
    fn monotonic_micros(&self) -> u64 {
        self.now.fetch_add(10, Ordering::SeqCst)
    }
}

#[derive(Default)]
struct CredentialState {
    resolves: AtomicUsize,
    rejected: AtomicUsize,
}

struct FakeCredentials {
    state: Arc<CredentialState>,
}

impl ApiKeySource for FakeCredentials {
    fn resolve(&mut self) -> Result<ApiKey, CredentialError> {
        self.state.resolves.fetch_add(1, Ordering::SeqCst);
        ApiKey::new("synthetic-fixture-api-key").map_err(|_| CredentialError::Unavailable)
    }

    fn mark_rejected(&mut self) {
        self.state.rejected.fetch_add(1, Ordering::SeqCst);
    }
}

struct FakeAdcCredentials {
    state: Arc<CredentialState>,
}

impl AdcAccessTokenSource for FakeAdcCredentials {
    fn resolve_access_token(&mut self) -> Result<AdcAccessToken, CredentialError> {
        self.state.resolves.fetch_add(1, Ordering::SeqCst);
        let token = AccessToken::new("synthetic-fixture-adc-access-token")
            .map_err(|_| CredentialError::Unavailable)?;
        Ok(AdcAccessToken::new(token, Some(9_999_999)))
    }

    fn mark_rejected(&mut self) {
        self.state.rejected.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct FakeTransportHandle {
    state: Arc<Mutex<FakeTransportState>>,
}

impl FakeTransportHandle {
    fn new(scripts: impl IntoIterator<Item = Script>) -> (Self, FakeTransport) {
        let state = Arc::new(Mutex::new(FakeTransportState {
            scripts: scripts.into_iter().collect(),
            captured: Vec::new(),
        }));
        (
            Self {
                state: state.clone(),
            },
            FakeTransport { state },
        )
    }

    fn captured(&self) -> Vec<CapturedRequest> {
        self.state.lock().unwrap().captured.clone()
    }

    fn pending_scripts(&self) -> usize {
        self.state.lock().unwrap().scripts.len()
    }
}

struct FakeTransport {
    state: Arc<Mutex<FakeTransportState>>,
}

struct FakeTransportState {
    scripts: VecDeque<Script>,
    captured: Vec<CapturedRequest>,
}

enum Script {
    Response {
        status: u16,
        content_type: &'static str,
        chunks: Vec<Vec<u8>>,
    },
    Error(TransportError),
}

impl Script {
    fn json(body: impl AsRef<[u8]>) -> Self {
        Self::Response {
            status: 200,
            content_type: "application/json",
            chunks: vec![body.as_ref().to_vec()],
        }
    }

    fn sse(chunks: Vec<Vec<u8>>) -> Self {
        Self::Response {
            status: 200,
            content_type: "text/event-stream; charset=utf-8",
            chunks,
        }
    }

    fn status(status: u16, body: impl AsRef<[u8]>) -> Self {
        Self::Response {
            status,
            content_type: "application/json",
            chunks: vec![body.as_ref().to_vec()],
        }
    }
}

#[derive(Clone)]
struct CapturedRequest {
    method: HttpMethod,
    url: String,
    public_headers: Vec<(String, String)>,
    secret_headers: Vec<String>,
    body: Option<Value>,
}

impl fmt::Debug for CapturedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field(
                "public_header_names",
                &self
                    .public_headers
                    .iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>(),
            )
            .field("secret_headers", &self.secret_headers)
            .field("body", &self.body.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl HttpTransport for FakeTransport {
    fn send(
        &mut self,
        request: &HttpRequest,
        cancel: &CancellationToken,
    ) -> Result<HttpResponse, TransportError> {
        if cancel.is_cancelled() {
            return Err(TransportError::not_sent(TransportErrorKind::Canceled));
        }
        let mut public_headers = Vec::new();
        let mut secret_headers = Vec::new();
        for header in request.headers() {
            if header.value().is_secret() {
                // Assert the trusted transport can access the synthetic value, but never retain it.
                assert!(!header.value().expose_to_transport().is_empty());
                secret_headers.push(header.name().to_owned());
            } else {
                public_headers.push((
                    header.name().to_owned(),
                    header.value().expose_to_transport().to_owned(),
                ));
            }
        }
        let body = (!request.body().is_empty())
            .then(|| serde_json::from_slice(request.body()).expect("adapter request is JSON"));
        let mut state = self.state.lock().unwrap();
        state.captured.push(CapturedRequest {
            method: request.method(),
            url: request.url().as_str().to_owned(),
            public_headers,
            secret_headers,
            body,
        });
        match state.scripts.pop_front().expect("unexpected HTTP request") {
            Script::Response {
                status,
                content_type,
                chunks,
            } => Ok(HttpResponse::new(
                status,
                [("content-type".to_owned(), content_type.to_owned())],
                Box::new(ChunkBody {
                    chunks: chunks.into(),
                    canceled: false,
                }),
            )),
            Script::Error(error) => Err(error),
        }
    }
}

struct ChunkBody {
    chunks: VecDeque<Vec<u8>>,
    canceled: bool,
}

impl HttpResponseBody for ChunkBody {
    fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        if self.canceled {
            return Ok(None);
        }
        Ok(self.chunks.pop_front())
    }

    fn cancel(&mut self) {
        self.canceled = true;
    }
}

#[derive(Default)]
struct CollectingSink {
    events: Mutex<Vec<ProviderEvent>>,
}

impl CollectingSink {
    fn events(&self) -> Vec<ProviderEvent> {
        self.events.lock().unwrap().clone()
    }

    fn text(&self) -> String {
        self.events()
            .into_iter()
            .filter_map(|event| match event.kind {
                ProviderEventKind::TextDelta(delta) => Some(delta.into_string()),
                _ => None,
            })
            .collect()
    }
}

impl ProviderEventSink for CollectingSink {
    fn emit(&self, event: ProviderEvent) {
        self.events.lock().unwrap().push(event);
    }
}

struct CancelingSink {
    events: Mutex<Vec<ProviderEvent>>,
    cancel: CancellationToken,
}

impl ProviderEventSink for CancelingSink {
    fn emit(&self, event: ProviderEvent) {
        let should_cancel = matches!(event.kind, ProviderEventKind::TextDelta(_));
        self.events.lock().unwrap().push(event);
        if should_cancel {
            self.cancel.cancel(CancellationReason::Explicit);
        }
    }
}

struct FixedCacheKey(ProviderCacheKey);

impl ProviderCacheKeySource for FixedCacheKey {
    fn key_for(&mut self, _request: &HostedRequest) -> Result<ProviderCacheKey, CacheKeyError> {
        Ok(self.0.clone())
    }
}

fn credentials() -> (Arc<CredentialState>, Box<dyn ApiKeySource>) {
    let state = Arc::new(CredentialState::default());
    (
        state.clone(),
        Box::new(FakeCredentials { state }) as Box<dyn ApiKeySource>,
    )
}

fn scope() -> ProviderScope {
    ProviderScope {
        connection_scope_id: ConnectionScopeId::new("synthetic-scope-id").unwrap(),
        region: None,
    }
}

fn vertex_scope() -> ProviderScope {
    ProviderScope {
        connection_scope_id: ConnectionScopeId::new("synthetic-vertex-scope-id").unwrap(),
        region: Some(VERTEX_REGION.to_owned()),
    }
}

fn hosted_request(
    transport: KnownTransport,
    model: &str,
    cache: ProviderCacheMode,
) -> HostedRequest {
    let target = TranscriptRow {
        row_id: RowId::new("r-000001").unwrap(),
        speaker: "Speaker A".into(),
        start_ms: 10,
        end_ms: 20,
        text: "Synthetic input sentence.".into(),
    };
    let bank = WordBankDocument::from_entries(1, ["SyntheticTerm"]).unwrap();
    let prompt = corti_postprocess::CanonicalPrompt::rewrite(
        &bank,
        "Use concise synthetic prose.",
        &[],
        std::slice::from_ref(&target),
    );
    let descriptor = transport.descriptor();
    HostedRequest {
        call_id: corti_postprocess::CallId::new("synthetic-call-id").unwrap(),
        group_id: RequestGroupId::new("synthetic-group-id").unwrap(),
        target_id: None,
        lane: Lane::Final,
        fence: RequestFence {
            process_epoch: ProcessEpoch(1),
            session_generation: 2,
            transcript_revision: 3,
            control_revision: 4,
            lane_revision: 5,
            steering_revision: 6,
            bank_revision: 7,
            question_revision: None,
        },
        provider: descriptor.provider,
        transport: descriptor.transport,
        model: ModelId::new(model).unwrap(),
        targets: vec![target],
        context: Vec::new(),
        prompt,
        deadline: MonotonicDeadline(10_000_000),
        cache_policy: CachePolicy {
            local: LocalCacheMode::MemoryOnly,
            provider: cache,
        },
    }
}

fn provider_cache_key() -> ProviderCacheKey {
    let provider = ProviderId::new("openai").unwrap();
    let transport = TransportId::new("openai_api").unwrap();
    let scope = ConnectionScopeId::new("synthetic-scope-id").unwrap();
    let model = ModelId::new(OPENAI_LUNA_MODEL_ID).unwrap();
    let material = ProviderCacheKeyMaterial {
        provider: &provider,
        transport: &transport,
        support_tier: SupportTier::Documented,
        connection_scope_id: &scope,
        region: None,
        exact_model_id: &model,
        adapter_version: OPENAI_RESPONSES_ADAPTER_VERSION,
        prompt_template_version: 1,
        output_schema_version: 1,
        prompt_task: PromptTask::Rewrite,
        provider_cache_mode: ProviderCacheMode::ExplicitStablePrefix,
        word_bank_canonical_digest: "synthetic-bank-digest",
    };
    ProviderCacheKey::derive(&DigestKey::new([17; 32]), &material)
}

fn sse_event(name: &str, payload: Value) -> Vec<u8> {
    format!(
        "event: {name}\ndata: {}\n\n",
        serde_json::to_string(&payload).unwrap()
    )
    .into_bytes()
}

fn openai_stream() -> Vec<Vec<u8>> {
    let output = r#"{"schema":1,"replacements":[{"row_id":"r-000001","text":"Synthetic corrected sentence."}]}"#;
    let split = output.find("corrected").unwrap();
    let mut wire = Vec::new();
    wire.extend(sse_event(
        "response.created",
        json!({"type":"response.created","response":{"model":OPENAI_LUNA_MODEL_ID}}),
    ));
    wire.extend(sse_event(
        "response.output_text.delta",
        json!({"type":"response.output_text.delta","delta":&output[..split]}),
    ));
    wire.extend(sse_event(
        "response.output_text.delta",
        json!({"type":"response.output_text.delta","delta":&output[split..]}),
    ));
    wire.extend(sse_event(
        "response.output_text.done",
        json!({"type":"response.output_text.done","text":output}),
    ));
    wire.extend(sse_event(
        "response.completed",
        json!({
            "type":"response.completed",
            "response":{
                "model":OPENAI_LUNA_MODEL_ID,
                "status":"completed",
                "usage":{
                    "input_tokens":20,
                    "output_tokens":9,
                    "input_tokens_details":{"cached_tokens":8,"cache_write_tokens":4},
                    "output_tokens_details":{"reasoning_tokens":2}
                }
            }
        }),
    ));
    // Split inside both SSE framing and JSON strings.
    vec![
        wire[..37].to_vec(),
        wire[37..113].to_vec(),
        wire[113..].to_vec(),
    ]
}

fn vertex_stream() -> Vec<Vec<u8>> {
    let output = r#"{"schema":1,"replacements":[{"row_id":"r-000001","text":"Synthetic corrected sentence."}]}"#;
    let split = output.find("corrected").unwrap();
    let mut wire = Vec::new();
    wire.extend(
        format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "candidates":[{
                    "index":0,
                    "content":{"role":"model","parts":[{"text":&output[..split]}]}
                }],
                "modelVersion":VERTEX_MODEL_ID
            }))
            .unwrap()
        )
        .into_bytes(),
    );
    wire.extend(
        format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "candidates":[{
                    "index":0,
                    "content":{"role":"model","parts":[{"text":&output[split..]}]},
                    "finishReason":"STOP"
                }],
                "usageMetadata":{
                    "promptTokenCount":31,
                    "candidatesTokenCount":11,
                    "cachedContentTokenCount":13,
                    "thoughtsTokenCount":2,
                    "totalTokenCount":44
                },
                "modelVersion":VERTEX_MODEL_ID
            }))
            .unwrap()
        )
        .into_bytes(),
    );
    wire.extend_from_slice(b"data: [DONE]\n\n");
    vec![
        wire[..23].to_vec(),
        wire[23..151].to_vec(),
        wire[151..].to_vec(),
    ]
}

fn anthropic_stream() -> Vec<Vec<u8>> {
    let output = r#"{"schema":1,"replacements":[{"row_id":"r-000001","text":"Synthetic corrected sentence."}]}"#;
    let split = output.find("corrected").unwrap();
    let mut wire = Vec::new();
    wire.extend(sse_event(
        "message_start",
        json!({
            "type":"message_start",
            "message":{
                "model":ANTHROPIC_MODEL_ID,
                "usage":{
                    "input_tokens":12,
                    "output_tokens":1,
                    "cache_creation_input_tokens":5,
                    "cache_read_input_tokens":3,
                    "output_tokens_details":{"thinking_tokens":0}
                }
            }
        }),
    ));
    wire.extend(sse_event(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    ));
    wire.extend(sse_event("ping", json!({"type":"ping"})));
    wire.extend(sse_event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":&output[..split]}}),
    ));
    wire.extend(sse_event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":&output[split..]}}),
    ));
    wire.extend(sse_event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    ));
    wire.extend(sse_event(
        "message_delta",
        json!({
            "type":"message_delta",
            "delta":{"stop_reason":"end_turn","stop_sequence":null},
            "usage":{
                "input_tokens":12,
                "output_tokens":9,
                "cache_creation_input_tokens":5,
                "cache_read_input_tokens":3,
                "output_tokens_details":{"thinking_tokens":2}
            }
        }),
    ));
    wire.extend(sse_event("message_stop", json!({"type":"message_stop"})));
    vec![
        wire[..19].to_vec(),
        wire[19..207].to_vec(),
        wire[207..].to_vec(),
    ]
}

fn openai_adapter(scripts: Vec<Script>) -> (FakeTransportHandle, OpenAiResponsesAdapter) {
    let (handle, transport) = FakeTransportHandle::new(scripts);
    let (_, credentials) = credentials();
    let adapter = OpenAiResponsesAdapter::new(
        Box::new(transport),
        Box::new(FakeClock::new(100)),
        credentials,
    )
    .with_cache_key_source(Box::new(FixedCacheKey(provider_cache_key())));
    (handle, adapter)
}

fn anthropic_adapter(scripts: Vec<Script>) -> (FakeTransportHandle, AnthropicMessagesAdapter) {
    let (handle, transport) = FakeTransportHandle::new(scripts);
    let (_, credentials) = credentials();
    let adapter = AnthropicMessagesAdapter::new(
        Box::new(transport),
        Box::new(FakeClock::new(100)),
        credentials,
    );
    (handle, adapter)
}

fn vertex_adapter(
    scripts: Vec<Script>,
) -> (FakeTransportHandle, Arc<CredentialState>, VertexRestAdapter) {
    let (handle, transport) = FakeTransportHandle::new(scripts);
    let state = Arc::new(CredentialState::default());
    let metadata = VertexProjectMetadata::new(
        VERTEX_PROJECT_ID,
        VERTEX_REGION,
        Some(VERTEX_QUOTA_PROJECT_ID.to_owned()),
    )
    .unwrap();
    let model =
        VertexModel::new(ModelId::new(VERTEX_MODEL_ID).unwrap(), 1_000_000, 64_000).unwrap();
    let adapter = VertexRestAdapter::new(
        Box::new(transport),
        Box::new(FakeClock::new(100)),
        Box::new(FakeAdcCredentials {
            state: state.clone(),
        }),
        metadata,
        [model],
    )
    .unwrap();
    (handle, state, adapter)
}

#[test]
fn openai_responses_shape_stream_usage_cache_and_exact_catalog() {
    let (handle, mut adapter) = openai_adapter(vec![
        Script::json(OPENAI_MODEL_LIST),
        Script::sse(openai_stream()),
    ]);
    let catalog = adapter.catalog(&scope()).unwrap();
    assert_eq!(catalog.models.len(), 1);
    let descriptor = &catalog.models[0];
    assert_eq!(descriptor.exact_model_id.as_str(), OPENAI_LUNA_MODEL_ID);
    assert_eq!(
        descriptor.max_context_tokens,
        OPENAI_LUNA_MAX_CONTEXT_TOKENS
    );
    assert_eq!(descriptor.max_output_tokens, OPENAI_LUNA_MAX_OUTPUT_TOKENS);
    assert!(descriptor.capabilities.explicit_prefix_cache);
    assert!(!descriptor.benchmarked_for_live);

    let request = hosted_request(
        KnownTransport::OpenAiDirect,
        OPENAI_LUNA_MODEL_ID,
        ProviderCacheMode::ExplicitStablePrefix,
    );
    let sink = CollectingSink::default();
    let terminal = adapter
        .execute(&request, &CancellationToken::new(), &sink)
        .unwrap();
    assert_eq!(
        sink.text(),
        r#"{"schema":1,"replacements":[{"row_id":"r-000001","text":"Synthetic corrected sentence."}]}"#
    );
    assert_eq!(terminal.usage.input_tokens, Some(20));
    assert_eq!(terminal.usage.output_tokens, Some(9));
    assert_eq!(terminal.usage.cached_read_tokens, Some(8));
    assert_eq!(terminal.usage.cached_write_tokens, Some(4));
    assert_eq!(terminal.usage.reasoning_tokens, Some(2));
    assert!(terminal.usage.usage_complete);
    assert_eq!(
        terminal.cache,
        corti_postprocess::CacheObservation::ProviderRead
    );

    let captured = handle.captured();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].method, HttpMethod::Get);
    assert!(captured[0].url.ends_with("/v1/models"));
    assert_eq!(captured[0].secret_headers, ["authorization"]);
    let body = captured[1].body.as_ref().unwrap();
    assert_eq!(body["model"], OPENAI_LUNA_MODEL_ID);
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["prompt_cache_options"]["mode"], "explicit");
    assert!(body["prompt_cache_key"].as_str().is_some());
    assert_eq!(body["input"].as_array().unwrap().len(), 6);
    assert_eq!(body["input"][0]["role"], "developer");
    assert_eq!(
        body["input"][2]["content"][0]["prompt_cache_breakpoint"]["mode"],
        "explicit"
    );
    assert!(
        body["input"][3]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    );
    assert_eq!(body["text"]["format"]["type"], "json_schema");
    assert_eq!(body["text"]["format"]["strict"], true);
    assert_eq!(captured[1].secret_headers, ["authorization"]);
    let debug = format!("{:?}", captured[1]);
    assert!(!debug.contains("Synthetic input"));
    assert!(!debug.contains("synthetic-fixture-api-key"));
}

#[test]
fn anthropic_messages_shape_reconciles_terminal_usage_and_cache_classes() {
    let (handle, mut adapter) = anthropic_adapter(vec![
        Script::json(ANTHROPIC_MODEL_LIST),
        Script::sse(anthropic_stream()),
    ]);
    let catalog = adapter.catalog(&scope()).unwrap();
    assert_eq!(catalog.models.len(), 1);
    assert_eq!(
        catalog.models[0].exact_model_id.as_str(),
        ANTHROPIC_MODEL_ID
    );
    assert_eq!(catalog.models[0].max_context_tokens, 200_000);
    assert_eq!(catalog.models[0].max_output_tokens, 64_000);

    let request = hosted_request(
        KnownTransport::AnthropicDirect,
        ANTHROPIC_MODEL_ID,
        ProviderCacheMode::ExplicitStablePrefix,
    );
    let sink = CollectingSink::default();
    let terminal = adapter
        .execute(&request, &CancellationToken::new(), &sink)
        .unwrap();
    assert_eq!(terminal.usage.input_tokens, Some(12));
    assert_eq!(terminal.usage.output_tokens, Some(9));
    assert_eq!(terminal.usage.cached_read_tokens, Some(3));
    assert_eq!(terminal.usage.cached_write_tokens, Some(5));
    assert_eq!(terminal.usage.reasoning_tokens, Some(2));
    assert!(terminal.usage.usage_complete);
    let events = sink.events();
    assert!(events.iter().any(|event| matches!(
        event.kind,
        ProviderEventKind::UsageProvisional(usage) if usage.output_tokens == Some(1) && !usage.usage_complete
    )));

    let captured = handle.captured();
    assert_eq!(captured[0].secret_headers, ["x-api-key"]);
    assert!(captured[0].url.contains("limit=1000"));
    let body = captured[1].body.as_ref().unwrap();
    assert_eq!(body["model"], ANTHROPIC_MODEL_ID);
    assert_eq!(body["stream"], true);
    assert_eq!(body["system"].as_array().unwrap().len(), 3);
    assert_eq!(body["system"][2]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["system"][2]["cache_control"]["ttl"], "5m");
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert!(body.get("tools").is_none());
    assert_eq!(captured[1].secret_headers, ["x-api-key"]);
}

#[test]
fn vertex_rest_shape_normalizes_stream_usage_and_quota_metadata() {
    let (handle, credential_state, mut adapter) =
        vertex_adapter(vec![Script::sse(vertex_stream())]);
    let catalog = adapter.catalog(&vertex_scope()).unwrap();
    assert_eq!(catalog.models.len(), 1);
    let descriptor = &catalog.models[0];
    assert_eq!(descriptor.exact_model_id.as_str(), VERTEX_MODEL_ID);
    assert_eq!(descriptor.region.as_deref(), Some(VERTEX_REGION));
    assert!(descriptor.capabilities.implicit_cache_may_apply);
    assert!(!descriptor.capabilities.explicit_prefix_cache);
    assert!(!descriptor.benchmarked_for_live);

    let request = hosted_request(
        KnownTransport::VertexDirect,
        VERTEX_MODEL_ID,
        ProviderCacheMode::UnavoidableImplicit,
    );
    let sink = CollectingSink::default();
    let terminal = adapter
        .execute(&request, &CancellationToken::new(), &sink)
        .unwrap();
    assert_eq!(terminal.usage.input_tokens, Some(31));
    assert_eq!(terminal.usage.output_tokens, Some(11));
    assert_eq!(terminal.usage.cached_read_tokens, Some(13));
    assert_eq!(terminal.usage.cached_write_tokens, None);
    assert_eq!(terminal.usage.reasoning_tokens, Some(2));
    assert!(terminal.usage.usage_complete);
    assert_eq!(
        terminal.cache,
        corti_postprocess::CacheObservation::ProviderImplicit
    );
    assert_eq!(credential_state.resolves.load(Ordering::SeqCst), 1);

    let captured = handle.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].method, HttpMethod::Post);
    assert_eq!(
        captured[0].url,
        format!(
            "https://{VERTEX_REGION}-aiplatform.googleapis.com/v1/projects/{VERTEX_PROJECT_ID}/locations/{VERTEX_REGION}/publishers/google/models/{VERTEX_MODEL_ID}:streamGenerateContent?alt=sse"
        )
    );
    assert_eq!(captured[0].secret_headers, ["authorization"]);
    assert!(captured[0].public_headers.iter().any(|(name, value)| {
        name == "x-goog-user-project" && value == VERTEX_QUOTA_PROJECT_ID
    }));
    let body = captured[0].body.as_ref().unwrap();
    assert_eq!(
        body["systemInstruction"]["parts"].as_array().unwrap().len(),
        3
    );
    assert_eq!(body["contents"][0]["role"], "user");
    assert_eq!(body["contents"][0]["parts"].as_array().unwrap().len(), 3);
    assert_eq!(body["generationConfig"]["candidateCount"], 1);
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 8 * 1024);
    assert_eq!(
        body["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        body["generationConfig"]["responseJsonSchema"]["type"],
        "object"
    );
    assert!(body.get("tools").is_none());
    assert!(sink.events().iter().any(|event| matches!(
        event.kind,
        ProviderEventKind::CacheObserved(corti_postprocess::CacheObservation::ProviderImplicit)
    )));
}

#[test]
fn vertex_service_errors_are_sanitized_and_do_not_confuse_quota_with_auth() {
    let provider_body = r#"{
        "error":{
            "code":429,
            "status":"RESOURCE_EXHAUSTED",
            "message":"synthetic personal provider text must not escape",
            "details":[{"reason":"QUOTA_EXCEEDED"}]
        }
    }"#;
    let (_, credential_state, mut adapter) =
        vertex_adapter(vec![Script::status(429, provider_body)]);
    adapter.catalog(&vertex_scope()).unwrap();
    let request = hosted_request(
        KnownTransport::VertexDirect,
        VERTEX_MODEL_ID,
        ProviderCacheMode::UnavoidableImplicit,
    );
    let error = adapter
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::Quota);
    assert_eq!(credential_state.rejected.load(Ordering::SeqCst), 0);
    let rendered = format!("{error:?}");
    assert!(!rendered.contains("personal provider text"));
    assert!(!rendered.contains(VERTEX_PROJECT_ID));

    let (_, credential_state, mut adapter) = vertex_adapter(vec![Script::status(
        401,
        r#"{"error":{"code":401,"status":"UNAUTHENTICATED","message":"do not retain"}}"#,
    )]);
    adapter.catalog(&vertex_scope()).unwrap();
    let error = adapter
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::AuthRejected);
    assert_eq!(credential_state.rejected.load(Ordering::SeqCst), 1);
}

#[test]
fn vertex_metadata_and_cache_policy_fail_closed_before_egress() {
    assert!(VertexProjectMetadata::new("project\nforged", VERTEX_REGION, None).is_err());
    assert!(VertexProjectMetadata::new(VERTEX_PROJECT_ID, "region/forged", None).is_err());
    assert!(
        VertexProjectMetadata::new(
            VERTEX_PROJECT_ID,
            VERTEX_REGION,
            Some("quota\rforged".to_owned())
        )
        .is_err()
    );

    let (handle, _, mut adapter) = vertex_adapter(Vec::new());
    assert_eq!(
        adapter.catalog(&scope()).unwrap_err().code,
        corti_postprocess::ErrorCode::PolicyBlocked
    );
    adapter.catalog(&vertex_scope()).unwrap();
    let request = hosted_request(
        KnownTransport::VertexDirect,
        VERTEX_MODEL_ID,
        ProviderCacheMode::Off,
    );
    let error = adapter
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::PolicyBlocked);
    assert!(handle.captured().is_empty());
}

#[test]
fn openai_explicit_cache_requires_an_injected_opaque_cache_key() {
    let (handle, transport) = FakeTransportHandle::new([Script::json(OPENAI_MODEL_LIST)]);
    let (_, credential_source) = credentials();
    let mut adapter = OpenAiResponsesAdapter::new(
        Box::new(transport),
        Box::new(FakeClock::new(100)),
        credential_source,
    );
    adapter.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::OpenAiDirect,
        OPENAI_LUNA_MODEL_ID,
        ProviderCacheMode::ExplicitStablePrefix,
    );
    let error = adapter
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::Cache);
    assert_eq!(
        handle.captured().len(),
        1,
        "cache failure must prevent egress"
    );
}

#[test]
fn provider_cache_off_never_marks_the_dynamic_or_stable_anthropic_blocks() {
    let (openai_handle, mut openai) = openai_adapter(vec![
        Script::json(OPENAI_MODEL_LIST),
        Script::sse(openai_stream()),
    ]);
    openai.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::OpenAiDirect,
        OPENAI_LUNA_MODEL_ID,
        ProviderCacheMode::Off,
    );
    openai
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap();
    let body = openai_handle.captured()[1].body.clone().unwrap();
    assert_eq!(body["prompt_cache_options"]["mode"], "explicit");
    assert!(body.get("prompt_cache_key").is_none());
    assert!(body["input"].as_array().unwrap().iter().all(|message| {
        message["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none()
    }));

    let (anthropic_handle, mut anthropic) = anthropic_adapter(vec![
        Script::json(ANTHROPIC_MODEL_LIST),
        Script::sse(anthropic_stream()),
    ]);
    anthropic.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::AnthropicDirect,
        ANTHROPIC_MODEL_ID,
        ProviderCacheMode::Off,
    );
    anthropic
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap();
    let body = anthropic_handle.captured()[1].body.clone().unwrap();
    assert!(body.get("cache_control").is_none());
    assert!(
        body["system"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block.get("cache_control").is_none())
    );
}

#[test]
fn http_401_rejects_the_injected_key_without_retaining_provider_body() {
    let (handle, transport) = FakeTransportHandle::new([
        Script::json(OPENAI_MODEL_LIST),
        Script::status(401, "synthetic-provider-body-must-not-escape"),
    ]);
    let (credential_state, credential_source) = credentials();
    let mut adapter = OpenAiResponsesAdapter::new(
        Box::new(transport),
        Box::new(FakeClock::new(100)),
        credential_source,
    );
    adapter.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::OpenAiDirect,
        OPENAI_LUNA_MODEL_ID,
        ProviderCacheMode::Off,
    );
    let sink = CollectingSink::default();
    let error = adapter
        .execute(&request, &CancellationToken::new(), &sink)
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::AuthRejected);
    assert_eq!(credential_state.rejected.load(Ordering::SeqCst), 1);
    assert!(!format!("{error:?}").contains("provider-body"));
    assert!(sink.events().iter().any(|event| matches!(
        event.kind,
        ProviderEventKind::Failed {
            code: corti_postprocess::ErrorCode::AuthRejected,
            ..
        }
    )));
    assert_eq!(handle.pending_scripts(), 0);

    let (_, transport) = FakeTransportHandle::new([
        Script::json(ANTHROPIC_MODEL_LIST),
        Script::status(401, "another-synthetic-provider-body-must-not-escape"),
    ]);
    let (credential_state, credential_source) = credentials();
    let mut adapter = AnthropicMessagesAdapter::new(
        Box::new(transport),
        Box::new(FakeClock::new(100)),
        credential_source,
    );
    adapter.catalog(&scope()).unwrap();
    let anthropic_request = hosted_request(
        KnownTransport::AnthropicDirect,
        ANTHROPIC_MODEL_ID,
        ProviderCacheMode::Off,
    );
    let error = adapter
        .execute(
            &anthropic_request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::AuthRejected);
    assert_eq!(credential_state.rejected.load(Ordering::SeqCst), 1);
    assert!(!format!("{error:?}").contains("provider-body"));
}

#[test]
fn retries_once_only_when_transport_proves_request_was_not_sent() {
    let (handle, mut adapter) = openai_adapter(vec![
        Script::json(OPENAI_MODEL_LIST),
        Script::Error(TransportError::not_sent(TransportErrorKind::Network)),
        Script::sse(openai_stream()),
    ]);
    adapter.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::OpenAiDirect,
        OPENAI_LUNA_MODEL_ID,
        ProviderCacheMode::Off,
    );
    adapter
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap();
    assert_eq!(handle.captured().len(), 3);

    let (handle, mut adapter) = openai_adapter(vec![
        Script::json(OPENAI_MODEL_LIST),
        Script::Error(TransportError::new(
            TransportErrorKind::Network,
            RequestDelivery::MayHaveBeenSent,
        )),
        Script::sse(openai_stream()),
    ]);
    adapter.catalog(&scope()).unwrap();
    let error = adapter
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::Network);
    assert_eq!(handle.captured().len(), 2);
    assert_eq!(handle.pending_scripts(), 1, "ambiguous send must not retry");
}

#[test]
fn malformed_or_truncated_streams_fail_closed_with_sanitized_errors() {
    let incomplete = sse_event(
        "response.output_text.delta",
        json!({"type":"response.output_text.delta","delta":"{\"schema\":1"}),
    );
    let (_, mut openai) = openai_adapter(vec![
        Script::json(OPENAI_MODEL_LIST),
        Script::sse(vec![incomplete]),
    ]);
    openai.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::OpenAiDirect,
        OPENAI_LUNA_MODEL_ID,
        ProviderCacheMode::Off,
    );
    let error = openai
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::MalformedOutput);

    let bad_usage = String::from_utf8(openai_stream().concat())
        .unwrap()
        .replace("\"cached_tokens\":8", "\"cached_tokens\":-1")
        .into_bytes();
    let (_, mut openai) = openai_adapter(vec![
        Script::json(OPENAI_MODEL_LIST),
        Script::sse(vec![bad_usage]),
    ]);
    openai.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::OpenAiDirect,
        OPENAI_LUNA_MODEL_ID,
        ProviderCacheMode::Off,
    );
    let error = openai
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::MalformedOutput);

    let (_, mut anthropic) = anthropic_adapter(vec![
        Script::json(ANTHROPIC_MODEL_LIST),
        Script::sse(vec![b"event: message_start\ndata: {not-json}\n\n".to_vec()]),
    ]);
    anthropic.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::AnthropicDirect,
        ANTHROPIC_MODEL_ID,
        ProviderCacheMode::Off,
    );
    let error = anthropic
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::MalformedOutput);
}

#[test]
fn cancellation_is_free_before_send_and_best_effort_after_dispatch() {
    let (handle, mut adapter) = openai_adapter(vec![Script::json(OPENAI_MODEL_LIST)]);
    adapter.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::OpenAiDirect,
        OPENAI_LUNA_MODEL_ID,
        ProviderCacheMode::Off,
    );
    let cancel = CancellationToken::new();
    cancel.cancel(CancellationReason::Explicit);
    let sink = CollectingSink::default();
    let error = adapter.execute(&request, &cancel, &sink).unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::Canceled);
    assert_eq!(handle.captured().len(), 1);
    assert!(sink.events().iter().any(|event| matches!(
        event.kind,
        ProviderEventKind::Canceled {
            provider_billing_may_still_occur: false,
            ..
        }
    )));

    let (_, mut adapter) = anthropic_adapter(vec![
        Script::json(ANTHROPIC_MODEL_LIST),
        Script::sse(anthropic_stream()),
    ]);
    adapter.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::AnthropicDirect,
        ANTHROPIC_MODEL_ID,
        ProviderCacheMode::Off,
    );
    let cancel = CancellationToken::new();
    let sink = CancelingSink {
        events: Mutex::new(Vec::new()),
        cancel: cancel.clone(),
    };
    let error = adapter.execute(&request, &cancel, &sink).unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::Canceled);
    assert!(sink.events.lock().unwrap().iter().any(|event| matches!(
        event.kind,
        ProviderEventKind::Canceled {
            terminal_usage: Some(usage),
            provider_billing_may_still_occur: true,
            ..
        } if usage.output_tokens == Some(9) && usage.usage_complete
    )));
}

#[test]
fn model_ids_are_never_substituted() {
    let (handle, mut adapter) = openai_adapter(vec![Script::json(OPENAI_MODEL_LIST)]);
    adapter.catalog(&scope()).unwrap();
    let request = hosted_request(
        KnownTransport::OpenAiDirect,
        "gpt-5.6",
        ProviderCacheMode::Off,
    );
    let error = adapter
        .execute(
            &request,
            &CancellationToken::new(),
            &CollectingSink::default(),
        )
        .unwrap_err();
    assert_eq!(error.code, corti_postprocess::ErrorCode::ModelUnavailable);
    assert_eq!(
        handle.captured().len(),
        1,
        "unsupported alias must not be sent"
    );
}

const _: () = {
    assert!(CODEX_APP_SERVER_EXPERIMENTAL);
    assert!(!CODEX_APP_SERVER_DEFAULT_ENABLED);
    assert!(CODEX_APP_SERVER_COMPILED == cfg!(feature = "codex-experimental"));
    assert!(CLAUDE_SUBSCRIPTION_ADAPTER_BLOCKED);
};
