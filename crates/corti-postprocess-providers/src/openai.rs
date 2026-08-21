use std::{collections::HashSet, fmt};

use corti_postprocess::{
    AdapterCapabilities, BillingBasis, CancellationToken, ErrorCode, HostedRequest, KnownTransport,
    ModelCatalog, ModelDescriptor, ModelId, NormalizedUsage, PostprocessError, ProviderAdapter,
    ProviderCacheMode, ProviderDescriptor, ProviderEventKind, ProviderEventSink, ProviderScope,
    ProviderTerminal, RawUsage, SupportTier,
};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::{
    common::{
        DISCARD_EVENT_SINK, DirectAdapterOptions, MAX_CATALOG_BODY_BYTES, ProviderCacheKeySource,
        SendFailure, TextCollector, Timing, boundary_code, credential_code, emit, http_status_code,
        json_bytes, parse_output, read_body_limited, request_timeout, send_with_retry,
        terminal_cache_observation, usage_cache_observations, validate_event_stream_response,
        validate_prompt_layout,
    },
    schema::{output_schema, output_schema_name},
    sse::{SseDecoder, SseEvent},
    transport::{ApiKeySource, Clock, HttpMethod, HttpRequest, HttpTransport},
};

pub const OPENAI_RESPONSES_ADAPTER_VERSION: u32 = 1;
pub const OPENAI_LUNA_MODEL_ID: &str = "gpt-5.6-luna";
pub const OPENAI_LUNA_MAX_CONTEXT_TOKENS: u64 = 1_050_000;
pub const OPENAI_LUNA_MAX_OUTPUT_TOKENS: u64 = 128_000;

const OPENAI_RESPONSES_URL: &str = "https://api.openai.com/v1/responses";
const OPENAI_MODELS_URL: &str = "https://api.openai.com/v1/models";
const CATALOG_TIMEOUT_MICROS: u64 = 30_000_000;

pub struct OpenAiResponsesAdapter {
    transport: Box<dyn HttpTransport>,
    clock: Box<dyn Clock>,
    credentials: Box<dyn ApiKeySource>,
    cache_keys: Option<Box<dyn ProviderCacheKeySource>>,
    options: DirectAdapterOptions,
    catalog: ModelCatalog,
}

impl OpenAiResponsesAdapter {
    pub fn new(
        transport: Box<dyn HttpTransport>,
        clock: Box<dyn Clock>,
        credentials: Box<dyn ApiKeySource>,
    ) -> Self {
        Self {
            transport,
            clock,
            credentials,
            cache_keys: None,
            options: DirectAdapterOptions::default(),
            catalog: ModelCatalog { models: Vec::new() },
        }
    }

    pub fn with_options(mut self, options: DirectAdapterOptions) -> Result<Self, PostprocessError> {
        self.options = options.validate()?;
        Ok(self)
    }

    pub fn with_cache_key_source(mut self, source: Box<dyn ProviderCacheKeySource>) -> Self {
        self.cache_keys = Some(source);
        self
    }

