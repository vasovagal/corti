//! Shared domain types for corti, with no platform dependencies.
//!
//! Every other crate in the workspace depends on this one; it deliberately pulls in nothing
//! macOS-specific so the heavy crates (`corti-coreaudio`, `corti-capture`, the transcription backends)
//! stay decoupled and free of cycles.

pub mod activity;
mod app;
pub mod capture_accounting;
pub mod capture_mismatch;
pub mod capture_normalizer;
pub mod quality;
mod quality_activity;
pub mod quality_mapping;
pub mod quality_store;
mod recording;
mod transcript;

pub use activity::{ActivityBlock, ActivityInputError, ActivityLevels, ChannelActivity};
pub use app::{OwningApp, is_known_conferencing_app};
pub use capture_mismatch::MicTapMismatches;
pub use quality::{
    BatchRecoveryOutcome, CaptureEpoch, CaptureEpochReason, MAX_QUALITY_EPOCHS, MAX_QUALITY_SPANS,
    QUALITY_SCHEMA_VERSION, QualityCause, QualityClass, QualityCoverage, QualityError,
    QualityOverflow, QualityRecord, QualityResolution, QualitySpan, QualityStage, QualityTrack,
    SourceCompleteness,
};
pub use quality_activity::{
    ActivityEvidence, ActivityInput, ActivityScope, FarReference, QualityActivity,
};
pub use quality_mapping::insert_retained_processing_loss;
pub use recording::{JobStatus, RecordingMeta, RecordingMode, WEBINAR_NAME};
pub use transcript::{DiarizedTranscript, Speaker, TranscriptSegment};
