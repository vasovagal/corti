use std::{collections::HashSet, fmt};

use corti_postprocess::{
    AdapterCapabilities, BillingBasis, CacheObservation, CancellationToken, ErrorCode,
    HostedRequest, KnownTransport, ModelCatalog, ModelDescriptor, ModelId, NormalizedUsage,
    PostprocessError, ProviderAdapter, ProviderCacheMode, ProviderDescriptor, ProviderEventKind,
    ProviderEventSink, ProviderScope, ProviderTerminal, RawUsage, SupportTier,
};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;
use url::Url;

use crate::{
    anthropic_wire::{AnthropicStreamState, AnthropicWire, WireFailure},
    common::{
        DISCARD_EVENT_SINK, DirectAdapterOptions, SendFailure, TextCollector, Timing,
        boundary_code, credential_code, emit, http_status_code, json_bytes, parse_output,
        read_body_limited, request_timeout, send_with_retry, terminal_cache_observation,
        usage_cache_observations, validate_event_stream_response, validate_prompt_layout,
    },
    schema::output_schema,
    sse::{SseDecoder, SseEvent},
    transport::{AccessToken, Clock, CredentialError, HttpMethod, HttpRequest, HttpTransport},
};

pub const VERTEX_REST_ADAPTER_VERSION: u32 = 1;

const MAX_VERTEX_ERROR_BODY_BYTES: usize = 256 * 1024;

/// Gemini limits, used for a model whose id carries no other signal.
const GEMINI_MAX_CONTEXT_TOKENS: u64 = 1_000_000;
const GEMINI_MAX_OUTPUT_TOKENS: u64 = 65_536;
const CLAUDE_MAX_CONTEXT_TOKENS: u64 = 200_000;
const CLAUDE_MAX_OUTPUT_TOKENS: u64 = 64_000;

/// The Model Garden publisher a model is served under. Vertex fronts each publisher's own API rather than a
/// common one, so this picks the request body, the URL verb, and the stream codec together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexPublisher {
    Google,
    Anthropic,
}

impl VertexPublisher {
    /// Derived from the id, not configured alongside it: `validate_model_id` rejects `/`, so a
    /// `publishers/anthropic/...` prefix is not expressible, and a bare id is what operators type.
    pub fn for_model_id(id: &str) -> Self {
        if id.starts_with("claude-") {
            Self::Anthropic
        } else {
            Self::Google
        }
    }

    const fn path_segment(self) -> &'static str {
        match self {
            Self::Google => "google",
            Self::Anthropic => "anthropic",
        }
    }
}

/// One memory-only ADC access-token lease. Refresh credentials and ADC file paths cannot be represented.
pub struct AdcAccessToken {
    token: AccessToken,
    expires_at_unix_ms: Option<i64>,
}

impl AdcAccessToken {
    pub const fn new(token: AccessToken, expires_at_unix_ms: Option<i64>) -> Self {
        Self {
            token,
            expires_at_unix_ms,
        }
    }

    pub const fn expires_at_unix_ms(&self) -> Option<i64> {
        self.expires_at_unix_ms
    }

    fn into_token(self) -> AccessToken {
        self.token
    }
}

impl fmt::Debug for AdcAccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdcAccessToken")
            .field("token", &"<redacted>")
            .field("expires_at_unix_ms", &self.expires_at_unix_ms)
            .finish()
    }
}

/// Injected Application Default Credentials seam. This crate provides no ambient ADC discovery and never
/// reads a credential file or invokes `gcloud`; the application-owned resolver supplies memory-only leases.
pub trait AdcAccessTokenSource: Send {
    fn resolve_access_token(&mut self) -> Result<AdcAccessToken, CredentialError>;

    fn mark_rejected(&mut self) {}
}

/// Non-secret Vertex project routing and quota attribution. Debug output omits project identifiers because
/// provider adapters can appear in diagnostics even though project metadata must not enter call telemetry.
#[derive(Clone, PartialEq, Eq)]
pub struct VertexProjectMetadata {
    project_id: String,
    region: String,
    quota_project_id: Option<String>,
}

