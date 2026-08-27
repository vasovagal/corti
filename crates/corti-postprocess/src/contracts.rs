use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    CallId, ConnectionScopeId, ModelId, ProcessEpoch, ProviderId, RequestGroupId, RowId, TargetId,
    TransportId,
    pricing::NormalizedUsage,
    prompt::CanonicalPrompt,
    validation::{QuestionOutput, RewriteOutput},
};

/// The scheduling lane for a hosted operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Live,
    Final,
    AdHocQuestion,
    PinnedQuestion,
}

impl Lane {
    pub const fn is_question(self) -> bool {
        matches!(self, Self::AdHocQuestion | Self::PinnedQuestion)
    }

    pub const fn is_automatic(self) -> bool {
        matches!(self, Self::Live | Self::Final | Self::PinnedQuestion)
    }
}

/// Product/release support status. This value is supplied by Rust, never inferred by a UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportTier {
    Documented,
    Experimental,
    Blocked,
}

/// Truthful provider billing classification for one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingBasis {
    MeteredEstimate,
    IncludedSubscription,
    NoProviderRequest,
    Unknown,
}

/// Known provider/transport combinations and their non-negotiable support posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownTransport {
    VertexDirect,
    OpenAiDirect,
    ChatGptSubscription,
    AnthropicDirect,
    ClaudeSubscription,
    BedrockRuntime,
}

impl KnownTransport {
    pub const fn support_tier(self) -> SupportTier {
        match self {
            Self::VertexDirect
            | Self::OpenAiDirect
            | Self::AnthropicDirect
            | Self::BedrockRuntime => SupportTier::Documented,
            Self::ChatGptSubscription => SupportTier::Experimental,
            Self::ClaudeSubscription => SupportTier::Blocked,
        }
    }

    pub const fn billing_basis(self) -> BillingBasis {
        match self {
            Self::VertexDirect
            | Self::OpenAiDirect
            | Self::AnthropicDirect
            | Self::BedrockRuntime => BillingBasis::MeteredEstimate,
            Self::ChatGptSubscription => BillingBasis::IncludedSubscription,
            Self::ClaudeSubscription => BillingBasis::Unknown,
        }
    }

    /// Whether an adapter may exist in an ordinary production build.
    pub const fn production_adapter_allowed(self) -> bool {
        matches!(
            self,
            Self::VertexDirect
                | Self::OpenAiDirect
                | Self::ChatGptSubscription
                | Self::AnthropicDirect
                | Self::BedrockRuntime
        )
    }

    pub fn descriptor(self) -> ProviderDescriptor {
        let (provider, transport) = match self {
            Self::VertexDirect => ("google", "vertex_api"),
            Self::OpenAiDirect => ("openai", "openai_api"),
            Self::ChatGptSubscription => ("openai", "chatgpt_subscription"),
            Self::AnthropicDirect => ("anthropic", "anthropic_api"),
            Self::ClaudeSubscription => ("anthropic", "claude_subscription"),
            Self::BedrockRuntime => ("amazon", "bedrock_runtime"),
        };
        ProviderDescriptor {
            provider: ProviderId::new(provider).expect("known provider id is valid"),
            transport: TransportId::new(transport).expect("known transport id is valid"),
            support_tier: self.support_tier(),
            billing_basis: self.billing_basis(),
            adapter_available: self.production_adapter_allowed(),
        }
    }
}

/// Every revision that can make a provider result stale.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestFence {
    pub process_epoch: ProcessEpoch,
    pub session_generation: u64,
    pub transcript_revision: u64,
    pub control_revision: u64,
    pub lane_revision: u64,
    pub steering_revision: u64,
    pub bank_revision: u64,
    pub question_revision: Option<u64>,
}

impl RequestFence {
    /// Results apply only under exact fence equality. There is deliberately no partial/"newer" match.
    pub fn is_current(&self, current: &Self) -> bool {
        self == current
    }
}

/// Monotonic deadline in microseconds in the injected clock's epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MonotonicDeadline(pub u64);

impl MonotonicDeadline {
    pub const fn is_expired_at(self, now_micros: u64) -> bool {
        now_micros >= self.0
    }

    pub const fn remaining_at(self, now_micros: u64) -> u64 {
        self.0.saturating_sub(now_micros)
    }
}

