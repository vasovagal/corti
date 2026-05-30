//! Shared domain types for corti, with no platform dependencies.
//!
//! Every other crate in the workspace depends on this one; it deliberately pulls in nothing
//! macOS-specific so the heavy crates (`corti-coreaudio`, `corti-capture`, the transcription backends)
//! stay decoupled and free of cycles.

mod app;
mod recording;
mod transcript;

pub use app::{OwningApp, is_known_conferencing_app};
pub use recording::{JobStatus, RecordingMeta};
pub use transcript::{DiarizedTranscript, Speaker, TranscriptSegment};