impl VertexProjectMetadata {
    pub fn new(
        project_id: impl Into<String>,
        region: impl Into<String>,
        quota_project_id: Option<String>,
    ) -> Result<Self, VertexConfigurationError> {
        let project_id = project_id.into();
        let region = region.into();
        validate_project_id(&project_id).map_err(|_| VertexConfigurationError::InvalidProject)?;
        validate_region(&region).map_err(|_| VertexConfigurationError::InvalidRegion)?;
        if let Some(quota_project_id) = quota_project_id.as_deref() {
            validate_project_id(quota_project_id)
                .map_err(|_| VertexConfigurationError::InvalidQuotaProject)?;
        }
        Ok(Self {
            project_id,
            region,
            quota_project_id,
        })
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn region(&self) -> &str {
        &self.region
    }

    pub fn quota_project_id(&self) -> Option<&str> {
        self.quota_project_id.as_deref()
    }
}

impl fmt::Debug for VertexProjectMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexProjectMetadata")
            .field("project_configured", &true)
            .field("region", &self.region)
            .field("quota_project_configured", &self.quota_project_id.is_some())
            .finish()
    }
}

/// One exact model from an authenticated project/region capability snapshot. The provider factory, not this
/// crate, owns how that snapshot is obtained. No alias or default model is added by the adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexModel {
    exact_model_id: ModelId,
    publisher: VertexPublisher,
    max_context_tokens: u64,
    max_output_tokens: u64,
    implicit_cache_may_apply: bool,
    deprecated: bool,
    benchmarked_for_live: bool,
    tariff_version: Option<String>,
}

impl VertexModel {
    pub fn new(
        exact_model_id: ModelId,
        max_context_tokens: u64,
        max_output_tokens: u64,
    ) -> Result<Self, VertexConfigurationError> {
        validate_model_id(exact_model_id.as_str())
            .map_err(|_| VertexConfigurationError::InvalidModel)?;
        if max_context_tokens == 0 || max_output_tokens == 0 {
            return Err(VertexConfigurationError::InvalidModelLimits);
        }
        let publisher = VertexPublisher::for_model_id(exact_model_id.as_str());
        Ok(Self {
            exact_model_id,
            publisher,
            max_context_tokens,
            max_output_tokens,
            // Vertex Gemini models can apply implicit caching without a request cache resource. Callers must
            // explicitly classify a verified model otherwise before selecting ProviderCacheMode::Off. Claude
            // on Vertex caches only against an explicit prefix marker, as it does on the direct API.
            implicit_cache_may_apply: publisher == VertexPublisher::Google,
            deprecated: false,
            benchmarked_for_live: false,
            tariff_version: None,
        })
    }

    /// Publisher and documented limits from the id alone, for a model an operator typed rather than one the
    /// caller has a capability snapshot for.
    pub fn inferred(exact_model_id: ModelId) -> Result<Self, VertexConfigurationError> {
        let (context, output) = match VertexPublisher::for_model_id(exact_model_id.as_str()) {
            VertexPublisher::Google => (GEMINI_MAX_CONTEXT_TOKENS, GEMINI_MAX_OUTPUT_TOKENS),
            VertexPublisher::Anthropic => (CLAUDE_MAX_CONTEXT_TOKENS, CLAUDE_MAX_OUTPUT_TOKENS),
        };
        Self::new(exact_model_id, context, output)
    }

    pub const fn publisher(&self) -> VertexPublisher {
        self.publisher
    }

    pub const fn with_implicit_cache_may_apply(mut self, value: bool) -> Self {
        self.implicit_cache_may_apply = value;
        self
    }

    pub const fn with_deprecated(mut self, value: bool) -> Self {
        self.deprecated = value;
        self
    }

    pub const fn with_benchmarked_for_live(mut self, value: bool) -> Self {
        self.benchmarked_for_live = value;
        self
    }

    pub fn with_tariff_version(mut self, value: impl Into<String>) -> Self {
        self.tariff_version = Some(value.into());
        self
    }