/// One immutable transcript row supplied as context or as a rewrite target.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptRow {
    pub row_id: RowId,
    pub speaker: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

impl fmt::Debug for TranscriptRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranscriptRow")
            .field("row_id", &self.row_id)
            .field("start_ms", &self.start_ms)
            .field("end_ms", &self.end_ms)
            .field("speaker", &"<redacted>")
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

pub type RewriteTarget = TranscriptRow;
pub type ContextRow = TranscriptRow;

/// Reusable local cache behavior. Recovery commits for validated final output remain mandatory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalCacheMode {
    Reusable,
    RecoveryOnly,
    MemoryOnly,
}

/// Provider-side prefix cache behavior, disclosed separately from the local exact cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCacheMode {
    Off,
    ExplicitStablePrefix,
    UnavoidableImplicit,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CachePolicy {
    pub local: LocalCacheMode,
    pub provider: ProviderCacheMode,
}

/// A complete, immutable provider request. Its custom `Debug` never emits prompt or transcript text.
pub struct HostedRequest {
    pub call_id: CallId,
    pub group_id: RequestGroupId,
    pub target_id: Option<TargetId>,
    pub lane: Lane,
    pub fence: RequestFence,
    pub provider: ProviderId,
    pub transport: TransportId,
    pub model: ModelId,
    pub targets: Vec<RewriteTarget>,
    pub context: Vec<ContextRow>,
    pub prompt: CanonicalPrompt,
    /// Precomputed opaque provider prefix identity. It is present only when the selected provider cache
    /// policy requires one and cannot expose account, prompt, or transcript text.
    pub provider_cache_key: Option<crate::ProviderCacheKey>,
    pub deadline: MonotonicDeadline,
    pub cache_policy: CachePolicy,
}

impl fmt::Debug for HostedRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostedRequest")
            .field("call_id", &self.call_id)
            .field("group_id", &self.group_id)
            .field("target_id", &self.target_id)
            .field("lane", &self.lane)
            .field("fence", &self.fence)
            .field("provider", &self.provider)
            .field("transport", &self.transport)
            .field("model", &self.model)
            .field("target_count", &self.targets.len())
            .field("context_count", &self.context.len())
            .field("prompt", &self.prompt)
            .field("provider_cache_key", &self.provider_cache_key)
            .field("deadline", &self.deadline)
            .field("cache_policy", &self.cache_policy)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderDescriptor {
    pub provider: ProviderId,
    pub transport: TransportId,
    pub support_tier: SupportTier,
    pub billing_basis: BillingBasis,
    pub adapter_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderScope {
    pub connection_scope_id: ConnectionScopeId,
    pub region: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AdapterCapabilities {
    pub text_input: bool,
    pub text_output: bool,
    pub streaming: bool,
    pub structured_output: bool,
    pub explicit_prefix_cache: bool,
    pub implicit_cache_may_apply: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub provider: ProviderId,
    pub transport: TransportId,
    pub support_tier: SupportTier,
    pub exact_model_id: ModelId,
    pub account_scoped_available: bool,
    pub region: Option<String>,
    pub max_context_tokens: u64,
    pub max_output_tokens: u64,
    pub capabilities: AdapterCapabilities,
    pub billing_basis: BillingBasis,
    pub tariff_version: Option<String>,
    pub deprecated: bool,
    pub benchmarked_for_live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    pub models: Vec<ModelDescriptor>,
}

impl ModelCatalog {
    pub fn find_exact(&self, model: &ModelId, region: Option<&str>) -> Option<&ModelDescriptor> {
        self.models.iter().find(|candidate| {
            &candidate.exact_model_id == model && candidate.region.as_deref() == region
        })
    }
}

/// Secret-free credential source labels suitable for UI projection.
///
/// The AWS variants name only the *flavor* of resolution. Account ids, role ARNs, profile names, and
/// credential-file paths are deliberately unrepresentable here; the app projects those separately as
/// non-secret preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSourceKind {
    Keychain,
    WorkloadIdentity,
    ApplicationDefaultCredentials,
    BrokerKeyring,
    ChatGptDevice,
    AwsDefaultChain,
    AwsProfile,
    AwsStaticKeychain,
    AwsAssumedRole,
    AwsSso,
}

/// Secret-free credential state. Tokens, keys, provider bodies, and credential paths cannot be represented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CredentialState {
    Absent,
    Resolving,
    Ready {
        expires_at_unix_ms: Option<i64>,
        source: CredentialSourceKind,
    },
    AwaitingUser,
    DeviceAuthorization {
        verification_url: String,
        user_code: String,
        login_id: String,
    },
    Refreshing,
    Rejected,
    Unsupported {
        code: ErrorCode,
    },
    Error {
        code: ErrorCode,
    },
}

