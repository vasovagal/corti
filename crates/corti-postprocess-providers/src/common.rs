use std::{fmt, time::Duration};

use corti_postprocess::{
    CacheObservation, CancellationToken, ErrorCode, EventContext, HostedRequest, LatencyFields,
    MonotonicDeadline, NormalizedUsage, PostprocessError, PromptRole, PromptSection, PromptTask,
    ProviderCacheKey, ProviderEvent, ProviderEventKind, ProviderEventSink, ProviderOutput,
    QuestionOutput, QuestionTerminal, RewriteOutput, TextDelta,
};
use serde_json::Value;
use thiserror::Error;

use crate::transport::{
    Clock, CredentialError, HttpResponse, HttpTransport, RequestDelivery, TransportError,
    TransportErrorKind,
};

pub(crate) const MAX_CATALOG_BODY_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONFIGURED_STREAM_BYTES: usize = 64 * 1024 * 1024;
const MAX_EVENT_TEXT_DELTA_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirectAdapterOptions {
    /// Per-request output cap sent to the provider, further limited by the selected catalog descriptor.
    pub max_output_tokens: u64,
    /// Maximum SSE bytes accepted for one response before failing closed.
    pub max_stream_bytes: usize,
}

impl Default for DirectAdapterOptions {
    fn default() -> Self {
        Self {
            max_output_tokens: 8 * 1024,
            max_stream_bytes: DEFAULT_MAX_STREAM_BYTES,
        }
    }
}