    fn catalog_inner(&mut self, scope: &ProviderScope) -> Result<ModelCatalog, PostprocessError> {
        if scope.region.is_some() {
            return Err(ErrorCode::PolicyBlocked.into());
        }
        let key = self
            .credentials
            .resolve()
            .map_err(|error| PostprocessError::from(credential_code(error)))?;
        let now = self.clock.monotonic_micros();
        let deadline =
            corti_postprocess::MonotonicDeadline(now.saturating_add(CATALOG_TIMEOUT_MICROS));
        let wire = HttpRequest::new(HttpMethod::Get, parse_url(OPENAI_MODELS_URL)?)
            .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
            .with_public_header("accept", "application/json")
            .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
            .with_api_key_header("authorization", key, "Bearer ")
            .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
            .with_timeout(request_timeout(deadline, now)?);
        let cancel = CancellationToken::new();
        let mut exchange = send_with_retry(
            &mut *self.transport,
            &*self.clock,
            &wire,
            &cancel,
            deadline,
            None,
        )
        .map_err(|failure| PostprocessError::from(failure.code))?;
        if exchange.response.status() != 200 {
            let code = http_status_code(exchange.response.status());
            if code == ErrorCode::AuthRejected {
                self.credentials.mark_rejected();
            }
            return Err(code.into());
        }
        let bytes = read_body_limited(
            &mut exchange.response,
            MAX_CATALOG_BODY_BYTES,
            &cancel,
            deadline,
            &*self.clock,
        )?;
        let listing: OpenAiModelList =
            serde_json::from_slice(&bytes).map_err(|_| ErrorCode::MalformedOutput)?;
        if listing.object != "list" {
            return Err(ErrorCode::MalformedOutput.into());
        }

        let mut seen = HashSet::new();
        let mut models = Vec::new();
        for model in listing.data {
            if !seen.insert(model.id.clone()) {
                return Err(ErrorCode::MalformedOutput.into());
            }
            if model.object != "model" || model.id != OPENAI_LUNA_MODEL_ID {
                continue;
            }
            models.push(ModelDescriptor {
                provider: KnownTransport::OpenAiDirect.descriptor().provider,
                transport: KnownTransport::OpenAiDirect.descriptor().transport,
                support_tier: SupportTier::Documented,
                exact_model_id: ModelId::new(model.id).map_err(|_| ErrorCode::MalformedOutput)?,
                account_scoped_available: true,
                region: None,
                max_context_tokens: OPENAI_LUNA_MAX_CONTEXT_TOKENS,
                max_output_tokens: OPENAI_LUNA_MAX_OUTPUT_TOKENS,
                capabilities: AdapterCapabilities {
                    text_input: true,
                    text_output: true,
                    streaming: true,
                    structured_output: true,
                    explicit_prefix_cache: true,
                    // The adapter always requests explicit-only mode, even when no breakpoint is enabled.
                    implicit_cache_may_apply: false,
                },
                billing_basis: BillingBasis::MeteredEstimate,
                tariff_version: None,
                deprecated: model.shutdown_date.is_some(),
                benchmarked_for_live: false,
            });
        }
        let catalog = ModelCatalog { models };
        self.catalog = catalog.clone();
        Ok(catalog)
    }