/// The exact visible Vertex unarmed warning; callers must not decorate or paraphrase it.
pub const VERTEX_UNARMED_WARNING: &str = "gcloud token isn't armed";

pub trait CredentialSource: Send {
    fn resolve(&mut self) -> CredentialState;
}

/// Content-free error taxonomy that may be persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    AuthUnarmed,
    AuthRejected,
    Permission,
    Quota,
    RateLimited,
    ModelUnavailable,
    Network,
    Timeout,
    Canceled,
    Superseded,
    PolicyBlocked,
    Cache,
    MalformedOutput,
    Provider,
    BrokerExited,
    AmbiguousDispatch,
    Internal,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AuthUnarmed => "authentication unarmed",
            Self::AuthRejected => "authentication rejected",
            Self::Permission => "permission denied",
            Self::Quota => "quota exhausted",
            Self::RateLimited => "rate limited",
            Self::ModelUnavailable => "model unavailable",
            Self::Network => "network failure",
            Self::Timeout => "deadline exceeded",
            Self::Canceled => "canceled",
            Self::Superseded => "superseded",
            Self::PolicyBlocked => "blocked by policy",
            Self::Cache => "cache failure",
            Self::MalformedOutput => "malformed output",
            Self::Provider => "provider failure",
            Self::BrokerExited => "broker exited",
            Self::AmbiguousDispatch => "ambiguous dispatch",
            Self::Internal => "internal failure",
        })
    }
}

/// Sanitized domain error. It intentionally has no free-form provider message/body field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("{code}")]
pub struct PostprocessError {
    pub code: ErrorCode,
}

impl From<ErrorCode> for PostprocessError {
    fn from(code: ErrorCode) -> Self {
        Self { code }
    }
}

/// Why local work was asked to stop. Cancellation remains best effort after dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
#[serde(rename_all = "snake_case")]
pub enum CancellationReason {
    Explicit = 1,
    MasterDisabled = 2,
    LaneDisabled = 3,
    Superseded = 4,
    SteeringChanged = 5,
    WordBankChanged = 6,
    ModelChanged = 7,
    SessionEnded = 8,
    Deadline = 9,
    Shutdown = 10,
}

impl CancellationReason {
    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Explicit,
            2 => Self::MasterDisabled,
            3 => Self::LaneDisabled,
            4 => Self::Superseded,
            5 => Self::SteeringChanged,
            6 => Self::WordBankChanged,
            7 => Self::ModelChanged,
            8 => Self::SessionEnded,
            9 => Self::Deadline,
            10 => Self::Shutdown,
            _ => return None,
        })
    }

    pub const fn error_code(self) -> ErrorCode {
        match self {
            Self::Superseded
            | Self::SteeringChanged
            | Self::WordBankChanged
            | Self::ModelChanged => ErrorCode::Superseded,
            Self::Deadline => ErrorCode::Timeout,
            _ => ErrorCode::Canceled,
        }
    }
}

