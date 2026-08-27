//! Injected provider and credential edges for hosted transcript post-processing.
//!
//! Documented OpenAI, native ChatGPT subscription, Anthropic, Vertex, and Bedrock adapters run only through
//! injected HTTP, clock, credential, and storage seams. The crate provides no ambient credential discovery
//! and launches no model server. Claude subscription routing is descriptor-only and blocked. Provider bodies
//! and credentials are excluded from public error and debug representations.

#![forbid(unsafe_code)]

mod anthropic;
mod anthropic_wire;
mod bedrock;
mod blocked;
mod chatgpt;
mod common;
mod eventstream;
mod openai;
mod schema;
mod sigv4;
mod sse;
mod transport;
mod vertex;
mod vertex_auth;

pub use anthropic::{
    ANTHROPIC_API_VERSION, ANTHROPIC_MESSAGES_ADAPTER_VERSION, AnthropicMessagesAdapter,
};
pub use bedrock::{
    BEDROCK_CONSERVATIVE_MAX_CONTEXT_TOKENS, BEDROCK_CONSERVATIVE_MAX_OUTPUT_TOKENS,
    BEDROCK_CONVERSE_ADAPTER_VERSION, BEDROCK_EVENT_STREAM_CONTENT_TYPE, BedrockConverseAdapter,
};
pub use blocked::{ClaudeSubscriptionDescriptor, claude_subscription_descriptor};
pub use chatgpt::{
    CHATGPT_CONSERVATIVE_MAX_OUTPUT_TOKENS, CHATGPT_DEVICE_VERIFICATION_URL,
    CHATGPT_FALLBACK_CONTEXT_TOKENS, CHATGPT_SUBSCRIPTION_ADAPTER_VERSION, ChatGptAuthError,
    ChatGptClock, ChatGptCredentialStore, ChatGptDeviceAuthorization, ChatGptLoginPoll,
    ChatGptStoreError, ChatGptSubscriptionAdapter, ChatGptSubscriptionAuth,
};
pub use common::{CacheKeyError, DirectAdapterOptions, ProviderCacheKeySource};
pub use openai::{
    OPENAI_LUNA_MAX_CONTEXT_TOKENS, OPENAI_LUNA_MAX_OUTPUT_TOKENS, OPENAI_LUNA_MODEL_ID,
    OPENAI_RESPONSES_ADAPTER_VERSION, OpenAiResponsesAdapter,
};
pub use sigv4::{AwsCredentialSource, AwsCredentials, AwsCredentialsError};
pub use transport::{
    AccessToken, AccessTokenError, ApiKey, ApiKeyError, ApiKeySource, Clock, CredentialError,
    HttpBuildError, HttpHeader, HttpHeaderValue, HttpMethod, HttpRequest, HttpResponse,
    HttpResponseBody, HttpTransport, RequestDelivery, SecretString, SystemClock, TransportError,
    TransportErrorKind, UreqTransport, WallClock,
};
pub use vertex::{
    AdcAccessToken, AdcAccessTokenSource, VERTEX_REST_ADAPTER_VERSION, VertexConfigurationError,
    VertexModel, VertexProjectMetadata, VertexPublisher, VertexRestAdapter,
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

/// Claude Free/Pro/Max routing has a blocked descriptor but no credential, connect, or execute adapter
/// without written Anthropic permission.
pub const CLAUDE_SUBSCRIPTION_ADAPTER_BLOCKED: bool = true;

#[cfg(test)]
mod tests;