    fn execute_inner(
        &mut self,
        request: &HostedRequest,
        cancel: &CancellationToken,
        sink: &dyn ProviderEventSink,
    ) -> Result<ProviderTerminal, ExecFailure> {
        let total_start = self.clock.monotonic_micros();
        let mut timing = Timing::new(total_start);
        if let Some(code) = boundary_code(cancel, request.deadline, total_start) {
            return Err(ExecFailure::new(code, false));
        }
        validate_openai_request(request, &self.catalog, self.options.max_output_tokens)
            .map_err(ExecFailure::from_error)?;

        let cache_key = match request.cache_policy.provider {
            ProviderCacheMode::Off => None,
            ProviderCacheMode::ExplicitStablePrefix => Some(
                self.cache_keys
                    .as_mut()
                    .ok_or_else(|| ExecFailure::new(ErrorCode::Cache, false))?
                    .key_for(request)
                    .map_err(|_| ExecFailure::new(ErrorCode::Cache, false))?,
            ),
            ProviderCacheMode::UnavoidableImplicit | ProviderCacheMode::Unavailable => {
                return Err(ExecFailure::new(ErrorCode::PolicyBlocked, false));
            }
        };

        let auth_start = self.clock.monotonic_micros();
        let key = self
            .credentials
            .resolve()
            .map_err(|error| ExecFailure::new(credential_code(error), false))?;
        timing.auth_us = Some(self.clock.monotonic_micros().saturating_sub(auth_start));
        if let Some(code) = boundary_code(cancel, request.deadline, self.clock.monotonic_micros()) {
            return Err(ExecFailure::new(code, false));
        }

        let body = openai_request_body(request, self.options.max_output_tokens, cache_key.as_ref())
            .map_err(ExecFailure::from_error)?;
        let now = self.clock.monotonic_micros();
        let wire = HttpRequest::new(
            HttpMethod::Post,
            parse_url(OPENAI_RESPONSES_URL).map_err(ExecFailure::from_error)?,
        )
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_public_header("accept", "text/event-stream")
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_public_header("content-type", "application/json")
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_api_key_header("authorization", key, "Bearer ")
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_json_body(body)
        .with_timeout(request_timeout(request.deadline, now).map_err(ExecFailure::from_error)?);

        let mut exchange = send_with_retry(
            &mut *self.transport,
            &*self.clock,
            &wire,
            cancel,
            request.deadline,
            Some((request, sink)),
        )
        .map_err(ExecFailure::from_send)?;
        timing.exchange = Some(exchange.times);
        if exchange.response.status() != 200 {
            let code = http_status_code(exchange.response.status());
            if code == ErrorCode::AuthRejected {
                self.credentials.mark_rejected();
            }
            return Err(ExecFailure::new(code, true));
        }
        validate_event_stream_response(&exchange.response)
            .map_err(|error| ExecFailure::new(error.code, true))?;

        let mut decoder = SseDecoder::new(self.options.max_stream_bytes);
        let mut state = OpenAiStreamState::new(self.options.max_stream_bytes);
        loop {
            if let Some(code) =
                boundary_code(cancel, request.deadline, self.clock.monotonic_micros())
            {
                exchange.response.body_mut().cancel();
                return Err(ExecFailure {
                    code,
                    usage: state.terminal_usage,
                    dispatched: true,
                });
            }
            let next = exchange
                .response
                .body_mut()
                .next_chunk()
                .map_err(|error| ExecFailure {
                    code: crate::common::transport_code(error, cancel),
                    usage: state.terminal_usage,
                    dispatched: true,
                })?;
            let Some(chunk) = next else {
                break;
            };
            let events = decoder.push(&chunk).map_err(|error| ExecFailure {
                code: error.code,
                usage: state.terminal_usage,
                dispatched: true,
            })?;
            let mut buffered_boundary = None;
            for event in events {
                if buffered_boundary.is_none() {
                    buffered_boundary =
                        boundary_code(cancel, request.deadline, self.clock.monotonic_micros());
                }
                let event_sink: &dyn ProviderEventSink = if buffered_boundary.is_some() {
                    &DISCARD_EVENT_SINK
                } else {
                    sink
                };
                state
                    .process(event, request, event_sink, &*self.clock)
                    .map_err(|failure| failure.with_dispatched())?;
                if buffered_boundary.is_none() {
                    buffered_boundary =
                        boundary_code(cancel, request.deadline, self.clock.monotonic_micros());
                }
            }
            if let Some(code) = buffered_boundary {
                // Do not read another network chunk after cancellation. Terminal usage already buffered in
                // this chunk is reconciled above without emitting any late text.
                exchange.response.body_mut().cancel();
                return Err(ExecFailure {
                    code,
                    usage: state.terminal_usage,
                    dispatched: true,
                });
            }
        }
        for event in decoder.finish().map_err(|error| ExecFailure {
            code: error.code,
            usage: state.terminal_usage,
            dispatched: true,
        })? {
            state
                .process(event, request, sink, &*self.clock)
                .map_err(|failure| failure.with_dispatched())?;
        }
        state
            .finish()
            .map_err(|failure| failure.with_dispatched())?;
        timing.first_text_at = state.text.first_text_at();
        timing.terminal_at = state.terminal_at;

        if let Some(code) = boundary_code(cancel, request.deadline, self.clock.monotonic_micros()) {
            return Err(ExecFailure {
                code,
                usage: state.terminal_usage,
                dispatched: true,
            });
        }
        let parse_start = self.clock.monotonic_micros();
        let output = parse_output(request.prompt.task(), state.text.text()).map_err(|error| {
            ExecFailure {
                code: error.code,
                usage: state.terminal_usage,
                dispatched: true,
            }
        })?;
        let completed_at = self.clock.monotonic_micros();
        timing.parse_us = Some(completed_at.saturating_sub(parse_start));
        let usage = state
            .terminal_usage
            .unwrap_or_else(NormalizedUsage::unknown);
        let cache = terminal_cache_observation(usage);
        Ok(ProviderTerminal {
            output,
            usage,
            latency: timing.latency(completed_at),
            cache,
        })
    }
}

