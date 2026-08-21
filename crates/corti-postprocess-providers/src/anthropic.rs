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
        DISCARD_EVENT_SINK, DirectAdapterOptions, MAX_CATALOG_BODY_BYTES, SendFailure,
        TextCollector, Timing, boundary_code, credential_code, emit, http_status_code, json_bytes,
        parse_output, read_body_limited, request_timeout, send_with_retry,
        terminal_cache_observation, usage_cache_observations, validate_event_stream_response,
        validate_prompt_layout,
    },
    schema::output_schema,
    sse::{SseDecoder, SseEvent},
    transport::{ApiKeySource, Clock, HttpMethod, HttpRequest, HttpTransport},
};

pub const ANTHROPIC_MESSAGES_ADAPTER_VERSION: u32 = 1;
pub const ANTHROPIC_API_VERSION: &str = "2023-06-01";

const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_MODELS_URL: &str = "https://api.anthropic.com/v1/models";
const CATALOG_TIMEOUT_MICROS: u64 = 30_000_000;
const MAX_CATALOG_PAGES: usize = 32;

pub struct AnthropicMessagesAdapter {
    transport: Box<dyn HttpTransport>,
    clock: Box<dyn Clock>,
    credentials: Box<dyn ApiKeySource>,
    options: DirectAdapterOptions,
    catalog: ModelCatalog,
    active_region: Option<String>,
}

impl AnthropicMessagesAdapter {
    pub fn new(
        transport: Box<dyn HttpTransport>,
        clock: Box<dyn Clock>,
        credentials: Box<dyn ApiKeySource>,
    ) -> Self {
        Self {
            transport,
            clock,
            credentials,
            options: DirectAdapterOptions::default(),
            catalog: ModelCatalog { models: Vec::new() },
            active_region: None,
        }
    }

    pub fn with_options(mut self, options: DirectAdapterOptions) -> Result<Self, PostprocessError> {
        self.options = options.validate()?;
        Ok(self)
    }

    fn catalog_inner(&mut self, scope: &ProviderScope) -> Result<ModelCatalog, PostprocessError> {
        let cancel = CancellationToken::new();
        let mut cursor = None;
        let mut raw_models = Vec::new();
        let mut seen_cursors = HashSet::new();
        for _ in 0..MAX_CATALOG_PAGES {
            let key = self
                .credentials
                .resolve()
                .map_err(|error| PostprocessError::from(credential_code(error)))?;
            let now = self.clock.monotonic_micros();
            let deadline =
                corti_postprocess::MonotonicDeadline(now.saturating_add(CATALOG_TIMEOUT_MICROS));
            let mut url = parse_url(ANTHROPIC_MODELS_URL)?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("limit", "1000");
                if let Some(cursor) = cursor.as_deref() {
                    query.append_pair("after_id", cursor);
                }
            }
            let wire = HttpRequest::new(HttpMethod::Get, url)
                .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
                .with_public_header("accept", "application/json")
                .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
                .with_public_header("anthropic-version", ANTHROPIC_API_VERSION)
                .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
                .with_api_key_header("x-api-key", key, "")
                .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
                .with_timeout(request_timeout(deadline, now)?);
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
            let page: AnthropicModelPage =
                serde_json::from_slice(&bytes).map_err(|_| ErrorCode::MalformedOutput)?;
            raw_models.extend(page.data);
            if !page.has_more {
                cursor = None;
                break;
            }
            let next = page.last_id.ok_or(ErrorCode::MalformedOutput)?;
            if !seen_cursors.insert(next.clone()) {
                return Err(ErrorCode::MalformedOutput.into());
            }
            cursor = Some(next);
        }
        if cursor.is_some() {
            return Err(ErrorCode::Provider.into());
        }