    pub const fn exact_model_id(&self) -> &ModelId {
        &self.exact_model_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VertexConfigurationError {
    #[error("invalid Vertex project metadata")]
    InvalidProject,
    #[error("invalid Vertex region metadata")]
    InvalidRegion,
    #[error("invalid Vertex quota-project metadata")]
    InvalidQuotaProject,
    #[error("invalid Vertex model id")]
    InvalidModel,
    #[error("invalid Vertex model limits")]
    InvalidModelLimits,
    #[error("duplicate Vertex model id")]
    DuplicateModel,
    #[error("Vertex catalog is empty")]
    EmptyCatalog,
}

pub struct VertexRestAdapter {
    transport: Box<dyn HttpTransport>,
    clock: Box<dyn Clock>,
    credentials: Box<dyn AdcAccessTokenSource>,
    metadata: VertexProjectMetadata,
    models: Vec<VertexModel>,
    options: DirectAdapterOptions,
    catalog: ModelCatalog,
    catalog_armed: bool,
}

impl VertexRestAdapter {
    pub fn new(
        transport: Box<dyn HttpTransport>,
        clock: Box<dyn Clock>,
        credentials: Box<dyn AdcAccessTokenSource>,
        metadata: VertexProjectMetadata,
        models: impl IntoIterator<Item = VertexModel>,
    ) -> Result<Self, VertexConfigurationError> {
        let models = models.into_iter().collect::<Vec<_>>();
        if models.is_empty() {
            return Err(VertexConfigurationError::EmptyCatalog);
        }
        let mut seen = HashSet::new();
        if models
            .iter()
            .any(|model| !seen.insert(model.exact_model_id.as_str().to_owned()))
        {
            return Err(VertexConfigurationError::DuplicateModel);
        }
        Ok(Self {
            transport,
            clock,
            credentials,
            metadata,
            models,
            options: DirectAdapterOptions::default(),
            catalog: ModelCatalog { models: Vec::new() },
            catalog_armed: false,
        })
    }

    pub fn with_options(mut self, options: DirectAdapterOptions) -> Result<Self, PostprocessError> {
        self.options = options.validate()?;
        Ok(self)
    }

    pub const fn metadata(&self) -> &VertexProjectMetadata {
        &self.metadata
    }

    fn catalog_inner(&mut self, scope: &ProviderScope) -> Result<ModelCatalog, PostprocessError> {
        if scope.region.as_deref() != Some(self.metadata.region()) {
            return Err(ErrorCode::PolicyBlocked.into());
        }
        let descriptor = KnownTransport::VertexDirect.descriptor();
        let models = self
            .models
            .iter()
            .map(|model| ModelDescriptor {
                provider: descriptor.provider.clone(),
                transport: descriptor.transport.clone(),
                support_tier: SupportTier::Documented,
                exact_model_id: model.exact_model_id.clone(),
                account_scoped_available: true,
                region: Some(self.metadata.region.clone()),
                max_context_tokens: model.max_context_tokens,
                max_output_tokens: model.max_output_tokens,
                capabilities: AdapterCapabilities {
                    text_input: true,
                    text_output: true,
                    streaming: true,
                    structured_output: true,
                    explicit_prefix_cache: model.publisher == VertexPublisher::Anthropic,
                    implicit_cache_may_apply: model.implicit_cache_may_apply,
                },
                billing_basis: BillingBasis::MeteredEstimate,
                tariff_version: model.tariff_version.clone(),
                deprecated: model.deprecated,
                benchmarked_for_live: model.benchmarked_for_live,
            })
            .collect();
        let catalog = ModelCatalog { models };
        self.catalog = catalog.clone();
        self.catalog_armed = true;
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
        let model = validate_vertex_request(
            request,
            &self.catalog,
            self.catalog_armed,
            self.metadata.region(),
            self.options.max_output_tokens,
        )
        .map_err(ExecFailure::from_error)?;

        let auth_start = self.clock.monotonic_micros();
        let token = self
            .credentials
            .resolve_access_token()
            .map_err(|error| ExecFailure::new(credential_code(error), false))?;
        timing.auth_us = Some(self.clock.monotonic_micros().saturating_sub(auth_start));
        if let Some(code) = boundary_code(cancel, request.deadline, self.clock.monotonic_micros()) {
            return Err(ExecFailure::new(code, false));
        }

        let publisher = VertexPublisher::for_model_id(request.model.as_str());
        let body = match publisher {
            VertexPublisher::Google => vertex_request_body(request, self.options.max_output_tokens),
            VertexPublisher::Anthropic => crate::anthropic_wire::request_body(
                request,
                self.options.max_output_tokens,
                AnthropicWire::Vertex,
            ),
        }
        .map_err(ExecFailure::from_error)?;
        let now = self.clock.monotonic_micros();
        let mut wire = HttpRequest::new(
            HttpMethod::Post,
            vertex_stream_url(&self.metadata, publisher, request.model.as_str())
                .map_err(ExecFailure::from_error)?,
        )
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_public_header("accept", "text/event-stream")
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_public_header("content-type", "application/json")
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?
        .with_access_token_header("authorization", token.into_token(), "Bearer ")
        .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?;
        if let Some(quota_project_id) = self.metadata.quota_project_id() {
            wire = wire
                .with_public_header("x-goog-user-project", quota_project_id)
                .map_err(|_| ExecFailure::new(ErrorCode::Internal, false))?;
        }
        let wire = wire
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
            let fallback = http_status_code(exchange.response.status());
            let bytes = read_body_limited(
                &mut exchange.response,
                MAX_VERTEX_ERROR_BODY_BYTES,
                cancel,
                request.deadline,
                &*self.clock,
            )
            .unwrap_or_default();
            let code = vertex_error_code(exchange.response.status(), &bytes).unwrap_or(fallback);
            return Err(ExecFailure::new(code, true));
        }
        validate_event_stream_response(&exchange.response)
            .map_err(|error| ExecFailure::new(error.code, true))?;

        let mut decoder = SseDecoder::new(self.options.max_stream_bytes);
        let mut state = VertexStream::new(publisher, self.options.max_stream_bytes);
        loop {
            if let Some(code) =
                boundary_code(cancel, request.deadline, self.clock.monotonic_micros())
            {
                exchange.response.body_mut().cancel();
                return Err(ExecFailure {
                    code,
                    usage: state.latest_usage(),
                    dispatched: true,
                });
            }
            let next = exchange
                .response
                .body_mut()
                .next_chunk()
                .map_err(|error| ExecFailure {
                    code: crate::common::transport_code(error, cancel),
                    usage: state.latest_usage(),
                    dispatched: true,
                })?;
            let Some(chunk) = next else {
                break;
            };
            let events = decoder.push(&chunk).map_err(|error| ExecFailure {
                code: error.code,
                usage: state.latest_usage(),
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
                exchange.response.body_mut().cancel();
                return Err(ExecFailure {
                    code,
                    usage: state.latest_usage(),
                    dispatched: true,
                });
            }
        }
        for event in decoder.finish().map_err(|error| ExecFailure {
            code: error.code,
            usage: state.latest_usage(),
            dispatched: true,
        })? {
            state
                .process(event, request, sink, &*self.clock)
                .map_err(|failure| failure.with_dispatched())?;
        }
        state
            .finish()
            .map_err(|failure| failure.with_dispatched())?;
        timing.first_text_at = state.text().first_text_at();
        timing.terminal_at = state.terminal_at();

        if let Some(code) = boundary_code(cancel, request.deadline, self.clock.monotonic_micros()) {
            return Err(ExecFailure {
                code,
                usage: state.latest_usage(),
                dispatched: true,
            });
        }
        let parse_start = self.clock.monotonic_micros();
        let output = parse_output(request.prompt.task(), state.text().text()).map_err(|error| {
            ExecFailure {
                code: error.code,
                usage: state.latest_usage(),
                dispatched: true,
            }
        })?;
        let completed_at = self.clock.monotonic_micros();
        timing.parse_us = Some(completed_at.saturating_sub(parse_start));
        let usage = state
            .latest_usage()
            .unwrap_or_else(NormalizedUsage::unknown);
        let cache = match publisher {
            VertexPublisher::Google => {
                if model.capabilities.implicit_cache_may_apply
                    && usage.cached_read_tokens.is_some_and(|tokens| tokens > 0)
                {
                    CacheObservation::ProviderImplicit
                } else {
                    CacheObservation::None
                }
            }
            VertexPublisher::Anthropic => terminal_cache_observation(usage),
        };
        Ok(ProviderTerminal {
            output,
            usage,
            latency: timing.latency(completed_at),
            cache,
        })
    }
}

impl fmt::Debug for VertexRestAdapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexRestAdapter")
            .field("descriptor", &KnownTransport::VertexDirect.descriptor())
            .field("metadata", &self.metadata)
            .field("configured_models", &self.models.len())
            .field("options", &self.options)
            .field("credential", &"<injected-adc>")
            .finish()
    }
}

