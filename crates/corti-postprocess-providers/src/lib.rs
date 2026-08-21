//! Documented direct-provider adapters for hosted transcript post-processing.
//!
//! OpenAI Responses and Anthropic Messages run only through injected HTTP, clock, and API-key seams. The
//! crate provides no ambient credential discovery. Provider bodies and credentials are excluded from all
//! public error and debug representations.

#![forbid(unsafe_code)]

mod anthropic;
mod common;
mod openai;
mod schema;
mod sse;
mod transport;

pub use anthropic::{
    ANTHROPIC_API_VERSION, ANTHROPIC_MESSAGES_ADAPTER_VERSION, AnthropicMessagesAdapter,
};
pub use common::{CacheKeyError, DirectAdapterOptions, ProviderCacheKeySource};
pub use openai::{
    OPENAI_LUNA_MAX_CONTEXT_TOKENS, OPENAI_LUNA_MAX_OUTPUT_TOKENS, OPENAI_LUNA_MODEL_ID,
    OPENAI_RESPONSES_ADAPTER_VERSION, OpenAiResponsesAdapter,
};
pub use transport::{
    ApiKey, ApiKeyError, ApiKeySource, Clock, CredentialError, HttpBuildError, HttpHeader,
    HttpHeaderValue, HttpMethod, HttpRequest, HttpResponse, HttpResponseBody, HttpTransport,
    RequestDelivery, SecretString, SystemClock, TransportError, TransportErrorKind, UreqTransport,
};

pub use corti_postprocess::{
    CancellationToken, CredentialSource, HostedRequest, ModelCatalog, PostprocessError,
    ProviderAdapter, ProviderDescriptor, ProviderEvent, ProviderEventSink, ProviderScope,
    ProviderTerminal,
};

/// Codex app-server support is experimental and remains off in default builds. No Codex adapter is present
/// in this crate slice.
pub const CODEX_APP_SERVER_EXPERIMENTAL: bool = true;
pub const CODEX_APP_SERVER_DEFAULT_ENABLED: bool = false;
pub const CODEX_APP_SERVER_COMPILED: bool = cfg!(feature = "codex-experimental");

/// Claude Free/Pro/Max routing has no adapter without written Anthropic permission.
pub const CLAUDE_SUBSCRIPTION_ADAPTER_BLOCKED: bool = true;

#[cfg(test)]
mod tests;