        let descriptor = KnownTransport::AnthropicDirect.descriptor();
        let mut seen_models = HashSet::new();
        let mut models = Vec::new();
        for model in raw_models {
            if model.object_type != "model" || !seen_models.insert(model.id.clone()) {
                return Err(ErrorCode::MalformedOutput.into());
            }
            let structured = model
                .capabilities
                .as_ref()
                .and_then(|capabilities| capabilities.structured_outputs.as_ref())
                .is_some_and(|capability| capability.supported);
            let (Some(max_context_tokens), Some(max_output_tokens)) =
                (model.max_input_tokens, model.max_tokens)
            else {
                continue;
            };
            if !structured || max_context_tokens == 0 || max_output_tokens == 0 {
                continue;
            }
            models.push(ModelDescriptor {
                provider: descriptor.provider.clone(),
                transport: descriptor.transport.clone(),
                support_tier: SupportTier::Documented,
                exact_model_id: ModelId::new(model.id).map_err(|_| ErrorCode::MalformedOutput)?,
                account_scoped_available: true,
                region: scope.region.clone(),
                max_context_tokens,
                max_output_tokens,
                capabilities: AdapterCapabilities {
                    text_input: true,
                    text_output: true,
                    streaming: true,
                    structured_output: true,
                    explicit_prefix_cache: true,
                    // The adapter omits all cache controls when provider caching is off.
                    implicit_cache_may_apply: false,
                },
                billing_basis: BillingBasis::MeteredEstimate,
                tariff_version: None,
                deprecated: false,
                benchmarked_for_live: false,
            });
        }
        let catalog = ModelCatalog { models };
        self.active_region = scope.region.clone();
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
        validate_anthropic_request(
            request,
            &self.catalog,
            self.active_region.as_deref(),
            self.options.max_output_tokens,
        )
        .map_err(ExecFailure::from_error)?;
        if !matches!(
            request.cache_policy.provider,
            ProviderCacheMode::Off | ProviderCacheMode::ExplicitStablePrefix
        ) {
            return Err(ExecFailure::new(ErrorCode::PolicyBlocked, false));
        }

        let auth_start = self.clock.monotonic_micros();
        let key = self
            .credentials
            .resolve()
            .map_err(|error| ExecFailure::new(credential_code(error), false))?;
        timing.auth_us = Some(self.clock.monotonic_micros().saturating_sub(auth_start));
        if let Some(code) = boundary_code(cancel, request.deadline, self.clock.monotonic_micros()) {
            return Err(ExecFailure::new(code, false));
        }

        let body = anthropic_request_body(
            request,
            self.options.max_output_tokens,
            self.active_region.as_deref(),
        )
        .map_err(ExecFailure::from_error)?;
        let now = self.clock.monotonic_micros();
        let wire = HttpRequest::new(
            HttpMethod::Post,
            parse_url(ANTHROPIC_MESSAGES_URL).map_err(ExecFailure::from_error)?,
        )
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_public_header("accept", "text/event-stream")
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_public_header("content-type", "application/json")
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_public_header("anthropic-version", ANTHROPIC_API_VERSION)
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_api_key_header("x-api-key", key, "")
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
        let mut state = AnthropicStreamState::new(self.options.max_stream_bytes);
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
                // Reconcile terminal usage already buffered in this chunk, but emit no late text and do
                // not read another network chunk after cancellation.
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

impl fmt::Debug for AnthropicMessagesAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicMessagesAdapter")
            .field("descriptor", &KnownTransport::AnthropicDirect.descriptor())
            .field("options", &self.options)
            .field("catalog_models", &self.catalog.models.len())
            .field("region_configured", &self.active_region.is_some())
            .field("credential", &"<injected>")
            .finish()
    }
}

