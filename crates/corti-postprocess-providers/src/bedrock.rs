//! Amazon Bedrock `ConverseStream` adapter.
//!
//! Requests are SigV4-signed over the same injected `HttpTransport` the other documented adapters use,
//! and replies arrive in AWS's binary event-stream framing rather than SSE. Structured output is forced
//! tool use: Bedrock streams the tool input as JSON fragments, which gives the Live lane the same
//! `TextDelta` cadence the other transports produce.
//!
//! Credentials arrive through [`AwsCredentialSource`]; this module performs no ambient discovery.

use std::{collections::HashSet, fmt};

use corti_postprocess::{
    AdapterCapabilities, BillingBasis, CancellationToken, ErrorCode, HostedRequest, KnownTransport,
    ModelCatalog, ModelDescriptor, ModelId, MonotonicDeadline, NormalizedUsage, PostprocessError,
    ProviderAdapter, ProviderCacheMode, ProviderDescriptor, ProviderEventKind, ProviderEventSink,
    ProviderScope, ProviderTerminal, RawUsage, SupportTier,
};
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::{
    common::{
        DISCARD_EVENT_SINK, DirectAdapterOptions, MAX_CATALOG_BODY_BYTES, SendFailure,
        TextCollector, Timing, boundary_code, credential_code, emit, json_bytes, parse_output,
        read_body_limited, request_timeout, send_with_retry, terminal_cache_observation,
        usage_cache_observations, validate_prompt_layout,
    },
    eventstream::{EventStreamDecoder, EventStreamMessage},
    schema::{output_schema, output_schema_name},
    sigv4::{AwsCredentialSource, SigningScope, SigningTimestamp, sign_request, uri_encode},
    transport::{Clock, HttpMethod, HttpRequest, HttpTransport, WallClock},
};

pub const BEDROCK_CONVERSE_ADAPTER_VERSION: u32 = 1;
pub const BEDROCK_EVENT_STREAM_CONTENT_TYPE: &str = "application/vnd.amazon.eventstream";

/// `ListFoundationModels` publishes no per-model token limits, so the catalog declares conservative
/// floors that every Converse-capable text model on Bedrock satisfies. Under-declaring only narrows what
/// Corti will send; over-declaring would let a request exceed a small model's real ceiling.
pub const BEDROCK_CONSERVATIVE_MAX_CONTEXT_TOKENS: u64 = 32_000;
pub const BEDROCK_CONSERVATIVE_MAX_OUTPUT_TOKENS: u64 = 4_096;

const CATALOG_TIMEOUT_MICROS: u64 = 30_000_000;
const RUNTIME_SERVICE: &str = "bedrock";
const MAX_CATALOG_PAGES: usize = 32;

pub struct BedrockConverseAdapter {
    transport: Box<dyn HttpTransport>,
    clock: Box<dyn Clock>,
    wall_clock: Box<dyn WallClock>,
    credentials: Box<dyn AwsCredentialSource>,
    options: DirectAdapterOptions,
    catalog: ModelCatalog,
    active_region: Option<String>,
}

impl BedrockConverseAdapter {
    pub fn new(
        transport: Box<dyn HttpTransport>,
        clock: Box<dyn Clock>,
        wall_clock: Box<dyn WallClock>,
        credentials: Box<dyn AwsCredentialSource>,
    ) -> Self {
        Self {
            transport,
            clock,
            wall_clock,
            credentials,
            options: DirectAdapterOptions {
                max_output_tokens: BEDROCK_CONSERVATIVE_MAX_OUTPUT_TOKENS,
                ..DirectAdapterOptions::default()
            },
            catalog: ModelCatalog { models: Vec::new() },
            active_region: None,
        }
    }

    pub fn with_options(mut self, options: DirectAdapterOptions) -> Result<Self, PostprocessError> {
        self.options = options.validate()?;
        Ok(self)
    }

