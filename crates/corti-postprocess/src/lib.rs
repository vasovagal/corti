//! Runtime-free domain contracts for hosted transcript post-processing.
//!
//! This crate deliberately contains no network client, async runtime, platform binding, database, or
//! filesystem access. Provider implementations live at the edge and consume these typed, fenced contracts.

#![forbid(unsafe_code)]

mod cache_key;
mod contracts;
mod diff;
mod ids;
mod pricing;
mod prompt;
mod validation;
mod word_bank;

pub use cache_key::{
    DigestKey, KeyedFingerprint, ProviderCacheKey, ProviderCacheKeyMaterial, RequestKey,
    RequestKeyMaterial,
};
pub use contracts::{
    AdapterCapabilities, BillingBasis, CacheObservation, CachePolicy, CancellationReason,
    CancellationToken, ContextRow, CredentialSource, CredentialSourceKind, CredentialState,
    ErrorCode, EventContext, HostedRequest, KnownTransport, Lane, LatencyFields, LocalCacheMode,
    ModelCatalog, ModelDescriptor, MonotonicDeadline, PostprocessError, ProviderAdapter,
    ProviderCacheMode, ProviderDescriptor, ProviderEvent, ProviderEventKind, ProviderEventSink,
    ProviderOutput, ProviderScope, ProviderTerminal, QuestionTerminal, RequestFence, RewriteTarget,
    SupportTier, TextDelta, TranscriptRow, VERTEX_UNARMED_WARNING,
};
pub use diff::{DiffError, DiffSpan, TextDiff, diff, diff_with_limit};
pub use ids::{
    CallId, ConnectionScopeId, IdentifierError, ModelId, ProcessEpoch, ProviderId, RequestGroupId,
    RowId, TargetId, TransportId,
};
pub use pricing::{
    CostEstimate, CurrencyCode, InputTokenAccounting, NormalizedUsage, PricingCatalog,
    PricingError, PricingQuery, RawUsage, Tariff, TariffCatalog, TariffRates,
};
pub use prompt::{
    CanonicalPrompt, OUTPUT_SCHEMA_VERSION, PROMPT_TEMPLATE_VERSION, PromptMessage, PromptRole,
    PromptSection, PromptTask,
};
pub use validation::{
    QUESTION_SCHEMA_VERSION, QuestionOutput, REWRITE_SCHEMA_VERSION, Replacement, RewriteOutput,
    RewriteValidationLimits, ValidatedQuestion, ValidatedRewrite, ValidationError,
    parse_and_validate_question, parse_and_validate_rewrite, validate_disjoint_target_chunks,
};
pub use word_bank::{
    MAX_WORD_BANK_BYTES, MAX_WORD_BANK_ENTRIES, MAX_WORD_BANK_ENTRY_SCALARS, WORD_BANK_SCHEMA,
    WordBankDocument, WordBankError, normalize_word_bank_entry,
};