impl ProviderAdapter for AnthropicMessagesAdapter {
    fn descriptor(&self) -> ProviderDescriptor {
        KnownTransport::AnthropicDirect.descriptor()
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

fn validate_anthropic_request(
    request: &HostedRequest,
    catalog: &ModelCatalog,
    region: Option<&str>,
    configured_max_output: u64,
) -> Result<(), PostprocessError> {
    let descriptor = KnownTransport::AnthropicDirect.descriptor();
    if request.provider != descriptor.provider || request.transport != descriptor.transport {
        return Err(ErrorCode::PolicyBlocked.into());
    }
    validate_prompt_layout(request)?;
    let model = catalog
        .find_exact(&request.model, region)
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

fn anthropic_request_body(
    request: &HostedRequest,
    max_output_tokens: u64,
    region: Option<&str>,
) -> Result<Vec<u8>, PostprocessError> {
    let cache_enabled = request.cache_policy.provider == ProviderCacheMode::ExplicitStablePrefix;
    let prompt = request.prompt.messages();
    let mut system = Vec::with_capacity(3);
    for (index, message) in prompt[..3].iter().enumerate() {
        let mut block = json!({"type": "text", "text": message.content()});
        if cache_enabled && index == 2 {
            block["cache_control"] = json!({"type": "ephemeral", "ttl": "5m"});
        }
        system.push(block);
    }
    let dynamic = prompt[3..]
        .iter()
        .map(|message| json!({"type": "text", "text": message.content()}))
        .collect::<Vec<_>>();
    let mut body = json!({
        "model": request.model.as_str(),
        "max_tokens": max_output_tokens,
        "stream": true,
        "system": system,
        "messages": [{"role": "user", "content": dynamic}],
        "output_config": {
            "format": {
                "type": "json_schema",
                "schema": output_schema(request.prompt.task())
            }
        }
    });
    if let Some(region) = region {
        body["inference_geo"] = Value::String(region.to_owned());
    }
    json_bytes(&body)
}

struct AnthropicStreamState {
    text: TextCollector,
    started: bool,
    stopped: bool,
    text_block_seen: bool,
    open_text_index: Option<u64>,
    stop_reason: Option<String>,
    usage: AnthropicUsageValues,
    terminal_usage: Option<NormalizedUsage>,
    terminal_at: Option<u64>,
}

impl AnthropicStreamState {
    fn new(max_bytes: usize) -> Self {
        Self {
            text: TextCollector::new(max_bytes),
            started: false,
            stopped: false,
            text_block_seen: false,
            open_text_index: None,
            stop_reason: None,
            usage: AnthropicUsageValues::default(),
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
        let envelope: AnthropicEnvelope = serde_json::from_str(&event.data)
            .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
        if event
            .event
            .as_deref()
            .is_some_and(|name| name != envelope.kind)
        {
            return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
        }
        if self.stopped && envelope.kind != "ping" {
            return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
        }
        match envelope.kind.as_str() {
            "message_start" => {
                if self.started {
                    return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                }
                let payload: AnthropicMessageStart = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                if payload.message.model != request.model.as_str() {
                    return Err(ExecFailure::new(ErrorCode::ModelUnavailable, true));
                }
                self.usage
                    .reconcile(payload.message.usage)
                    .map_err(|code| ExecFailure::new(code, true))?;
                self.started = true;
                emit(
                    sink,
                    request,
                    ProviderEventKind::UsageProvisional(
                        self.usage
                            .normalized(false)
                            .map_err(|code| ExecFailure::new(code, true))?,
                    ),
                );
            }
            "content_block_start" => {
                if !self.started || self.open_text_index.is_some() || self.text_block_seen {
                    return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                }
                let payload: AnthropicBlockStart = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                if payload.content_block.kind != "text" {
                    return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                }
                self.open_text_index = Some(payload.index);
                self.text_block_seen = true;
                self.text
                    .push(&payload.content_block.text, request, sink, clock)
                    .map_err(|error| ExecFailure::new(error.code, true))?;
            }
            "content_block_delta" => {
                let payload: AnthropicBlockDelta = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                if self.open_text_index != Some(payload.index) || payload.delta.kind != "text_delta"
                {
                    return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                }
                self.text
                    .push(&payload.delta.text, request, sink, clock)
                    .map_err(|error| ExecFailure::new(error.code, true))?;
            }
            "content_block_stop" => {
                let payload: AnthropicBlockStop = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                if self.open_text_index != Some(payload.index) {
                    return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                }
                self.open_text_index = None;
            }
            "message_delta" => {
                if !self.started || self.open_text_index.is_some() {
                    return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                }
                let payload: AnthropicMessageDelta = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
                if let Some(reason) = payload.delta.stop_reason {
                    if self
                        .stop_reason
                        .as_ref()
                        .is_some_and(|existing| existing != &reason)
                    {
                        return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                    }
                    self.stop_reason = Some(reason);
                }
                self.usage
                    .reconcile(payload.usage)
                    .map_err(|code| ExecFailure::new(code, true))?;
                emit(
                    sink,
                    request,
                    ProviderEventKind::UsageProvisional(
                        self.usage
                            .normalized(false)
                            .map_err(|code| ExecFailure::new(code, true))?,
                    ),
                );
            }
            "message_stop" => {
                if !self.started
                    || self.stopped
                    || self.open_text_index.is_some()
                    || !self.text_block_seen
                {
                    return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                }
                match self.stop_reason.as_deref() {
                    Some("end_turn") => {}
                    Some("max_tokens" | "model_context_window_exceeded") | None => {
                        return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                    }
                    Some(_) => return Err(ExecFailure::new(ErrorCode::Provider, true)),
                }
                let usage = self
                    .usage
                    .normalized(true)
                    .map_err(|code| ExecFailure::new(code, true))?;
                self.terminal_usage = Some(usage);
                self.terminal_at = Some(clock.monotonic_micros());
                self.stopped = true;
            }
            "error" => {
                let payload: AnthropicErrorEvent = serde_json::from_str(&event.data)
                    .map_err(|_| ExecFailure::new(ErrorCode::Provider, true))?;
                return Err(ExecFailure {
                    code: anthropic_error_code(&payload.error.kind),
                    usage: self.usage.normalized(false).ok(),
                    dispatched: true,
                });
            }
            "ping" => {}
            // Anthropic documents that new event types may be added. Unknown events are ignored.
            _ => {}
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), ExecFailure> {
        if !self.stopped || self.text.text().is_empty() {
            return Err(ExecFailure {
                code: ErrorCode::MalformedOutput,
                usage: self.terminal_usage,
                dispatched: true,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AnthropicUsageValues {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_read_tokens: Option<i64>,
    cached_write_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
}

impl AnthropicUsageValues {
    fn reconcile(&mut self, update: AnthropicUsage) -> Result<(), ErrorCode> {
        reconcile_field(&mut self.input_tokens, update.input_tokens)?;
        reconcile_field(&mut self.output_tokens, update.output_tokens)?;
        reconcile_field(&mut self.cached_read_tokens, update.cache_read_input_tokens)?;
        reconcile_field(
            &mut self.cached_write_tokens,
            update.cache_creation_input_tokens,
        )?;
        reconcile_field(
            &mut self.reasoning_tokens,
            update.output_tokens_details.thinking_tokens,
        )?;
        Ok(())
    }

    fn normalized(self, terminal: bool) -> Result<NormalizedUsage, ErrorCode> {
        let complete = terminal && self.input_tokens.is_some() && self.output_tokens.is_some();
        NormalizedUsage::try_from(RawUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_read_tokens: self.cached_read_tokens,
            cached_write_tokens: self.cached_write_tokens,
            reasoning_tokens: self.reasoning_tokens,
            usage_complete: complete,
        })
        .map_err(|_| ErrorCode::MalformedOutput)
    }
}

fn reconcile_field(current: &mut Option<i64>, update: Option<i64>) -> Result<(), ErrorCode> {
    let Some(update) = update else {
        return Ok(());
    };
    if update < 0 || current.is_some_and(|current| update < current) {
        return Err(ErrorCode::MalformedOutput);
    }
    *current = Some(update);
    Ok(())
}

fn anthropic_error_code(kind: &str) -> ErrorCode {
    match kind {
        "authentication_error" => ErrorCode::AuthRejected,
        "permission_error" => ErrorCode::Permission,
        "rate_limit_error" => ErrorCode::RateLimited,
        "not_found_error" => ErrorCode::ModelUnavailable,
        "overloaded_error" => ErrorCode::Provider,
        _ => ErrorCode::Provider,
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
struct AnthropicModelPage {
    data: Vec<AnthropicModel>,
    has_more: bool,
    #[serde(default)]
    last_id: Option<String>,
}

#[derive(Deserialize)]
struct AnthropicModel {
    id: String,
    #[serde(rename = "type")]
    object_type: String,
    #[serde(default)]
    capabilities: Option<AnthropicModelCapabilities>,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct AnthropicModelCapabilities {
    #[serde(default)]
    structured_outputs: Option<AnthropicCapability>,
}

#[derive(Deserialize)]
struct AnthropicCapability {
    supported: bool,
}

#[derive(Deserialize)]
struct AnthropicEnvelope {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct AnthropicMessageStart {
    message: AnthropicStartMessage,
}

#[derive(Deserialize)]
struct AnthropicStartMessage {
    model: String,
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
struct AnthropicBlockStart {
    index: u64,
    content_block: AnthropicTextBlock,
}

#[derive(Deserialize)]
struct AnthropicTextBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct AnthropicBlockDelta {
    index: u64,
    delta: AnthropicTextDelta,
}

#[derive(Deserialize)]
struct AnthropicTextDelta {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct AnthropicBlockStop {
    index: u64,
}

#[derive(Deserialize)]
struct AnthropicMessageDelta {
    delta: AnthropicTopLevelDelta,
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
struct AnthropicTopLevelDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<i64>,
    #[serde(default)]
    cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens_details: AnthropicOutputTokenDetails,
}

#[derive(Clone, Copy, Default, Deserialize)]
struct AnthropicOutputTokenDetails {
    #[serde(default)]
    thinking_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct AnthropicErrorEvent {
    error: AnthropicStreamError,
}

#[derive(Deserialize)]
struct AnthropicStreamError {
    #[serde(rename = "type")]
    kind: String,
}

fn parse_url(value: &str) -> Result<Url, PostprocessError> {
    Url::parse(value).map_err(|_| ErrorCode::Internal.into())
}