    fn catalog_inner(&mut self, scope: &ProviderScope) -> Result<ModelCatalog, PostprocessError> {
        let region = scope
            .region
            .as_deref()
            .filter(|region| is_plausible_region(region))
            .ok_or(PostprocessError::from(ErrorCode::ModelUnavailable))?
            .to_owned();
        let cancel = CancellationToken::new();
        let descriptor = KnownTransport::BedrockRuntime.descriptor();
        let mut models = Vec::new();
        let mut seen = HashSet::new();

        for summary in self.list_foundation_models(&region, &cancel)? {
            if !summary.is_selectable() {
                continue;
            }
            if !seen.insert(summary.model_id.clone()) {
                continue;
            }
            models.push(ModelDescriptor {
                provider: descriptor.provider.clone(),
                transport: descriptor.transport.clone(),
                support_tier: SupportTier::Documented,
                exact_model_id: ModelId::new(summary.model_id)
                    .map_err(|_| ErrorCode::MalformedOutput)?,
                account_scoped_available: true,
                region: Some(region.clone()),
                max_context_tokens: BEDROCK_CONSERVATIVE_MAX_CONTEXT_TOKENS,
                max_output_tokens: BEDROCK_CONSERVATIVE_MAX_OUTPUT_TOKENS,
                capabilities: bedrock_capabilities(),
                billing_basis: BillingBasis::MeteredEstimate,
                tariff_version: None,
                deprecated: summary
                    .model_lifecycle
                    .as_ref()
                    .is_some_and(|lifecycle| lifecycle.status != "ACTIVE"),
                benchmarked_for_live: false,
            });
        }

        for profile in self.list_inference_profiles(&region, &cancel)? {
            if profile.status.as_deref() != Some("ACTIVE") {
                continue;
            }
            if !seen.insert(profile.inference_profile_id.clone()) {
                continue;
            }
            models.push(ModelDescriptor {
                provider: descriptor.provider.clone(),
                transport: descriptor.transport.clone(),
                support_tier: SupportTier::Documented,
                exact_model_id: ModelId::new(profile.inference_profile_id)
                    .map_err(|_| ErrorCode::MalformedOutput)?,
                account_scoped_available: true,
                region: Some(region.clone()),
                max_context_tokens: BEDROCK_CONSERVATIVE_MAX_CONTEXT_TOKENS,
                max_output_tokens: BEDROCK_CONSERVATIVE_MAX_OUTPUT_TOKENS,
                capabilities: bedrock_capabilities(),
                billing_basis: BillingBasis::MeteredEstimate,
                tariff_version: None,
                deprecated: false,
                benchmarked_for_live: false,
            });
        }

        let catalog = ModelCatalog { models };
        self.active_region = Some(region);
        self.catalog = catalog.clone();
        Ok(catalog)
    }

    fn list_foundation_models(
        &mut self,
        region: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<FoundationModelSummary>, PostprocessError> {
        let mut url = control_plane_url(region, "/foundation-models")?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("byOutputModality", "TEXT");
            query.append_pair("byInferenceType", "ON_DEMAND");
        }
        let page: FoundationModelPage = self.control_plane_get(url, region, cancel)?;
        Ok(page.model_summaries)
    }