impl ProviderAdapter for VertexRestAdapter {
    fn descriptor(&self) -> ProviderDescriptor {
        KnownTransport::VertexDirect.descriptor()
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
                if terminal.cache == CacheObservation::ProviderImplicit {
                    emit(
                        sink,
                        request,
                        ProviderEventKind::CacheObserved(CacheObservation::ProviderImplicit),
                    );
                } else {
                    for observation in usage_cache_observations(terminal.usage) {
                        emit(sink, request, ProviderEventKind::CacheObserved(observation));
                    }
                }
                emit(sink, request, ProviderEventKind::Completed(terminal.usage));
                Ok(terminal)
            }
            Err(failure) => {
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

fn validate_vertex_request<'a>(
    request: &HostedRequest,
    catalog: &'a ModelCatalog,
    catalog_armed: bool,
    region: &str,
    configured_max_output: u64,
) -> Result<&'a ModelDescriptor, PostprocessError> {
    let descriptor = KnownTransport::VertexDirect.descriptor();
    if request.provider != descriptor.provider || request.transport != descriptor.transport {
        return Err(ErrorCode::PolicyBlocked.into());
    }
    validate_prompt_layout(request)?;
    if !catalog_armed {
        return Err(ErrorCode::ModelUnavailable.into());
    }
    let model = catalog
        .find_exact(&request.model, Some(region))
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
    let cache_policy_matches = if model.capabilities.implicit_cache_may_apply {
        request.cache_policy.provider == ProviderCacheMode::UnavoidableImplicit
    } else if model.capabilities.explicit_prefix_cache {
        matches!(
            request.cache_policy.provider,
            ProviderCacheMode::Off | ProviderCacheMode::ExplicitStablePrefix
        )
    } else {
        request.cache_policy.provider == ProviderCacheMode::Off
    };
    if !cache_policy_matches {
        return Err(ErrorCode::PolicyBlocked.into());
    }
    Ok(model)
}