/// A cloneable, runtime-free cancellation flag. The first reason wins deterministically.
#[derive(Clone, Default)]
pub struct CancellationToken {
    reason: Arc<AtomicU8>,
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancellationToken")
            .field("reason", &self.reason())
            .finish()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` only for the caller that records the first cancellation reason.
    pub fn cancel(&self, reason: CancellationReason) -> bool {
        self.reason
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn is_cancelled(&self) -> bool {
        self.reason.load(Ordering::Acquire) != 0
    }

    pub fn reason(&self) -> Option<CancellationReason> {
        CancellationReason::from_u8(self.reason.load(Ordering::Acquire))
    }

    pub fn check(&self) -> Result<(), PostprocessError> {
        match self.reason() {
            Some(reason) => Err(reason.error_code().into()),
            None => Ok(()),
        }
    }
}

/// Common identity carried by every provider event, including late terminal events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventContext {
    pub call_id: CallId,
    pub group_id: RequestGroupId,
    pub target_id: Option<TargetId>,
    pub lane: Lane,
    pub fence: RequestFence,
}

pub const MAX_TEXT_DELTA_BYTES: usize = 8 * 1024;

/// One bounded accepted text delta. Debug output is always redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct TextDelta(String);

impl TextDelta {
    pub fn new(text: impl Into<String>) -> Result<Self, PostprocessError> {
        let text = text.into();
        if text.is_empty() || text.len() > MAX_TEXT_DELTA_BYTES {
            return Err(ErrorCode::MalformedOutput.into());
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for TextDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TextDelta")
            .field("bytes", &self.0.len())
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheObservation {
    None,
    Local,
    ProviderRead,
    ProviderWrite,
    ProviderImplicit,
}

/// Event payloads contain only typed text deltas or content-free metadata—never provider bodies or secrets.
#[derive(Clone, PartialEq, Eq)]
pub enum ProviderEventKind {
    Queued,
    AuthWaiting,
    DispatchStarted,
    Headers,
    FirstText,
    TextDelta(TextDelta),
    UsageProvisional(NormalizedUsage),
    CacheObserved(CacheObservation),
    Completed(NormalizedUsage),
    Canceled {
        reason: CancellationReason,
        terminal_usage: Option<NormalizedUsage>,
        provider_billing_may_still_occur: bool,
    },
    Failed {
        code: ErrorCode,
        terminal_usage: Option<NormalizedUsage>,
    },
}

impl fmt::Debug for ProviderEventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Queued => f.write_str("Queued"),
            Self::AuthWaiting => f.write_str("AuthWaiting"),
            Self::DispatchStarted => f.write_str("DispatchStarted"),
            Self::Headers => f.write_str("Headers"),
            Self::FirstText => f.write_str("FirstText"),
            Self::TextDelta(delta) => f.debug_tuple("TextDelta").field(delta).finish(),
            Self::UsageProvisional(usage) => {
                f.debug_tuple("UsageProvisional").field(usage).finish()
            }
            Self::CacheObserved(cache) => f.debug_tuple("CacheObserved").field(cache).finish(),
            Self::Completed(usage) => f.debug_tuple("Completed").field(usage).finish(),
            Self::Canceled {
                reason,
                terminal_usage,
                provider_billing_may_still_occur,
            } => f
                .debug_struct("Canceled")
                .field("reason", reason)
                .field("terminal_usage", terminal_usage)
                .field(
                    "provider_billing_may_still_occur",
                    provider_billing_may_still_occur,
                )
                .finish(),
            Self::Failed {
                code,
                terminal_usage,
            } => f
                .debug_struct("Failed")
                .field("code", code)
                .field("terminal_usage", terminal_usage)
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderEvent {
    pub context: EventContext,
    pub kind: ProviderEventKind,
}

impl fmt::Debug for ProviderEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderEvent")
            .field("context", &self.context)
            .field("kind", &self.kind)
            .finish()
    }
}

pub trait ProviderEventSink: Send + Sync {
    fn emit(&self, event: ProviderEvent);
}

#[derive(Clone, PartialEq, Eq)]
pub struct QuestionTerminal {
    pub output: QuestionOutput,
}

impl fmt::Debug for QuestionTerminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuestionTerminal")
            .field("answer_bytes", &self.output.answer.len())
            .field("citation_count", &self.output.cited_row_ids.len())
            .field("context_truncated", &self.output.context_truncated)
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ProviderOutput {
    Rewrite(RewriteOutput),
    Question(QuestionTerminal),
}

impl fmt::Debug for ProviderOutput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rewrite(output) => f
                .debug_struct("Rewrite")
                .field("replacement_count", &output.replacements.len())
                .finish(),
            Self::Question(output) => f.debug_tuple("Question").field(output).finish(),
        }
    }
}

/// Monotonic phase durations in microseconds. An unobserved phase is `None`, never zero-filled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LatencyFields {
    pub queue_us: Option<u64>,
    pub auth_us: Option<u64>,
    pub cache_lookup_us: Option<u64>,
    pub connect_us: Option<u64>,
    pub ttfb_us: Option<u64>,
    pub ttft_us: Option<u64>,
    pub stream_us: Option<u64>,
    pub parse_us: Option<u64>,
    pub cache_commit_us: Option<u64>,
    pub total_us: Option<u64>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderTerminal {
    pub output: ProviderOutput,
    pub usage: NormalizedUsage,
    pub latency: LatencyFields,
    pub cache: CacheObservation,
}

impl fmt::Debug for ProviderTerminal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderTerminal")
            .field("output", &self.output)
            .field("usage", &self.usage)
            .field("latency", &self.latency)
            .field("cache", &self.cache)
            .finish()
    }
}

/// Blocking domain seam. Implementations may hide a private runtime, but callers never depend on it.
pub trait ProviderAdapter: Send {
    fn descriptor(&self) -> ProviderDescriptor;

    fn catalog(&mut self, scope: &ProviderScope) -> Result<ModelCatalog, PostprocessError>;

    fn execute(
        &mut self,
        request: &HostedRequest,
        cancel: &CancellationToken,
        sink: &dyn ProviderEventSink,
    ) -> Result<ProviderTerminal, PostprocessError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_truth_is_not_inferred_or_silently_upgraded() {
        assert_eq!(
            KnownTransport::VertexDirect.support_tier(),
            SupportTier::Documented
        );
        let chatgpt = KnownTransport::ChatGptSubscription.descriptor();
        assert_eq!(chatgpt.support_tier, SupportTier::Experimental);
        assert_eq!(chatgpt.billing_basis, BillingBasis::IncludedSubscription);
        assert!(chatgpt.adapter_available);
        assert_eq!(chatgpt.transport.as_str(), "chatgpt_subscription");
        assert_eq!(
            KnownTransport::ClaudeSubscription.support_tier(),
            SupportTier::Blocked
        );
        assert!(!KnownTransport::ClaudeSubscription.production_adapter_allowed());
        assert_eq!(VERTEX_UNARMED_WARNING, "gcloud token isn't armed");

        let bedrock = KnownTransport::BedrockRuntime.descriptor();
        assert_eq!(bedrock.provider.as_str(), "amazon");
        assert_eq!(bedrock.transport.as_str(), "bedrock_runtime");
        assert_eq!(bedrock.support_tier, SupportTier::Documented);
        assert_eq!(bedrock.billing_basis, BillingBasis::MeteredEstimate);
        assert!(bedrock.adapter_available);
    }

    #[test]
    fn aws_credential_source_labels_carry_no_account_detail() {
        for source in [
            CredentialSourceKind::AwsDefaultChain,
            CredentialSourceKind::AwsProfile,
            CredentialSourceKind::AwsStaticKeychain,
            CredentialSourceKind::AwsAssumedRole,
            CredentialSourceKind::AwsSso,
        ] {
            let state = CredentialState::Ready {
                expires_at_unix_ms: Some(1_787_000_000_000),
                source,
            };
            let rendered = serde_json::to_string(&state).unwrap();
            assert!(rendered.starts_with(r#"{"state":"ready""#));
            for forbidden in ["arn:", "/", "\\", "@"] {
                assert!(!rendered.contains(forbidden), "{rendered}");
            }
        }
    }

    #[test]
    fn first_cancellation_reason_wins_across_clones() {
        let token = CancellationToken::new();
        let worker = token.clone();
        assert!(worker.cancel(CancellationReason::WordBankChanged));
        assert!(!token.cancel(CancellationReason::Explicit));
        assert_eq!(token.reason(), Some(CancellationReason::WordBankChanged));
        assert_eq!(token.check().unwrap_err().code, ErrorCode::Superseded);
    }

    #[test]
    fn fence_matching_is_exact() {
        let fence = RequestFence {
            process_epoch: ProcessEpoch(7),
            session_generation: 1,
            transcript_revision: 2,
            control_revision: 3,
            lane_revision: 4,
            steering_revision: 5,
            bank_revision: 6,
            question_revision: None,
        };
        assert!(fence.is_current(&fence));
        let mut stale = fence.clone();
        stale.bank_revision += 1;
        assert!(!fence.is_current(&stale));
    }

    #[test]
    fn text_delta_debug_never_contains_text() {
        let delta = TextDelta::new("synthetic delta").unwrap();
        let rendered = format!("{delta:?}");
        assert!(!rendered.contains("synthetic"));
        assert!(rendered.contains("15"));
    }
}