    fn list_inference_profiles(
        &mut self,
        region: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<InferenceProfileSummary>, PostprocessError> {
        let mut summaries = Vec::new();
        let mut next_token: Option<String> = None;
        let mut seen_tokens = HashSet::new();
        for _ in 0..MAX_CATALOG_PAGES {
            let mut url = control_plane_url(region, "/inference-profiles")?;
            {
                let mut query = url.query_pairs_mut();
                query.append_pair("maxResults", "1000");
                if let Some(token) = next_token.as_deref() {
                    query.append_pair("nextToken", token);
                }
            }
            let page: InferenceProfilePage = self.control_plane_get(url, region, cancel)?;
            summaries.extend(page.inference_profile_summaries);
            match page.next_token {
                Some(token) if seen_tokens.insert(token.clone()) => next_token = Some(token),
                Some(_) => return Err(ErrorCode::MalformedOutput.into()),
                None => return Ok(summaries),
            }
        }
        Err(ErrorCode::Provider.into())
    }

    fn control_plane_get<T: for<'de> Deserialize<'de>>(
        &mut self,
        url: Url,
        region: &str,
        cancel: &CancellationToken,
    ) -> Result<T, PostprocessError> {
        let now = self.clock.monotonic_micros();
        let deadline = MonotonicDeadline(now.saturating_add(CATALOG_TIMEOUT_MICROS));
        let wire = HttpRequest::new(HttpMethod::Get, url)
            .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
            .with_public_header("accept", "application/json")
            .map_err(|_| PostprocessError::from(ErrorCode::Internal))?
            .with_timeout(request_timeout(deadline, now)?);
        let wire = self.sign(wire, region).map_err(PostprocessError::from)?;
        let mut exchange = send_with_retry(
            &mut *self.transport,
            &*self.clock,
            &wire,
            cancel,
            deadline,
            None,
        )
        .map_err(|failure| PostprocessError::from(failure.code))?;
        if exchange.response.status() != 200 {
            let code = bedrock_http_code(
                exchange.response.status(),
                exchange.response.header("x-amzn-errortype"),
            );
            if code == ErrorCode::AuthRejected {
                self.credentials.mark_rejected();
            }
            return Err(code.into());
        }
        let bytes = read_body_limited(
            &mut exchange.response,
            MAX_CATALOG_BODY_BYTES,
            cancel,
            deadline,
            &*self.clock,
        )?;
        serde_json::from_slice(&bytes).map_err(|_| ErrorCode::MalformedOutput.into())
    }

    fn sign(&mut self, request: HttpRequest, region: &str) -> Result<HttpRequest, ErrorCode> {
        let credentials = self.credentials.resolve().map_err(credential_code)?;
        let timestamp = SigningTimestamp::from_unix_seconds(self.wall_clock.unix_seconds());
        sign_request(
            request,
            &credentials,
            &SigningScope {
                region,
                service: RUNTIME_SERVICE,
            },
            &timestamp,
        )
        .map_err(|error| error.code)
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
        let region = self
            .active_region
            .clone()
            .ok_or_else(|| ExecFailure::new(ErrorCode::ModelUnavailable, false))?;
        validate_bedrock_request(
            request,
            &self.catalog,
            Some(region.as_str()),
            self.options.max_output_tokens,
        )
        .map_err(ExecFailure::from_error)?;
        // ListFoundationModels does not disclose which models honour Converse cache points, so explicit
        // prefix caching is not offered at all rather than claimed and silently ignored.
        if request.cache_policy.provider != ProviderCacheMode::Off {
            return Err(ExecFailure::new(ErrorCode::PolicyBlocked, false));
        }

        let body = bedrock_request_body(request, self.options.max_output_tokens)
            .map_err(ExecFailure::from_error)?;
        let auth_start = self.clock.monotonic_micros();
        let now = auth_start;
        let wire = HttpRequest::new(
            HttpMethod::Post,
            converse_stream_url(&region, request.model.as_str())
                .map_err(ExecFailure::from_error)?,
        )
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_public_header("accept", BEDROCK_EVENT_STREAM_CONTENT_TYPE)
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_public_header("content-type", "application/json")
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_json_body(body)
        .with_timeout(request_timeout(request.deadline, now).map_err(ExecFailure::from_error)?);
        let wire = self
            .sign(wire, &region)
            .map_err(|code| ExecFailure::new(code, false))?;
        timing.auth_us = Some(self.clock.monotonic_micros().saturating_sub(auth_start));
        if let Some(code) = boundary_code(cancel, request.deadline, self.clock.monotonic_micros()) {
            return Err(ExecFailure::new(code, false));
        }

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
            let code = bedrock_http_code(
                exchange.response.status(),
                exchange.response.header("x-amzn-errortype"),
            );
            return Err(ExecFailure::new(code, true));
        }
        validate_binary_event_stream(&exchange.response)
            .map_err(|error| ExecFailure::new(error.code, true))?;

        let mut decoder = EventStreamDecoder::new(self.options.max_stream_bytes);
        let mut state = BedrockStreamState::new(self.options.max_stream_bytes);
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
            let messages = decoder.push(&chunk).map_err(|error| ExecFailure {
                code: error.code,
                usage: state.terminal_usage,
                dispatched: true,
            })?;
            let mut buffered_boundary = None;
            for message in messages {
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
                    .process(&message, request, event_sink, &*self.clock)
                    .map_err(|failure| failure.with_dispatched())?;
                if buffered_boundary.is_none() {
                    buffered_boundary =
                        boundary_code(cancel, request.deadline, self.clock.monotonic_micros());
                }
            }
            if let Some(code) = buffered_boundary {
                // Terminal usage already buffered in this chunk is reconciled, but no late text is
                // emitted and no further network chunk is read after cancellation.
                exchange.response.body_mut().cancel();
                return Err(ExecFailure {
                    code,
                    usage: state.terminal_usage,
                    dispatched: true,
                });
            }
        }
        decoder.finish().map_err(|error| ExecFailure {
            code: error.code,
            usage: state.terminal_usage,
            dispatched: true,
        })?;
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

impl fmt::Debug for BedrockConverseAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BedrockConverseAdapter")
            .field("descriptor", &KnownTransport::BedrockRuntime.descriptor())
            .field("options", &self.options)
            .field("catalog_models", &self.catalog.models.len())
            .field("region_configured", &self.active_region.is_some())
            .field("credential", &"<injected>")
            .finish()
    }
}