fn vertex_request_body(
    request: &HostedRequest,
    max_output_tokens: u64,
) -> Result<Vec<u8>, PostprocessError> {
    let prompt = request.prompt.messages();
    let system_parts = prompt[..2]
        .iter()
        .map(|message| json!({"text": message.content()}))
        .collect::<Vec<_>>();
    let user_parts = prompt[2..]
        .iter()
        .map(|message| json!({"text": message.content()}))
        .collect::<Vec<_>>();
    json_bytes(&json!({
        "systemInstruction": {
            "parts": system_parts
        },
        "contents": [{
            "role": "user",
            "parts": user_parts
        }],
        "generationConfig": {
            "candidateCount": 1,
            "maxOutputTokens": max_output_tokens,
            "responseMimeType": "application/json",
            "responseJsonSchema": output_schema(request.prompt.task())
        }
    }))
}

fn vertex_stream_url(
    metadata: &VertexProjectMetadata,
    publisher: VertexPublisher,
    model: &str,
) -> Result<Url, PostprocessError> {
    validate_model_id(model).map_err(|_| PostprocessError::from(ErrorCode::ModelUnavailable))?;
    let host = if metadata.region == "global" {
        "aiplatform.googleapis.com".to_owned()
    } else {
        format!("{}-aiplatform.googleapis.com", metadata.region)
    };
    // rawPredict streams SSE from the body's `"stream": true`; only generateContent needs `?alt=sse`.
    let verb = match publisher {
        VertexPublisher::Google => "streamGenerateContent?alt=sse",
        VertexPublisher::Anthropic => "streamRawPredict",
    };
    Url::parse(&format!(
        "https://{host}/v1/projects/{}/locations/{}/publishers/{}/models/{model}:{verb}",
        metadata.project_id,
        metadata.region,
        publisher.path_segment()
    ))
    .map_err(|_| ErrorCode::Internal.into())
}

