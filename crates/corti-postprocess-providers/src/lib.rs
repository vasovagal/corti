//! Injected provider and credential edges for hosted transcript post-processing.
//!
//! Documented OpenAI, Anthropic, and Vertex adapters run only through injected HTTP, clock, and credential
//! seams. The crate provides no ambient credential discovery. Experimental Codex broker authentication is
//! compile- and approval-gated; Claude subscription routing is descriptor-only and blocked. Provider bodies
//! and credentials are excluded from public error and debug representations.

#![forbid(unsafe_code)]

mod anthropic;
mod blocked;
#[cfg(feature = "codex-experimental")]
mod codex;
mod common;
mod openai;
mod schema;
mod sse;
mod transport;
mod vertex;
mod vertex_auth;

pub use anthropic::{
    ANTHROPIC_API_VERSION, ANTHROPIC_MESSAGES_ADAPTER_VERSION, AnthropicMessagesAdapter,
};
pub use blocked::{ClaudeSubscriptionDescriptor, claude_subscription_descriptor};
#[cfg(feature = "codex-experimental")]
pub use codex::{
    CodexAppServerBroker, CodexAppServerGate, CodexAuthorizationError, CodexBrokerError,
    CodexBrokerPosture, CodexDeviceAuthorization, CodexDeviceCodeError, CodexDeviceCodeMachine,
    CodexDeviceCodeState, CodexLoginPoll,
};
pub use common::{CacheKeyError, DirectAdapterOptions, ProviderCacheKeySource};
pub use openai::{
    OPENAI_LUNA_MAX_CONTEXT_TOKENS, OPENAI_LUNA_MAX_OUTPUT_TOKENS, OPENAI_LUNA_MODEL_ID,
    OPENAI_RESPONSES_ADAPTER_VERSION, OpenAiResponsesAdapter,
};
pub use transport::{
    AccessToken, AccessTokenError, ApiKey, ApiKeyError, ApiKeySource, Clock, CredentialError,
    HttpBuildError, HttpHeader, HttpHeaderValue, HttpMethod, HttpRequest, HttpResponse,
    HttpResponseBody, HttpTransport, RequestDelivery, SecretString, SystemClock, TransportError,
    TransportErrorKind, UreqTransport,
};
pub use vertex::{
    AdcAccessToken, AdcAccessTokenSource, VERTEX_PUBLISHER, VERTEX_REST_ADAPTER_VERSION,
    VertexConfigurationError, VertexModel, VertexProjectMetadata, VertexRestAdapter,
};
pub use vertex_auth::{
    VERTEX_CREDENTIAL_POLL_INTERVAL_MICROS, VertexAutoPending, VertexCredentialResolver,
    VertexCredentialState, VertexDispatchDisposition, VertexDispatchIntent, VertexPendingError,
    VertexPendingRetention, VertexResolutionAttempt, VertexResolutionKind, VertexResolutionOutcome,
    VertexResolutionUpdate, VertexResolverError, VertexUnarmedNotice,
};

pub use corti_postprocess::{
    CancellationToken, CredentialSource, HostedRequest, ModelCatalog, PostprocessError,
    ProviderAdapter, ProviderDescriptor, ProviderEvent, ProviderEventSink, ProviderScope,
    ProviderTerminal, VERTEX_UNARMED_WARNING,
};

/// Codex app-server support is experimental and remains off in default builds. Even feature-enabled builds
/// require an explicit product-approval gate before the broker can be touched.
pub const CODEX_APP_SERVER_EXPERIMENTAL: bool = true;
pub const CODEX_APP_SERVER_DEFAULT_ENABLED: bool = false;
pub const CODEX_APP_SERVER_COMPILED: bool = cfg!(feature = "codex-experimental");

/// Claude Free/Pro/Max routing has a blocked descriptor but no credential, connect, or execute adapter
/// without written Anthropic permission.
pub const CLAUDE_SUBSCRIPTION_ADAPTER_BLOCKED: bool = true;

#[cfg(test)]
mod tests;
