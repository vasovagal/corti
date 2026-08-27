//! The Anthropic Messages request body and streaming codec, shared by the direct API adapter and the
//! Anthropic publisher on Vertex. The two differ only in how the model and version are addressed: direct
//! names the model in the body, Vertex names it in the URL path and pins `anthropic_version` instead.

use corti_postprocess::{
    ErrorCode, HostedRequest, NormalizedUsage, PostprocessError, ProviderCacheMode,
    ProviderEventKind, ProviderEventSink, RawUsage,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    common::{TextCollector, emit, json_bytes},
    schema::output_schema,
    sse::SseEvent,
    transport::Clock,
};

pub(crate) const ANTHROPIC_VERTEX_API_VERSION: &str = "vertex-2023-10-16";

/// Which endpoint the body is addressed to.
pub(crate) enum AnthropicWire<'a> {
    Direct { region: Option<&'a str> },
    Vertex,
}

/// A stream failure in the codec's own terms. Each adapter module owns a private `ExecFailure` with these
/// same three fields and converts on the way out.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WireFailure {
    pub code: ErrorCode,
    pub usage: Option<NormalizedUsage>,
    pub dispatched: bool,
}

impl WireFailure {
    const fn new(code: ErrorCode, dispatched: bool) -> Self {
        Self {
            code,
            usage: None,
            dispatched,
        }
    }
}