struct VertexStreamState {
    text: TextCollector,
    usage: VertexUsageValues,
    latest_usage: Option<NormalizedUsage>,
    candidate_seen: bool,
    finished: bool,
    done_seen: bool,
    terminal_at: Option<u64>,
}

impl VertexStreamState {
    fn new(max_bytes: usize) -> Self {
        Self {
            text: TextCollector::new(max_bytes),
            usage: VertexUsageValues::default(),
            latest_usage: None,
            candidate_seen: false,
            finished: false,
            done_seen: false,
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
        if self.done_seen {
            return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
        }
        if event
            .event
            .as_deref()
            .is_some_and(|event| event != "message")
        {
            return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
        }
        if event.data == "[DONE]" {
            if !self.finished {
                return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
            }
            self.done_seen = true;
            return Ok(());
        }
        let chunk: VertexGenerateChunk = serde_json::from_str(&event.data)
            .map_err(|_| ExecFailure::new(ErrorCode::MalformedOutput, true))?;
        if let Some(error) = chunk.error {
            return Err(ExecFailure::new(vertex_stream_error_code(&error), true));
        }
        if chunk
            .prompt_feedback
            .and_then(|feedback| feedback.block_reason)
            .is_some_and(|reason| reason != "BLOCK_REASON_UNSPECIFIED")
        {
            return Err(ExecFailure::new(ErrorCode::PolicyBlocked, true));
        }
        if let Some(model_version) = chunk.model_version.as_deref()
            && resource_tail(model_version) != request.model.as_str()
        {
            return Err(ExecFailure::new(ErrorCode::ModelUnavailable, true));
        }
        if chunk.candidates.len() > 1 {
            return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
        }
        if let Some(candidate) = chunk.candidates.into_iter().next() {
            if self.finished || candidate.index.unwrap_or(0) != 0 {
                return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
            }
            self.candidate_seen = true;
            if let Some(content) = candidate.content {
                if content.role.as_deref().is_some_and(|role| role != "model") {
                    return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                }
                for part in content.parts {
                    if part.function_call.is_some()
                        || part.executable_code.is_some()
                        || part.code_execution_result.is_some()
                    {
                        return Err(ExecFailure::new(ErrorCode::PolicyBlocked, true));
                    }
                    if part.thought {
                        continue;
                    }
                    let Some(text) = part.text else {
                        return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                    };
                    self.text
                        .push(&text, request, sink, clock)
                        .map_err(|error| ExecFailure::new(error.code, true))?;
                }
            }
            if let Some(reason) = candidate.finish_reason.as_deref()
                && reason != "FINISH_REASON_UNSPECIFIED"
            {
                match reason {
                    "STOP" => {
                        self.finished = true;
                        self.terminal_at = Some(clock.monotonic_micros());
                    }
                    "MAX_TOKENS" => {
                        return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                    }
                    "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
                        return Err(ExecFailure::new(ErrorCode::PolicyBlocked, true));
                    }
                    "MALFORMED_FUNCTION_CALL" | "UNEXPECTED_TOOL_CALL" => {
                        return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
                    }
                    _ => return Err(ExecFailure::new(ErrorCode::Provider, true)),
                }
            }
        }
        if let Some(usage) = chunk.usage_metadata {
            self.usage
                .reconcile(usage)
                .map_err(|code| ExecFailure::new(code, true))?;
            let normalized = self
                .usage
                .normalized(self.finished)
                .map_err(|code| ExecFailure::new(code, true))?;
            self.latest_usage = Some(normalized);
            emit(
                sink,
                request,
                ProviderEventKind::UsageProvisional(normalized),
            );
        }
        if !self.candidate_seen && self.latest_usage.is_none() {
            return Err(ExecFailure::new(ErrorCode::MalformedOutput, true));
        }
        if self.finished {
            self.latest_usage = Some(
                self.usage
                    .normalized(true)
                    .map_err(|code| ExecFailure::new(code, true))?,
            );
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), ExecFailure> {
        if !self.finished || !self.candidate_seen || self.text.text().is_empty() {
            return Err(ExecFailure {
                code: ErrorCode::MalformedOutput,
                usage: self.latest_usage,
                dispatched: true,
            });
        }
        Ok(())
    }

    const fn latest_usage(&self) -> Option<NormalizedUsage> {
        self.latest_usage
    }
}

/// Vertex fronts each publisher's own streaming API, so the loop is shared but the codec is not.
enum VertexStream {
    Gemini(VertexStreamState),
    Anthropic(AnthropicStreamState),
}

impl VertexStream {
    fn new(publisher: VertexPublisher, max_bytes: usize) -> Self {
        match publisher {
            VertexPublisher::Google => Self::Gemini(VertexStreamState::new(max_bytes)),
            // No expected model id: the URL path pins routing, and Vertex echoes the id it resolved to
            // (`claude-sonnet-4-5@20250929` comes back as `claude-sonnet-4-5-20250929`).
            VertexPublisher::Anthropic => {
                Self::Anthropic(AnthropicStreamState::new(max_bytes, None))
            }
        }
    }

