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

pub mod coordinator;

pub use coordinator::*;