impl fmt::Debug for OpenAiResponsesAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiResponsesAdapter")
            .field("descriptor", &KnownTransport::OpenAiDirect.descriptor())
            .field("options", &self.options)
            .field("catalog_models", &self.catalog.models.len())
            .field("credential", &"<injected>")
            .finish()
    }
}

impl ProviderAdapter for OpenAiResponsesAdapter {
    fn descriptor(&self) -> ProviderDescriptor {
        KnownTransport::OpenAiDirect.descriptor()
    }

    fn catalog(&mut self, scope: &ProviderScope) -> Result<ModelCatalog, PostprocessError> {
        self.catalog_inner(scope)
    }

    fn execute(
        &mut self,
        request: &HostedRequest,
        cancel: &CancellationToken,
        sink: &dyn ProviderEventSink,
    ) -> Result<ProviderTerminal, PostprocessError> {
        emit(sink, request, ProviderEventKind::Queued);
        match self.execute_inner(request, cancel, sink) {
            Ok(terminal) => {
                for observation in usage_cache_observations(terminal.usage) {
                    emit(sink, request, ProviderEventKind::CacheObserved(observation));
                }
                emit(sink, request, ProviderEventKind::Completed(terminal.usage));
                Ok(terminal)
            }
            Err(failure) => {
                if let Some(reason) = cancel.reason() {
                    emit(
                        sink,
                        request,
                        ProviderEventKind::Canceled {
                            reason,
                            terminal_usage: failure.usage,
                            provider_billing_may_still_occur: failure.dispatched,
                        },
                    );
                } else {
                    emit(
                        sink,
                        request,
                        ProviderEventKind::Failed {
                            code: failure.code,
                            terminal_usage: failure.usage,
                        },
                    );
                }
                Err(failure.code.into())
            }
        }
    }
}

fn validate_openai_request(
    request: &HostedRequest,
    catalog: &ModelCatalog,
    configured_max_output: u64,
) -> Result<(), PostprocessError> {
    let descriptor = KnownTransport::OpenAiDirect.descriptor();
    if request.provider != descriptor.provider || request.transport != descriptor.transport {
        return Err(ErrorCode::PolicyBlocked.into());
    }
    validate_prompt_layout(request)?;
    let model = catalog
        .find_exact(&request.model, None)
        .ok_or_else(|| PostprocessError::from(ErrorCode::ModelUnavailable))?;
    if !model.account_scoped_available
        || model.support_tier != SupportTier::Documented
        || !model.capabilities.text_input
        || !model.capabilities.text_output
        || !model.capabilities.streaming
        || !model.capabilities.structured_output
        || configured_max_output > model.max_output_tokens
    {
        return Err(ErrorCode::ModelUnavailable.into());
    }
    Ok(())
}