impl ProviderAdapter for BedrockConverseAdapter {
    fn descriptor(&self) -> ProviderDescriptor {
        KnownTransport::BedrockRuntime.descriptor()
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
                // One terminal edge owns rejection for both HTTP and in-stream authentication failures.
                if failure.code == ErrorCode::AuthRejected {
                    self.credentials.mark_rejected();
                }
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

const fn bedrock_capabilities() -> AdapterCapabilities {
    AdapterCapabilities {
        text_input: true,
        text_output: true,
        streaming: true,
        structured_output: true,
        // Converse cache points exist for some models, but the catalog API does not say which.
        explicit_prefix_cache: false,
        implicit_cache_may_apply: false,
    }
}

fn is_plausible_region(region: &str) -> bool {
    !region.is_empty()
        && region.len() <= 32
        && region
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn control_plane_url(region: &str, path: &str) -> Result<Url, PostprocessError> {
    if !is_plausible_region(region) {
        return Err(ErrorCode::Internal.into());
    }
    Url::parse(&format!("https://bedrock.{region}.amazonaws.com{path}"))
        .map_err(|_| ErrorCode::Internal.into())
}

fn converse_stream_url(region: &str, model: &str) -> Result<Url, PostprocessError> {
    if !is_plausible_region(region) {
        return Err(ErrorCode::Internal.into());
    }
    // Bedrock model ids and inference-profile ids embed `:` and `.`; the id is percent-encoded once for
    // the wire, and SigV4 canonicalization encodes the resulting path a second time.
    Url::parse(&format!(
        "https://bedrock-runtime.{region}.amazonaws.com/model/{}/converse-stream",
        uri_encode(model)
    ))
    .map_err(|_| ErrorCode::Internal.into())
}

fn validate_bedrock_request(
    request: &HostedRequest,
    catalog: &ModelCatalog,
    region: Option<&str>,
    configured_max_output: u64,
) -> Result<(), PostprocessError> {
    let descriptor = KnownTransport::BedrockRuntime.descriptor();
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

fn bedrock_request_body(
    request: &HostedRequest,
    max_output_tokens: u64,
) -> Result<Vec<u8>, PostprocessError> {
    let prompt = request.prompt.messages();
    let system = prompt[..2]
        .iter()
        .map(|message| json!({"text": message.content()}))
        .collect::<Vec<_>>();
    let content = prompt[2..]
        .iter()
        .map(|message| json!({"text": message.content()}))
        .collect::<Vec<_>>();
    let tool_name = output_schema_name(request.prompt.task());
    let body = json!({
        "system": system,
        "messages": [{"role": "user", "content": content}],
        "inferenceConfig": {"maxTokens": max_output_tokens, "temperature": 0},
        // Converse has no JSON-schema response format; a single forced tool is the structured-output
        // lever, and its streamed input fragments are what the Live lane renders.
        "toolConfig": {
            "tools": [{
                "toolSpec": {
                    "name": tool_name,
                    "description": "Return the corti result object. Emit no other tool call.",
                    "inputSchema": {"json": output_schema(request.prompt.task())}
                }
            }],
            "toolChoice": {"tool": {"name": tool_name}}
        }
    });
    json_bytes(&body)
}

fn validate_binary_event_stream(
    response: &crate::transport::HttpResponse,
) -> Result<(), PostprocessError> {
    let content_type = response
        .header("content-type")
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if content_type != Some(BEDROCK_EVENT_STREAM_CONTENT_TYPE) {
        return Err(ErrorCode::MalformedOutput.into());
    }
    Ok(())
}

struct BedrockStreamState {
    text: TextCollector,
    started: bool,
    stopped: bool,
    tool_block_seen: bool,
    open_tool_index: Option<u64>,
    stop_reason: Option<String>,
    terminal_usage: Option<NormalizedUsage>,
    terminal_at: Option<u64>,
}

impl BedrockStreamState {
    fn new(max_bytes: usize) -> Self {
        Self {
            text: TextCollector::new(max_bytes),
            started: false,
            stopped: false,
            tool_block_seen: false,
            open_tool_index: None,
            stop_reason: None,
            terminal_usage: None,
            terminal_at: None,
        }
    }

    fn process(
        &mut self,
        message: &EventStreamMessage,
        request: &HostedRequest,
        sink: &dyn ProviderEventSink,
        clock: &dyn Clock,
    ) -> Result<(), ExecFailure> {
        let malformed = || ExecFailure::new(ErrorCode::MalformedOutput, true);
        match message.message_type() {
            Some("event") => {}
            Some("exception") => {
                return Err(ExecFailure {
                    code: bedrock_error_code(message.exception_type().unwrap_or_default()),
                    usage: self.terminal_usage,
                    dispatched: true,
                });
            }
            // A modelled `error` frame or an unlabelled frame is a protocol violation, not content.
            _ => return Err(ExecFailure::new(ErrorCode::Provider, true)),
        }
        let event_type = message.event_type().ok_or_else(malformed)?;
        if self.stopped {
            return Err(malformed());
        }
        match event_type {
            "messageStart" => {
                if self.started {
                    return Err(malformed());
                }
                self.started = true;
            }
            "contentBlockStart" => {
                if !self.started || self.open_tool_index.is_some() || self.tool_block_seen {
                    return Err(malformed());
                }
                let payload: BlockStart =
                    serde_json::from_slice(message.payload()).map_err(|_| malformed())?;
                let tool = payload.start.tool_use.ok_or_else(malformed)?;
                if tool.name != output_schema_name(request.prompt.task()) {
                    return Err(malformed());
                }
                self.open_tool_index = Some(payload.content_block_index);
                self.tool_block_seen = true;
            }
            "contentBlockDelta" => {
                let payload: BlockDelta =
                    serde_json::from_slice(message.payload()).map_err(|_| malformed())?;
                if self.open_tool_index != Some(payload.content_block_index) {
                    return Err(malformed());
                }
                // A `text` delta alongside a forced tool call is reasoning prose, not the result; it is
                // neither collected nor forwarded, so the parsed output stays exactly the tool input.
                let Some(tool) = payload.delta.tool_use else {
                    return Ok(());
                };
                self.text
                    .push(&tool.input, request, sink, clock)
                    .map_err(|error| ExecFailure::new(error.code, true))?;
            }
            "contentBlockStop" => {
                let payload: BlockStop =
                    serde_json::from_slice(message.payload()).map_err(|_| malformed())?;
                if self.open_tool_index != Some(payload.content_block_index) {
                    return Err(malformed());
                }
                self.open_tool_index = None;
            }
            "messageStop" => {
                if !self.started || self.open_tool_index.is_some() || !self.tool_block_seen {
                    return Err(malformed());
                }
                let payload: MessageStop =
                    serde_json::from_slice(message.payload()).map_err(|_| malformed())?;
                match payload.stop_reason.as_str() {
                    "tool_use" | "end_turn" => {}
                    "max_tokens" => return Err(malformed()),
                    _ => return Err(ExecFailure::new(ErrorCode::Provider, true)),
                }
                self.stop_reason = Some(payload.stop_reason);
            }
            "metadata" => {
                // Bedrock reports usage only once, in the trailing metadata frame; there is no
                // provisional figure to disclose earlier.
                if self.terminal_usage.is_some() {
                    return Err(malformed());
                }
                let payload: MetadataEvent =
                    serde_json::from_slice(message.payload()).map_err(|_| malformed())?;
                self.terminal_usage = Some(
                    payload
                        .usage
                        .normalized()
                        .map_err(|code| ExecFailure::new(code, true))?,
                );
                self.terminal_at = Some(clock.monotonic_micros());
                self.stopped = true;
            }
            // AWS documents that new event types may appear on this stream.
            _ => {}
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), ExecFailure> {
        if !self.stopped || self.stop_reason.is_none() || self.text.text().is_empty() {
            return Err(ExecFailure {
                code: ErrorCode::MalformedOutput,
                usage: self.terminal_usage,
                dispatched: true,
            });
        }
        Ok(())
    }
}

fn bedrock_error_code(kind: &str) -> ErrorCode {
    // AWS sends the exception name either bare or as a Smithy shape id (`com.amazon...#Name`).
    let name = kind.rsplit(['#', '.']).next().unwrap_or(kind);
    match name {
        "AccessDeniedException" => ErrorCode::Permission,
        "ThrottlingException" => ErrorCode::RateLimited,
        "ExpiredTokenException"
        | "InvalidSignatureException"
        | "UnrecognizedClientException"
        | "IncompleteSignatureException" => ErrorCode::AuthRejected,
        "ServiceQuotaExceededException" => ErrorCode::Quota,
        "ResourceNotFoundException" => ErrorCode::ModelUnavailable,
        "ModelTimeoutException" => ErrorCode::Timeout,
        _ => ErrorCode::Provider,
    }
}

fn bedrock_http_code(status: u16, error_type: Option<&str>) -> ErrorCode {
    // `x-amzn-errortype` distinguishes an expired session from a plain 403, which the status alone
    // cannot; the status is the fallback when the header is absent.
    if let Some(error_type) = error_type.filter(|value| !value.is_empty()) {
        let code = bedrock_error_code(error_type);
        if code != ErrorCode::Provider {
            return code;
        }
    }
    match status {
        400 => ErrorCode::Provider,
        401 => ErrorCode::AuthRejected,
        403 => ErrorCode::Permission,
        404 => ErrorCode::ModelUnavailable,
        408 | 504 => ErrorCode::Timeout,
        429 => ErrorCode::RateLimited,
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
#[serde(rename_all = "camelCase")]
struct FoundationModelPage {
    #[serde(default)]
    model_summaries: Vec<FoundationModelSummary>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FoundationModelSummary {
    model_id: String,
    #[serde(default)]
    output_modalities: Vec<String>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    response_streaming_supported: Option<bool>,
    #[serde(default)]
    model_lifecycle: Option<ModelLifecycle>,
}

impl FoundationModelSummary {
    fn is_selectable(&self) -> bool {
        self.response_streaming_supported.unwrap_or(false)
            && self.input_modalities.iter().any(|value| value == "TEXT")
            && self.output_modalities.iter().any(|value| value == "TEXT")
    }
}

#[derive(Deserialize)]
struct ModelLifecycle {
    status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InferenceProfilePage {
    #[serde(default)]
    inference_profile_summaries: Vec<InferenceProfileSummary>,
    #[serde(default)]
    next_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InferenceProfileSummary {
    inference_profile_id: String,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockStart {
    content_block_index: u64,
    start: BlockStartBody,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockStartBody {
    #[serde(default)]
    tool_use: Option<ToolUseStart>,
}

#[derive(Deserialize)]
struct ToolUseStart {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockDelta {
    content_block_index: u64,
    delta: BlockDeltaBody,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockDeltaBody {
    #[serde(default)]
    tool_use: Option<ToolUseDelta>,
}

#[derive(Deserialize)]
struct ToolUseDelta {
    #[serde(default)]
    input: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockStop {
    content_block_index: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageStop {
    stop_reason: String,
}

#[derive(Deserialize)]
struct MetadataEvent {
    usage: BedrockUsage,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BedrockUsage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    cache_write_input_tokens: Option<i64>,
}

impl BedrockUsage {
    fn normalized(self) -> Result<NormalizedUsage, ErrorCode> {
        NormalizedUsage::try_from(RawUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_read_tokens: self.cache_read_input_tokens,
            cached_write_tokens: self.cache_write_input_tokens,
            reasoning_tokens: None,
            usage_complete: self.input_tokens.is_some() && self.output_tokens.is_some(),
        })
        .map_err(|_| ErrorCode::MalformedOutput)
    }
}
