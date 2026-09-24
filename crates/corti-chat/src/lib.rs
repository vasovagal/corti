//! `corti-chat` — the runtime-free hosted chat harness.
//!
//! This crate owns the pure state machines that sit between Corti's live ASR rows and the injected
//! provider adapters: request fences, lane scheduling, cancellation, exact-cache ordering, Vertex
//! catch-up, and the encrypted-store journal protocol. Nothing here discovers ambient credentials,
//! opens a network connection, or depends on Tauri, so the whole engine builds and tests on any
//! platform even though the Corti app itself is macOS-only.
//!
//! Layering: `corti-postprocess` (contracts) → `corti-postprocess-providers` (adapters) →
//! `corti-chat` (this engine) → the Corti app (Service/Tauri glue).

#![forbid(unsafe_code)]

pub mod batch;
pub mod cache_policy;
pub mod coordinator;
pub mod subscriptions;

pub use batch::{BacklogReleaseReason, Batch, LiveBatchPolicy, LiveBatcher, ReleasedBacklog};
pub use cache_policy::{
    CHATGPT_SUBSCRIPTION_TRANSPORT, CachePolicyBlock, effective_provider_cache,
    stored_policy_is_acceptable,
};
pub use coordinator::*;
pub use subscriptions::{
    AnswerFormat, ContextWindow, MAX_SUBSCRIPTIONS, MAX_TEMPLATE_BYTES, ProgressLedger,
    SpeakerFilter, SubscriptionError, SubscriptionId, SubscriptionPreset, SubscriptionSpec,
    TriggerPolicy, looks_asked_of_me, partial_answer_from_json_prefix, preset_title,
    rows_contain_question_for_me,
};