    fn process(
        &mut self,
        event: SseEvent,
        request: &HostedRequest,
        sink: &dyn ProviderEventSink,
        clock: &dyn Clock,
    ) -> Result<(), ExecFailure> {
        match self {
            Self::Gemini(state) => state.process(event, request, sink, clock),
            Self::Anthropic(state) => state
                .process(event, request, sink, clock)
                .map_err(ExecFailure::from),
        }
    }

    fn finish(&self) -> Result<(), ExecFailure> {
        match self {
            Self::Gemini(state) => state.finish(),
            Self::Anthropic(state) => state.finish().map_err(ExecFailure::from),
        }
    }

    const fn latest_usage(&self) -> Option<NormalizedUsage> {
        match self {
            Self::Gemini(state) => state.latest_usage(),
            Self::Anthropic(state) => state.terminal_usage,
        }
    }

    const fn text(&self) -> &TextCollector {
        match self {
            Self::Gemini(state) => &state.text,
            Self::Anthropic(state) => &state.text,
        }
    }

    const fn terminal_at(&self) -> Option<u64> {
        match self {
            Self::Gemini(state) => state.terminal_at,
            Self::Anthropic(state) => state.terminal_at,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct VertexUsageValues {
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cached_read_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
}

impl VertexUsageValues {
    fn reconcile(&mut self, update: VertexUsageMetadata) -> Result<(), ErrorCode> {
        reconcile_usage_field(&mut self.input_tokens, update.prompt_token_count)?;
        reconcile_usage_field(&mut self.output_tokens, update.candidates_token_count)?;
        reconcile_usage_field(
            &mut self.cached_read_tokens,
            update.cached_content_token_count,
        )?;
        reconcile_usage_field(&mut self.reasoning_tokens, update.thoughts_token_count)?;
        Ok(())
    }

    fn normalized(self, terminal: bool) -> Result<NormalizedUsage, ErrorCode> {
        let complete = terminal && self.input_tokens.is_some() && self.output_tokens.is_some();
        NormalizedUsage::try_from(RawUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cached_read_tokens: self.cached_read_tokens,
            cached_write_tokens: None,
            reasoning_tokens: self.reasoning_tokens,
            usage_complete: complete,
        })
        .map_err(|_| ErrorCode::MalformedOutput)
    }
}

fn reconcile_usage_field(current: &mut Option<i64>, update: Option<i64>) -> Result<(), ErrorCode> {
    let Some(update) = update else {
        return Ok(());
    };
    if update < 0 || current.is_some_and(|current| update < current) {
        return Err(ErrorCode::MalformedOutput);
    }
    *current = Some(update);
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

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VertexGenerateChunk {
    #[serde(default)]
    candidates: Vec<VertexCandidate>,
    #[serde(default)]
    usage_metadata: Option<VertexUsageMetadata>,
    #[serde(default)]
    prompt_feedback: Option<VertexPromptFeedback>,
    #[serde(default)]
    model_version: Option<String>,
    #[serde(default)]
    error: Option<VertexStreamError>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VertexCandidate {
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    content: Option<VertexContent>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct VertexContent {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    parts: Vec<VertexPart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VertexPart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    #[serde(default)]
    function_call: Option<Value>,
    #[serde(default)]
    executable_code: Option<Value>,
    #[serde(default)]
    code_execution_result: Option<Value>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VertexUsageMetadata {
    #[serde(default)]
    prompt_token_count: Option<i64>,
    #[serde(default)]
    candidates_token_count: Option<i64>,
    #[serde(default)]
    cached_content_token_count: Option<i64>,
    #[serde(default)]
    thoughts_token_count: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct VertexPromptFeedback {
    #[serde(default)]
    block_reason: Option<String>,
}

#[derive(Deserialize)]
struct VertexStreamError {
    #[serde(default)]
    code: Option<u16>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    details: Vec<Value>,
}

#[derive(Deserialize)]
struct GoogleErrorEnvelope {
    error: VertexStreamError,
}

fn vertex_error_code(status: u16, bytes: &[u8]) -> Option<ErrorCode> {
    let envelope: GoogleErrorEnvelope = serde_json::from_slice(bytes).ok()?;
    Some(vertex_stream_error_code_with_http(&envelope.error, status))
}

fn vertex_stream_error_code(error: &VertexStreamError) -> ErrorCode {
    vertex_stream_error_code_with_http(error, error.code.unwrap_or(500))
}

fn vertex_stream_error_code_with_http(error: &VertexStreamError, http_status: u16) -> ErrorCode {
    match error.status.as_deref() {
        Some("UNAUTHENTICATED") => ErrorCode::AuthRejected,
        Some("PERMISSION_DENIED") => ErrorCode::Permission,
        Some("NOT_FOUND") => ErrorCode::ModelUnavailable,
        Some("DEADLINE_EXCEEDED") => ErrorCode::Timeout,
        Some("RESOURCE_EXHAUSTED") => {
            if error.details.iter().any(detail_has_quota_reason) {
                ErrorCode::Quota
            } else {
                ErrorCode::RateLimited
            }
        }
        Some("CANCELLED") => ErrorCode::Canceled,
        Some("INVALID_ARGUMENT" | "FAILED_PRECONDITION" | "INTERNAL" | "UNAVAILABLE")
        | Some(_)
        | None => http_status_code(http_status),
    }
}

fn detail_has_quota_reason(value: &Value) -> bool {
    match value {
        Value::String(value) => matches!(
            value.as_str(),
            "QUOTA_EXCEEDED" | "QUOTA_LIMIT" | "BILLING_NOT_ENABLED"
        ),
        Value::Array(values) => values.iter().any(detail_has_quota_reason),
        Value::Object(values) => values.values().any(detail_has_quota_reason),
        _ => false,
    }
}

fn resource_tail(value: &str) -> &str {
    value.rsplit('/').next().unwrap_or(value)
}

fn validate_project_id(value: &str) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b':'))
        || !value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !value
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(());
    }
    Ok(())
}

fn validate_region(value: &str) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > 63
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !value
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Err(());
    }
    Ok(())
}

fn validate_model_id(value: &str) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@'))
    {
        return Err(());
    }
    Ok(())
}