fn openai_request_body(
    request: &HostedRequest,
    max_output_tokens: u64,
    cache_key: Option<&corti_postprocess::ProviderCacheKey>,
) -> Result<Vec<u8>, PostprocessError> {
    let cache_enabled = request.cache_policy.provider == ProviderCacheMode::ExplicitStablePrefix;
    let mut input = Vec::with_capacity(request.prompt.messages().len());
    for (index, message) in request.prompt.messages().iter().enumerate() {
        let role = match message.role() {
            corti_postprocess::PromptRole::Developer => "developer",
            corti_postprocess::PromptRole::User => "user",
        };
        let mut content = json!({
            "type": "input_text",
            "text": message.content(),
        });
        if cache_enabled && index == 2 {
            content["prompt_cache_breakpoint"] = json!({"mode": "explicit"});
        }
        input.push(json!({
            "type": "message",
            "role": role,
            "content": [content],
        }));
    }
    let mut body = json!({
        "model": request.model.as_str(),
        "input": input,
        "max_output_tokens": max_output_tokens,
        "stream": true,
        "store": false,
        "prompt_cache_options": {"mode": "explicit"},
        "text": {
            "format": {
                "type": "json_schema",
                "name": output_schema_name(request.prompt.task()),
                "strict": true,
                "schema": output_schema(request.prompt.task()),
            }
        }
    });
    if let Some(cache_key) = cache_key {
        body["prompt_cache_key"] = Value::String(cache_key.as_str().to_owned());
    }
    json_bytes(&body)
}

struct OpenAiStreamState {
    text: TextCollector,
    completed: bool,
    done_text_seen: bool,
    terminal_usage: Option<NormalizedUsage>,
    terminal_at: Option<u64>,
}

impl OpenAiStreamState {
    fn new(max_bytes: usize) -> Self {
        Self {
            text: TextCollector::new(max_bytes),
            completed: false,
            done_text_seen: false,
            terminal_usage: None,
            terminal_at: None,
        }
    }

    fn process(
        &mut self,
        event: SseEvent,
        request: &HostedRequest,
        sink: &dyn ProviderEventSink,
        clock: &dyn Clock,
    ) -> Result<(), ExecFailure> {
        if event.data == "[DONE]" {
            if !self.completed {
                return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
            }
            return Ok(());
        }
        let envelope: OpenAiEnvelope = serde_json::from_str(&event.data)
            .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
        if event
            .event
            .as_deref()
            .is_some_and(|name| name != envelope.kind)
        {
            return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
        }
        if self.completed {
            return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
        }
        match envelope.kind.as_str() {
            "response.output_text.delta" => {
                let payload: OpenAiTextDelta = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                self.text
                    .push(&payload.delta, request, sink, clock)
                    .map_err(|error| ExecFailure::new(error.code, true))?;
            }
            "response.output_text.done" => {
                if self.done_text_seen {
                    return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                }
                let payload: OpenAiTextDone = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                self.text
                    .verify_done_text(&payload.text)
                    .map_err(|error| ExecFailure::new(error.code, true))?;
                self.done_text_seen = true;
            }
            "response.completed" => {
                let payload: OpenAiCompleted = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                if payload.response.model != request.model.as_str()
                    || payload
                        .response
                        .status
                        .as_deref()
                        .is_some_and(|status| status != "completed")
                {
                    return Err(ExecFailure::new(ErrorCode::ModelUnavailable, true));
                }
                let usage = payload
                    .response
                    .usage
                    .map(normalize_openai_usage)
                    .transpose()
                    .map_err(|code| ExecFailure::new(code, true))?
                    .unwrap_or_else(NormalizedUsage::unknown);
                self.terminal_usage = Some(usage);
                self.completed = true;
                self.terminal_at = Some(clock.monotonic_micros());
            }
            "response.failed" | "response.incomplete" | "error" => {
                let payload: OpenAiFailure = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::Provider, true))?;
                let nested_code = payload
                    .response
                    .as_ref()
                    .and_then(|response| response.error.as_ref())
                    .and_then(|error| error.code.as_deref());
                let code = if envelope.kind == "response.incomplete" {
                    ErrorCode::MalformedOutput
                } else {
                    openai_error_code(payload.code.as_deref().or(nested_code))
                };
                return Err(ExecFailure {
                    code,
                    usage: payload
                        .response
                        .and_then(|response| response.usage)
                        .and_then(|usage| normalize_openai_usage(usage).ok()),
                    dispatched: true,
                });
            }
            // The Responses API is explicitly extensible. Unknown semantic events are ignored; known text
            // and terminal events above remain strict.
            _ => {}
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), ExecFailure> {
        if !self.completed || self.text.text().is_empty() {
            return Err(ExecFailure {
                code: ErrorCode::MalformedOutput,
                usage: self.terminal_usage,
                dispatched: true,
            });
        }
        Ok(())
    }
}