impl DirectAdapterOptions {
    pub(crate) fn validate(self) -> Result<Self, PostprocessError> {
        if self.max_output_tokens == 0
            || self.max_stream_bytes == 0
            || self.max_stream_bytes > MAX_CONFIGURED_STREAM_BYTES
        {
            return Err(ErrorCode::PolicyBlocked.into());
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CacheKeyError {
    #[error("provider cache key is unavailable")]
    Unavailable,
}

/// Injected source for OpenAI's opaque `prompt_cache_key`. Implementations should derive it with the
/// domain crate's HMAC-backed `ProviderCacheKey` contract; readable prompt/account values must not be used.
pub trait ProviderCacheKeySource: Send {
    fn key_for(&mut self, request: &HostedRequest) -> Result<ProviderCacheKey, CacheKeyError>;
}

pub(crate) fn context(request: &HostedRequest) -> EventContext {
    EventContext {
        call_id: request.call_id.clone(),
        group_id: request.group_id.clone(),
        target_id: request.target_id.clone(),
        lane: request.lane,
        fence: request.fence.clone(),
    }
}

pub(crate) fn emit(sink: &dyn ProviderEventSink, request: &HostedRequest, kind: ProviderEventKind) {
    sink.emit(ProviderEvent {
        context: context(request),
        kind,
    });
}

pub(crate) struct DiscardEventSink;

impl ProviderEventSink for DiscardEventSink {
    fn emit(&self, _event: ProviderEvent) {}
}

pub(crate) static DISCARD_EVENT_SINK: DiscardEventSink = DiscardEventSink;

pub(crate) fn credential_code(error: CredentialError) -> ErrorCode {
    match error {
        CredentialError::Absent | CredentialError::Unavailable => ErrorCode::AuthUnarmed,
        CredentialError::Rejected => ErrorCode::AuthRejected,
    }
}

pub(crate) fn validate_prompt_layout(request: &HostedRequest) -> Result<(), PostprocessError> {
    let messages = request.prompt.messages();
    if messages.len() != 6
        || messages[0].role() != PromptRole::Developer
        || messages[0].section() != PromptSection::ImmutablePolicy
        || messages[1].role() != PromptRole::Developer
        || messages[1].section() != PromptSection::OutputSchema
        || messages[2].role() != PromptRole::Developer
        || messages[2].section() != PromptSection::WordBank
        || messages[3].role() != PromptRole::User
        || messages[3].section() != PromptSection::Steering
        || messages[4].role() != PromptRole::User
        || messages[4].section() != PromptSection::ContextRows
        || messages[5].role() != PromptRole::User
        || !matches!(
            (request.prompt.task(), messages[5].section()),
            (PromptTask::Rewrite, PromptSection::TargetRows)
                | (PromptTask::Question, PromptSection::Question)
        )
        || request.prompt.stable_prefix_len() == 0
    {
        return Err(ErrorCode::Internal.into());
    }
    if request.prompt.task() == PromptTask::Question && !request.lane.is_question() {
        return Err(ErrorCode::PolicyBlocked.into());
    }
    if request.prompt.task() == PromptTask::Rewrite && request.lane.is_question() {
        return Err(ErrorCode::PolicyBlocked.into());
    }
    Ok(())
}

pub(crate) fn parse_output(
    task: PromptTask,
    text: &str,
) -> Result<ProviderOutput, PostprocessError> {
    match task {
        PromptTask::Rewrite => serde_json::from_str::<RewriteOutput>(text)
            .map(ProviderOutput::Rewrite)
            .map_err(|_| ErrorCode::MalformedOutput.into()),
        PromptTask::Question => serde_json::from_str::<QuestionOutput>(text)
            .map(|output| ProviderOutput::Question(QuestionTerminal { output }))
            .map_err(|_| ErrorCode::MalformedOutput.into()),
    }
}

pub(crate) fn http_status_code(status: u16) -> ErrorCode {
    match status {
        401 => ErrorCode::AuthRejected,
        403 => ErrorCode::Permission,
        404 => ErrorCode::ModelUnavailable,
        408 | 504 => ErrorCode::Timeout,
        409 | 429 => ErrorCode::RateLimited,
        402 => ErrorCode::Quota,
        _ => ErrorCode::Provider,
    }
}

pub(crate) fn transport_code(error: TransportError, cancel: &CancellationToken) -> ErrorCode {
    match error.kind {
        TransportErrorKind::Canceled => cancel
            .reason()
            .map_or(ErrorCode::Canceled, |reason| reason.error_code()),
        TransportErrorKind::Timeout => ErrorCode::Timeout,
        TransportErrorKind::Network | TransportErrorKind::Protocol => ErrorCode::Network,
    }
}

pub(crate) fn boundary_code(
    cancel: &CancellationToken,
    deadline: MonotonicDeadline,
    now_micros: u64,
) -> Option<ErrorCode> {
    cancel
        .reason()
        .map(|reason| reason.error_code())
        .or_else(|| {
            deadline
                .is_expired_at(now_micros)
                .then_some(ErrorCode::Timeout)
        })
}

pub(crate) fn request_timeout(
    deadline: MonotonicDeadline,
    now_micros: u64,
) -> Result<Duration, PostprocessError> {
    let remaining = deadline.remaining_at(now_micros);
    if remaining == 0 {
        return Err(ErrorCode::Timeout.into());
    }
    Ok(Duration::from_micros(remaining))
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ExchangeTimes {
    pub dispatch_at: u64,
    pub headers_at: u64,
}

pub(crate) struct Exchange {
    pub response: HttpResponse,
    pub times: ExchangeTimes,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SendFailure {
    pub code: ErrorCode,
    pub dispatched: bool,
}

/// Send with at most one retry, and only when the transport proves zero request-body bytes were sent.
pub(crate) fn send_with_retry(
    transport: &mut dyn HttpTransport,
    clock: &dyn Clock,
    wire_request: &crate::transport::HttpRequest,
    cancel: &CancellationToken,
    deadline: MonotonicDeadline,
    event_target: Option<(&HostedRequest, &dyn ProviderEventSink)>,
) -> Result<Exchange, SendFailure> {
    let mut retried = false;
    let mut dispatch_event_emitted = false;
    loop {
        let now = clock.monotonic_micros();
        if let Some(code) = boundary_code(cancel, deadline, now) {
            return Err(SendFailure {
                code,
                dispatched: dispatch_event_emitted,
            });
        }
        let dispatch_at = now;
        match transport.send(wire_request, cancel) {
            Ok(response) => {
                if !dispatch_event_emitted && let Some((request, sink)) = event_target {
                    emit(sink, request, ProviderEventKind::DispatchStarted);
                }
                let headers_at = clock.monotonic_micros();
                if let Some((request, sink)) = event_target {
                    emit(sink, request, ProviderEventKind::Headers);
                }
                return Ok(Exchange {
                    response,
                    times: ExchangeTimes {
                        dispatch_at,
                        headers_at,
                    },
                });
            }
            Err(error) => {
                if error.delivery != RequestDelivery::NotSent && !dispatch_event_emitted {
                    if let Some((request, sink)) = event_target {
                        emit(sink, request, ProviderEventKind::DispatchStarted);
                    }
                    dispatch_event_emitted = true;
                }
                let retryable = !retried
                    && error.delivery == RequestDelivery::NotSent
                    && matches!(
                        error.kind,
                        TransportErrorKind::Network | TransportErrorKind::Timeout
                    )
                    && boundary_code(cancel, deadline, clock.monotonic_micros()).is_none();
                if retryable {
                    retried = true;
                    continue;
                }
                return Err(SendFailure {
                    code: transport_code(error, cancel),
                    dispatched: dispatch_event_emitted,
                });
            }
        }
    }
}

pub(crate) fn validate_event_stream_response(
    response: &HttpResponse,
) -> Result<(), PostprocessError> {
    let content_type = response
        .header("content-type")
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if content_type != Some("text/event-stream") {
        return Err(ErrorCode::MalformedOutput.into());
    }
    Ok(())
}

pub(crate) fn read_body_limited(
    response: &mut HttpResponse,
    limit: usize,
    cancel: &CancellationToken,
    deadline: MonotonicDeadline,
    clock: &dyn Clock,
) -> Result<Vec<u8>, PostprocessError> {
    let mut bytes = Vec::new();
    loop {
        if let Some(code) = boundary_code(cancel, deadline, clock.monotonic_micros()) {
            response.body_mut().cancel();
            return Err(code.into());
        }
        let chunk = response
            .body_mut()
            .next_chunk()
            .map_err(|error| PostprocessError::from(transport_code(error, cancel)))?;
        let Some(chunk) = chunk else {
            return Ok(bytes);
        };
        let new_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| PostprocessError::from(ErrorCode::MalformedOutput))?;
        if new_len > limit {
            return Err(ErrorCode::MalformedOutput.into());
        }
        bytes.extend_from_slice(&chunk);
    }
}

pub(crate) struct TextCollector {
    text: String,
    max_bytes: usize,
    first_text_at: Option<u64>,
}

impl TextCollector {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            text: String::new(),
            max_bytes,
            first_text_at: None,
        }
    }

    pub fn push(
        &mut self,
        delta: &str,
        request: &HostedRequest,
        sink: &dyn ProviderEventSink,
        clock: &dyn Clock,
    ) -> Result<(), PostprocessError> {
        if delta.is_empty() {
            return Ok(());
        }
        let new_len = self
            .text
            .len()
            .checked_add(delta.len())
            .ok_or_else(|| PostprocessError::from(ErrorCode::MalformedOutput))?;
        if new_len > self.max_bytes {
            return Err(ErrorCode::MalformedOutput.into());
        }
        self.text.push_str(delta);
        if self.first_text_at.is_none() {
            self.first_text_at = Some(clock.monotonic_micros());
            emit(sink, request, ProviderEventKind::FirstText);
        }
        for bounded in split_delta(delta) {
            emit(
                sink,
                request,
                ProviderEventKind::TextDelta(TextDelta::new(bounded)?),
            );
        }
        Ok(())
    }

    pub fn verify_done_text(&self, done: &str) -> Result<(), PostprocessError> {
        if self.text != done {
            return Err(ErrorCode::MalformedOutput.into());
        }
        Ok(())
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub const fn first_text_at(&self) -> Option<u64> {
        self.first_text_at
    }
}

impl fmt::Debug for TextCollector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextCollector")
            .field("text_bytes", &self.text.len())
            .field("max_bytes", &self.max_bytes)
            .field("first_text_at", &self.first_text_at)
            .finish()
    }
}

fn split_delta(mut delta: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    while delta.len() > MAX_EVENT_TEXT_DELTA_BYTES {
        let mut boundary = MAX_EVENT_TEXT_DELTA_BYTES;
        while !delta.is_char_boundary(boundary) {
            boundary -= 1;
        }
        chunks.push(delta[..boundary].to_owned());
        delta = &delta[boundary..];
    }
    if !delta.is_empty() {
        chunks.push(delta.to_owned());
    }
    chunks
}

pub(crate) fn usage_cache_observations(usage: NormalizedUsage) -> Vec<CacheObservation> {
    let mut observations = Vec::new();
    if usage.cached_read_tokens.is_some_and(|tokens| tokens > 0) {
        observations.push(CacheObservation::ProviderRead);
    }
    if usage.cached_write_tokens.is_some_and(|tokens| tokens > 0) {
        observations.push(CacheObservation::ProviderWrite);
    }
    observations
}

pub(crate) fn terminal_cache_observation(usage: NormalizedUsage) -> CacheObservation {
    if usage.cached_read_tokens.is_some_and(|tokens| tokens > 0) {
        CacheObservation::ProviderRead
    } else if usage.cached_write_tokens.is_some_and(|tokens| tokens > 0) {
        CacheObservation::ProviderWrite
    } else {
        CacheObservation::None
    }
}

pub(crate) struct Timing {
    pub total_start: u64,
    pub auth_us: Option<u64>,
    pub exchange: Option<ExchangeTimes>,
    pub first_text_at: Option<u64>,
    pub terminal_at: Option<u64>,
    pub parse_us: Option<u64>,
}

impl Timing {
    pub fn new(total_start: u64) -> Self {
        Self {
            total_start,
            auth_us: None,
            exchange: None,
            first_text_at: None,
            terminal_at: None,
            parse_us: None,
        }
    }

    pub fn latency(&self, completed_at: u64) -> LatencyFields {
        let (connect_us, ttfb_us, ttft_us) = self.exchange.map_or((None, None, None), |exchange| {
            (
                Some(exchange.headers_at.saturating_sub(exchange.dispatch_at)),
                Some(exchange.headers_at.saturating_sub(exchange.dispatch_at)),
                self.first_text_at
                    .map(|first| first.saturating_sub(exchange.dispatch_at)),
            )
        });
        LatencyFields {
            queue_us: None,
            auth_us: self.auth_us,
            cache_lookup_us: None,
            connect_us,
            ttfb_us,
            ttft_us,
            stream_us: self
                .first_text_at
                .zip(self.terminal_at)
                .map(|(first, terminal)| terminal.saturating_sub(first)),
            parse_us: self.parse_us,
            cache_commit_us: None,
            total_us: Some(completed_at.saturating_sub(self.total_start)),
        }
    }
}

pub(crate) fn json_bytes(value: &Value) -> Result<Vec<u8>, PostprocessError> {
    serde_json::to_vec(value).map_err(|_| ErrorCode::Internal.into())
}