pub(crate) fn request_body(
    request: &HostedRequest,
    max_output_tokens: u64,
    wire: AnthropicWire<'_>,
) -> Result<Vec<u8>, PostprocessError> {
    let cache_enabled = request.cache_policy.provider == ProviderCacheMode::ExplicitStablePrefix;
    let prompt = request.prompt.messages();
    let system = prompt[..2]
        .iter()
        .map(|message| json!({"type": "text", "text": message.content()}))
        .collect::<Vec<_>>();
    let dynamic = prompt[2..]
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let mut block = json!({"type": "text", "text": message.content()});
            if cache_enabled && index == 0 {
                block["cache_control"] = json!({"type": "ephemeral", "ttl": "5m"});
            }
            block
        })
        .collect::<Vec<_>>();
    let mut body = json!({
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
    match wire {
        AnthropicWire::Direct { region } => {
            body["model"] = Value::String(request.model.as_str().to_owned());
            if let Some(region) = region {
                body["inference_geo"] = Value::String(region.to_owned());
            }
        }
        AnthropicWire::Vertex => {
            body["anthropic_version"] = Value::String(ANTHROPIC_VERTEX_API_VERSION.to_owned());
        }
    }
    json_bytes(&body)
}

pub(crate) struct AnthropicStreamState {
    pub text: TextCollector,
    expect_model: Option<String>,
    started: bool,
    stopped: bool,
    text_block_seen: bool,
    open_text_index: Option<u64>,
    stop_reason: Option<String>,
    usage: AnthropicUsageValues,
    pub terminal_usage: Option<NormalizedUsage>,
    pub terminal_at: Option<u64>,
}

impl AnthropicStreamState {
    /// `expect_model` rejects a response whose `message_start` names a different model. Vertex passes `None`:
    /// the model is in the request path, and Vertex echoes the id it resolved to rather than the one asked
    /// for (`claude-sonnet-4-5@20250929` comes back as `claude-sonnet-4-5-20250929`).
    pub(crate) fn new(max_bytes: usize, expect_model: Option<&str>) -> Self {
        Self {
            text: TextCollector::new(max_bytes),
            expect_model: expect_model.map(ToOwned::to_owned),
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

    pub(crate) fn process(
        &mut self,
        event: SseEvent,
        request: &HostedRequest,
        sink: &dyn ProviderEventSink,
        clock: &dyn Clock,
    ) -> Result<(), WireFailure> {
        let envelope: AnthropicEnvelope = serde_json::from_str(&event.data)
            .map_err(|_| WireFailure::new(ErrorCode::MalformedOutput, true))?;
        if event
            .event
            .as_deref()
            .is_some_and(|name| name != envelope.kind)
        {
            return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
        }
        if self.stopped && envelope.kind != "ping" {
            return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
        }
        match envelope.kind.as_str() {
            "message_start" => {
                if self.started {
                    return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
                }
                let payload: AnthropicMessageStart = serde_json::from_str(&event.data)
                    .map_err(|_| WireFailure::new(ErrorCode::MalformedOutput, true))?;
                if self
                    .expect_model
                    .as_ref()
                    .is_some_and(|expected| &payload.message.model != expected)
                {
                    return Err(WireFailure::new(ErrorCode::ModelUnavailable, true));
                }
                self.usage
                    .reconcile(payload.message.usage)
                    .map_err(|code| WireFailure::new(code, true))?;
                self.started = true;
                emit(
                    sink,
                    request,
                    ProviderEventKind::UsageProvisional(
                        self.usage
                            .normalized(false)
                            .map_err(|code| WireFailure::new(code, true))?,
                    ),
                );
            }
            "content_block_start" => {
                if !self.started || self.open_text_index.is_some() || self.text_block_seen {
                    return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
                }
                let payload: AnthropicBlockStart = serde_json::from_str(&event.data)
                    .map_err(|_| WireFailure::new(ErrorCode::MalformedOutput, true))?;
                if payload.content_block.kind != "text" {
                    return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
                }
                self.open_text_index = Some(payload.index);
                self.text_block_seen = true;
                self.text
                    .push(&payload.content_block.text, request, sink, clock)
                    .map_err(|error| WireFailure::new(error.code, true))?;
            }
            "content_block_delta" => {
                let payload: AnthropicBlockDelta = serde_json::from_str(&event.data)
                    .map_err(|_| WireFailure::new(ErrorCode::MalformedOutput, true))?;
                if self.open_text_index != Some(payload.index) || payload.delta.kind != "text_delta"
                {
                    return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
                }
                self.text
                    .push(&payload.delta.text, request, sink, clock)
                    .map_err(|error| WireFailure::new(error.code, true))?;
            }
            "content_block_stop" => {
                let payload: AnthropicBlockStop = serde_json::from_str(&event.data)
                    .map_err(|_| WireFailure::new(ErrorCode::MalformedOutput, true))?;
                if self.open_text_index != Some(payload.index) {
                    return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
                }
                self.open_text_index = None;
            }
            "message_delta" => {
                if !self.started || self.open_text_index.is_some() {
                    return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
                }
                let payload: AnthropicMessageDelta = serde_json::from_str(&event.data)
                    .map_err(|_| WireFailure::new(ErrorCode::MalformedOutput, true))?;
                if let Some(reason) = payload.delta.stop_reason {
                    if self
                        .stop_reason
                        .as_ref()
                        .is_some_and(|existing| existing != &reason)
                    {
                        return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
                    }
                    self.stop_reason = Some(reason);
                }
                self.usage
                    .reconcile(payload.usage)
                    .map_err(|code| WireFailure::new(code, true))?;
                emit(
                    sink,
                    request,
                    ProviderEventKind::UsageProvisional(
                        self.usage
                            .normalized(false)
                            .map_err(|code| WireFailure::new(code, true))?,
                    ),
                );
            }
            "message_stop" => {
                if !self.started
                    || self.stopped
                    || self.open_text_index.is_some()
                    || !self.text_block_seen
                {
                    return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
                }
                match self.stop_reason.as_deref() {
                    Some("end_turn") => {}
                    Some("max_tokens" | "model_context_window_exceeded") | None => {
                        return Err(WireFailure::new(ErrorCode::MalformedOutput, true));
                    }
                    Some(_) => return Err(WireFailure::new(ErrorCode::Provider, true)),
                }
                let usage = self
                    .usage
                    .normalized(true)
                    .map_err(|code| WireFailure::new(code, true))?;
                self.terminal_usage = Some(usage);
                self.terminal_at = Some(clock.monotonic_micros());
                self.stopped = true;
            }
            "error" => {
                let payload: AnthropicErrorEvent = serde_json::from_str(&event.data)
                    .map_err(|_| WireFailure::new(ErrorCode::Provider, true))?;
                return Err(WireFailure {
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

    pub(crate) fn finish(&self) -> Result<(), WireFailure> {
        if !self.stopped || self.text.text().is_empty() {
            return Err(WireFailure {
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