fn normalize_openai_usage(usage: OpenAiUsage) -> Result<NormalizedUsage, ErrorCode> {
    let complete = usage.input_tokens.is_some() && usage.output_tokens.is_some();
    NormalizedUsage::try_from(RawUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_read_tokens: usage.input_tokens_details.cached_tokens,
        cached_write_tokens: usage.input_tokens_details.cache_write_tokens,
        reasoning_tokens: usage.output_tokens_details.reasoning_tokens,
        usage_complete: complete,
    })
    .map_err(|_| ErrorCode::MalformedOutput)
}

fn openai_error_code(code: Option<&str>) -> ErrorCode {
    match code {
        Some("invalid_api_key" | "authentication_error") => ErrorCode::AuthRejected,
        Some("permission_denied") => ErrorCode::Permission,
        Some("insufficient_quota" | "billing_hard_limit_reached") => ErrorCode::Quota,
        Some("rate_limit_exceeded") => ErrorCode::RateLimited,
        Some("model_not_found") => ErrorCode::ModelUnavailable,
        Some("server_error" | "overloaded_error") | None => ErrorCode::Provider,
        Some(_) => ErrorCode::Provider,
    }
}

#[derive(Debug, Clone, Copy)]
struct ExecFailure {
    code: ErrorCode,
    usage: Option<NormalizedUsage>,
    dispatched: bool,
}

impl ExecFailure {
    const fn new(code: ErrorCode, dispatched: bool) -> Self {
        Self {
            code,
            usage: None,
            dispatched,
        }
    }

    fn from_error(error: PostprocessError) -> Self {
        Self::new(error.code, false)
    }

    fn from_send(failure: SendFailure) -> Self {
        Self::new(failure.code, failure.dispatched)
    }

    const fn with_dispatched(mut self) -> Self {
        self.dispatched = true;
        self
    }
}

#[derive(Deserialize)]
struct OpenAiModelList {
    object: String,
    data: Vec<OpenAiModel>,
}

#[derive(Deserialize)]
struct OpenAiModel {
    id: String,
    object: String,
    #[serde(default)]
    shutdown_date: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiEnvelope {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct OpenAiTextDelta {
    delta: String,
}

#[derive(Deserialize)]
struct OpenAiTextDone {
    text: String,
}

#[derive(Deserialize)]
struct OpenAiCompleted {
    response: OpenAiCompletedResponse,
}

#[derive(Deserialize)]
struct OpenAiCompletedResponse {
    model: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Deserialize)]
struct OpenAiFailure {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    response: Option<OpenAiFailureResponse>,
}

#[derive(Deserialize)]
struct OpenAiFailureResponse {
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    #[serde(default)]
    error: Option<OpenAiFailureDetail>,
}

#[derive(Deserialize)]
struct OpenAiFailureDetail {
    #[serde(default)]
    code: Option<String>,
}

#[derive(Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    input_tokens_details: OpenAiInputTokenDetails,
    #[serde(default)]
    output_tokens_details: OpenAiOutputTokenDetails,
}

#[derive(Default, Deserialize)]
struct OpenAiInputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<i64>,
    #[serde(default)]
    cache_write_tokens: Option<i64>,
}

#[derive(Default, Deserialize)]
struct OpenAiOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<i64>,
}

fn parse_url(value: &str) -> Result<Url, PostprocessError> {
    Url::parse(value).map_err(|_| ErrorCode::Internal.into())
}
