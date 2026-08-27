use std::{collections::HashSet, fmt};

use corti_postprocess::{
    AdapterCapabilities, BillingBasis, CancellationToken, ErrorCode, HostedRequest, KnownTransport,
    ModelCatalog, ModelDescriptor, ModelId, NormalizedUsage, PostprocessError, ProviderAdapter,
    ProviderCacheMode, ProviderDescriptor, ProviderEventKind, ProviderEventSink, ProviderScope,
    ProviderTerminal, SupportTier,
};
use serde::Deserialize;
use url::Url;

use crate::{
    anthropic_wire::{AnthropicStreamState, AnthropicWire, WireFailure},
    common::{
        DISCARD_EVENT_SINK, DirectAdapterOptions, MAX_CATALOG_BODY_BYTES, SendFailure, Timing,
        boundary_code, credential_code, emit, http_status_code, parse_output, read_body_limited,
        request_timeout, send_with_retry, terminal_cache_observation, usage_cache_observations,
        validate_event_stream_response, validate_prompt_layout,
    },
    sse::SseDecoder,
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

        let body = crate::anthropic_wire::request_body(
            request,
            self.options.max_output_tokens,
            AnthropicWire::Direct {
                region: self.active_region.as_deref(),
            },
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
            return Err(ExecFailure::new(code, true));
        }
        validate_event_stream_response(&exchange.response)
            .map_err(|error| ExecFailure::new(error.code, true))?;

        let mut decoder = SseDecoder::new(self.options.max_stream_bytes);
        let mut state =
            AnthropicStreamState::new(self.options.max_stream_bytes, Some(request.model.as_str()));
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
                    .map_err(|failure| ExecFailure::from(failure).with_dispatched())?;
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
                .map_err(|failure| ExecFailure::from(failure).with_dispatched())?;
        }
        state
            .finish()
            .map_err(|failure| ExecFailure::from(failure).with_dispatched())?;
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

#[derive(Debug, Clone, Copy)]
struct ExecFailure {
    code: ErrorCode,
    usage: Option<NormalizedUsage>,
    dispatched: bool,
}

impl From<WireFailure> for ExecFailure {
    fn from(failure: WireFailure) -> Self {
        Self {
            code: failure.code,
            usage: failure.usage,
            dispatched: failure.dispatched,
        }
    }
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

fn parse_url(value: &str) -> Result<Url, PostprocessError> {
    Url::parse(value).map_err(|_| ErrorCode::Internal.into())
}
