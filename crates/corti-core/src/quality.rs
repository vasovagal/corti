//! Bounded, content-free accounting shared by capture and transcription.
//!
//! Frames are source coordinates within an epoch, not positions in the retained WAV. Mutation and
//! snapshot merging belong off the audio callback; these vectors are not a HAL event queue.

use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::quality_activity::QualityActivity;

pub const QUALITY_SCHEMA_VERSION: u32 = 1;
pub const MAX_QUALITY_EPOCHS: usize = 64;
pub const MAX_QUALITY_SPANS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityCoverage {
    Observed,
    LegacyUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureEpochReason {
    Start,
    HostDiscontinuity,
    LayoutChange,
    RateChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureEpoch {
    pub id: u32,
    pub source_start_ns: Option<u64>,
    pub source_rate_hz: u32,
    pub source_frame_base: u64,
    pub retained_start_frame: u64,
    pub retained_rate_hz: u32,
    pub mic_channels: u32,
    pub tap_channels: u32,
    pub reason: CaptureEpochReason,
}

impl CaptureEpoch {
    fn validate(&self) -> Result<(), QualityError> {
        if self.source_rate_hz == 0 || self.retained_rate_hz == 0 {
            return Err(QualityError::InvalidRate);
        }
        if self
            .mic_channels
            .checked_add(self.tap_channels)
            .is_none_or(|channels| channels == 0)
        {
            return Err(QualityError::InvalidChannels);
        }
        Ok(())
    }

    /// Recording-relative source time, preserving an unknown timestamp as `None`.
    pub fn source_time_ns(&self, frame: u64) -> Result<Option<u64>, QualityError> {
        self.validate()?;
        let delta = frame
            .checked_sub(self.source_frame_base)
            .ok_or(QualityError::InvalidInterval)?;
        self.source_start_ns
            .map(|base| {
                let nanos = u128::from(delta) * 1_000_000_000 / u128::from(self.source_rate_hz);
                let nanos = u64::try_from(nanos).map_err(|_| QualityError::ArithmeticOverflow)?;
                base.checked_add(nanos)
                    .ok_or(QualityError::ArithmeticOverflow)
            })
            .transpose()
    }

    /// Map a source boundary to the fixed-rate WAV. Computing from the epoch base avoids cumulative
    /// rounding error; a streaming converter must likewise carry its fractional remainder.
    pub fn retained_frame(&self, frame: u64) -> Result<u64, QualityError> {
        self.validate()?;
        let delta = frame
            .checked_sub(self.source_frame_base)
            .ok_or(QualityError::InvalidInterval)?;
        let frames =
            u128::from(delta) * u128::from(self.retained_rate_hz) / u128::from(self.source_rate_hz);
        let frames = u64::try_from(frames).map_err(|_| QualityError::ArithmeticOverflow)?;
        self.retained_start_frame
            .checked_add(frames)
            .ok_or(QualityError::ArithmeticOverflow)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityTrack {
    Mic,
    Tap,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityCause {
    RingOverflow,
    HostDiscontinuity,
    LayoutChange,
    RateChange,
    TeeDrop,
    MicTapMismatch,
    DecodeFailure,
    NoResult,
    ResamplerFailure,
    WindowNotFiled,
    ForcedBoundary,
    FarReferenceSilent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityClass {
    SourceLoss,
    ProcessingLoss,
    DegradedOnly,
}

impl QualityClass {
    /// Stable index in both overflow arrays: source, processing, degraded.
    pub const fn index(self) -> usize {
        match self {
            Self::SourceLoss => 0,
            Self::ProcessingLoss => 1,
            Self::DegradedOnly => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityStage {
    Capture,
    Live,
    Batch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityResolution {
    Outstanding,
    RecoveredByBatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QualitySpan {
    pub epoch: u32,
    pub start_frame: u64,
    pub end_frame: u64,
    pub track: QualityTrack,
    pub cause: QualityCause,
    pub class: QualityClass,
    pub stage: QualityStage,
    pub resolution: QualityResolution,
}

impl QualitySpan {
    fn validate_class(&self) -> Result<(), QualityError> {
        let valid = match self.cause {
            QualityCause::RingOverflow | QualityCause::HostDiscontinuity => {
                self.class == QualityClass::SourceLoss
            }
            QualityCause::LayoutChange | QualityCause::RateChange => {
                matches!(
                    self.class,
                    QualityClass::SourceLoss | QualityClass::DegradedOnly
                )
            }
            QualityCause::TeeDrop
            | QualityCause::MicTapMismatch
            | QualityCause::DecodeFailure
            | QualityCause::NoResult
            | QualityCause::WindowNotFiled => self.class == QualityClass::ProcessingLoss,
            QualityCause::ResamplerFailure => {
                self.class
                    == if self.stage == QualityStage::Capture {
                        QualityClass::SourceLoss
                    } else {
                        QualityClass::ProcessingLoss
                    }
            }
            QualityCause::ForcedBoundary | QualityCause::FarReferenceSilent => {
                self.class == QualityClass::DegradedOnly
            }
        };
        if valid {
            Ok(())
        } else {
            Err(QualityError::InvalidClassification)
        }
    }

    fn same_kind(&self, other: &Self) -> bool {
        self.epoch == other.epoch
            && self.track == other.track
            && self.cause == other.cause
            && self.class == other.class
            && self.stage == other.stage
            && self.resolution == other.resolution
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityOverflow {
    pub omitted_spans: [u64; 3],
    pub outstanding_omitted_spans: [u64; 3],
    pub omitted_epochs: u64,
    pub counter_saturated: bool,
}

impl QualityOverflow {
    fn omit_span(
        &mut self,
        class: QualityClass,
        resolution: QualityResolution,
        count: u64,
        activity_saturated: Option<&mut bool>,
    ) {
        let saturated = activity_saturated.unwrap_or(&mut self.counter_saturated);
        saturating_count(&mut self.omitted_spans[class.index()], count, saturated);
        if resolution == QualityResolution::Outstanding {
            saturating_count(
                &mut self.outstanding_omitted_spans[class.index()],
                count,
                saturated,
            );
        }
    }

    fn merge_lower_bounds(&mut self, other: &Self) {
        for index in 0..3 {
            self.omitted_spans[index] = self.omitted_spans[index].max(other.omitted_spans[index]);
            self.outstanding_omitted_spans[index] =
                self.outstanding_omitted_spans[index].max(other.outstanding_omitted_spans[index]);
        }
        self.omitted_epochs = self.omitted_epochs.max(other.omitted_epochs);
        self.counter_saturated |= other.counter_saturated;
    }
}

fn saturating_count(value: &mut u64, count: u64, saturated: &mut bool) {
    match value.checked_add(count) {
        Some(sum) => *value = sum,
        None => {
            *value = u64::MAX;
            *saturated = true;
        }
    }
}

/// The single persisted quality model. Missing optional carrier fields should remain `None`, or use
/// this type's legacy-unknown default; they must never be replaced with [`Self::observed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityRecord {
    pub schema_version: u32,
    pub coverage: QualityCoverage,
    pub epochs: Vec<CaptureEpoch>,
    pub spans: Vec<QualitySpan>,
    pub overflow: QualityOverflow,
    pub activity: QualityActivity,
}

impl Default for QualityRecord {
    fn default() -> Self {
        Self {
            schema_version: QUALITY_SCHEMA_VERSION,
            coverage: QualityCoverage::LegacyUnknown,
            epochs: Vec::new(),
            spans: Vec::new(),
            overflow: QualityOverflow::default(),
            activity: QualityActivity::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCompleteness {
    Complete,
    Incomplete,
    Unknown,
}

/// Only `SameBackendWholeFileSuccess` authorizes recovery. The caller verifies backend identity,
/// complete WAV coverage and decoder success before selecting that variant; this is not egress
/// authorization. A successful decode of a single region is `Partial`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchRecoveryOutcome {
    SameBackendWholeFileSuccess,
    /// User explicitly selected local whole-file recovery and that local decode succeeded. This
    /// authorizes processing recovery without asserting any historical backend identity.
    ExplicitLocalWholeFileSuccess,
    Failed,
    Partial,
    DifferentBackend,
    UnknownBackend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityError {
    UnsupportedVersion,
    CapacityExceeded,
    InvalidRate,
    InvalidChannels,
    InvalidEpochOrder,
    ConflictingEpoch,
    UnknownEpoch,
    UnknownAvailability,
    InvalidInterval,
    InvalidClassification,
    InvalidResolution,
    InvalidOverflow,
    InvalidActivity,
    ArithmeticOverflow,
}

impl fmt::Display for QualityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedVersion => "unsupported quality schema version",
            Self::CapacityExceeded => "quality record capacity exceeded",
            Self::InvalidRate => "invalid quality sample rate",
            Self::InvalidChannels => "invalid quality channel count",
            Self::InvalidEpochOrder => "invalid quality epoch order or mapping",
            Self::ConflictingEpoch => "conflicting quality epoch",
            Self::UnknownEpoch => "unknown quality epoch",
            Self::UnknownAvailability => {
                "unknown source availability after quality accounting overflow"
            }
            Self::InvalidInterval => "invalid quality interval",
            Self::InvalidClassification => "invalid quality cause classification",
            Self::InvalidResolution => "invalid quality resolution",
            Self::InvalidOverflow => "invalid quality overflow counters",
            Self::InvalidActivity => "invalid quality activity evidence",
            Self::ArithmeticOverflow => "quality frame arithmetic overflow",
        })
    }
}

impl std::error::Error for QualityError {}

impl QualityRecord {
    /// Start an observed capture. Storage completion is a separate, durable-envelope decision.
    pub fn observed() -> Self {
        Self {
            coverage: QualityCoverage::Observed,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), QualityError> {
        if self.schema_version != QUALITY_SCHEMA_VERSION {
            return Err(QualityError::UnsupportedVersion);
        }
        if self.epochs.len() > MAX_QUALITY_EPOCHS || self.spans.len() > MAX_QUALITY_SPANS {
            return Err(QualityError::CapacityExceeded);
        }
        validate_epochs(&self.epochs)?;
        self.activity.validate()?;
        let (activity_omissions, activity_saturated) = activity_omission_sum(&self.activity);
        if self.overflow.omitted_spans[QualityClass::DegradedOnly.index()] < activity_omissions
            || (activity_saturated && !self.activity.omission_counter_saturated)
        {
            return Err(QualityError::InvalidActivity);
        }
        for span in &self.spans {
            self.validate_span(span)?;
        }
        for index in 0..3 {
            let total = self.overflow.omitted_spans[index];
            let outstanding = self.overflow.outstanding_omitted_spans[index];
            if outstanding > total
                || (index != QualityClass::ProcessingLoss.index() && outstanding != total)
            {
                return Err(QualityError::InvalidOverflow);
            }
        }
        Ok(())
    }

    fn validate_span(&self, span: &QualitySpan) -> Result<(), QualityError> {
        span.validate_class()?;
        validate_resolution(span.class, span.resolution)?;
        let epoch_index = self
            .epochs
            .iter()
            .position(|epoch| epoch.id == span.epoch)
            .ok_or(QualityError::UnknownEpoch)?;
        let epoch = &self.epochs[epoch_index];
        if span.start_frame >= span.end_frame || span.start_frame < epoch.source_frame_base {
            return Err(QualityError::InvalidInterval);
        }
        let source_end_ns = epoch.source_time_ns(span.end_frame)?;
        epoch.retained_frame(span.end_frame)?;
        if let Some(next) = self.epochs.get(epoch_index + 1)
            && let (Some(end), Some(next_start)) = (source_end_ns, next.source_start_ns)
            && end > next_start
        {
            // Offsets store whole nanoseconds: a fractional frame boundary and its next epoch
            // necessarily share the same rounded-down value. Do not compare retained endpoints:
            // a large unfilled source gap deliberately compresses the WAV coordinate.
            return Err(QualityError::InvalidInterval);
        }
        Ok(())
    }

    /// Append a new monotonically numbered epoch. `false` means the epoch was counted but omitted.
    /// Producers must never reuse an ID, including IDs omitted after the cap.
    pub fn insert_epoch(&mut self, epoch: CaptureEpoch) -> Result<bool, QualityError> {
        self.validate()?;
        epoch.validate()?;
        if let Some(previous) = self.epochs.iter().find(|previous| previous.id == epoch.id) {
            return if *previous == epoch {
                Ok(true)
            } else {
                Err(QualityError::ConflictingEpoch)
            };
        }
        if let Some(previous) = self.epochs.last() {
            validate_epoch_pair(previous, &epoch)?;
        } else if epoch.reason != CaptureEpochReason::Start {
            return Err(QualityError::InvalidEpochOrder);
        }
        if let Some(offset) = epoch.source_start_ns
            && self
                .epochs
                .iter()
                .filter_map(|previous| previous.source_start_ns)
                .any(|previous| previous > offset)
        {
            return Err(QualityError::InvalidEpochOrder);
        }
        if self.epochs.len() == MAX_QUALITY_EPOCHS {
            self.record_omitted_epochs(1);
            return Ok(false);
        }
        self.epochs.push(epoch);
        if let Err(error) = self.validate() {
            self.epochs.pop();
            return Err(error);
        }
        Ok(true)
    }

    /// Add a positioned fact. Matching adjacent/overlapping intervals coalesce before the cap.
    /// `false` means its class and resolution were retained in the overflow summary instead.
    pub fn insert_span(&mut self, span: QualitySpan) -> Result<bool, QualityError> {
        if !self.try_insert_span(span)? {
            if span.cause == QualityCause::FarReferenceSilent {
                self.record_omitted_activity_run()?;
            } else {
                self.overflow
                    .omit_span(span.class, span.resolution, 1, None);
            }
            return Ok(false);
        }
        Ok(true)
    }

    /// The same bounded insertion/coalescing used by insert_span. The activity tracker owns its
    /// per-traversal lower bound and accounts a rejected run once through observe_activity_omissions.
    pub(crate) fn try_insert_span(&mut self, span: QualitySpan) -> Result<bool, QualityError> {
        self.validate()?;
        self.validate_span(&span)?;
        let span = coalesce(&mut self.spans, span);
        if self.spans.len() == MAX_QUALITY_SPANS {
            return Ok(false);
        }
        self.spans.push(span);
        self.spans.sort_unstable();
        Ok(true)
    }

    /// Account for facts whose epoch/interval could not be retained. Never invent an interval for
    /// these counts. Callers must display the overflow summary alongside retained markers.
    pub fn record_omitted_spans(
        &mut self,
        class: QualityClass,
        resolution: QualityResolution,
        count: u64,
    ) -> Result<(), QualityError> {
        self.validate()?;
        validate_resolution(class, resolution)?;
        self.overflow.omit_span(class, resolution, count, None);
        Ok(())
    }

    /// Activity counter saturation is diagnostic; it cannot revoke capture completeness. Keep
    /// the existing degraded overflow lower bound and record its saturation in the fixed activity
    /// summary, independently of source/processing accounting saturation.
    pub(crate) fn record_omitted_activity_run(&mut self) -> Result<(), QualityError> {
        self.validate()?;
        self.overflow.omit_span(
            QualityClass::DegradedOnly,
            QualityResolution::Outstanding,
            1,
            Some(&mut self.activity.omission_counter_saturated),
        );
        Ok(())
    }

    /// Only a newly raised per-scope lower bound advances the shared degraded total. A fresh retry
    /// starts its local count at zero, so carrying already settled quality cannot recount old runs.
    /// Incompatible input bindings remain explicit conflicts; their counts are conservative maxima.
    pub(crate) fn observe_activity_omissions(
        &mut self,
        scope: crate::ActivityScope,
        input: crate::ActivityInput,
        omitted_runs: u64,
        saturated: bool,
    ) -> Result<(), QualityError> {
        self.validate()?;
        let delta = omitted_runs.saturating_sub(self.activity.get(scope).omitted_runs());
        self.overflow.omit_span(
            QualityClass::DegradedOnly,
            QualityResolution::Outstanding,
            delta,
            Some(&mut self.activity.omission_counter_saturated),
        );
        self.activity.omission_counter_saturated |= saturated;
        self.activity.observe(
            scope,
            crate::quality_activity::ActivityEvidence {
                input: Some(input),
                omitted_runs,
                ..Default::default()
            },
        );
        Ok(())
    }

    /// Merge supplementary activity against this record's epoch authority. Activity cannot supply
    /// capture coverage or epochs: a conflicting/missing mapping keeps its summary and counts the
    /// unpositioned diagnostic through the existing per-scope lower bound. Repeated snapshots are
    /// idempotent, including when the authoritative record has reached either capacity limit.
    pub fn merge_activity_snapshot(&mut self, snapshot: &Self) -> Result<(), QualityError> {
        self.validate()?;
        snapshot.validate()?;
        let mut activity = Self {
            coverage: self.coverage,
            epochs: self.epochs.clone(),
            activity: snapshot.activity,
            ..Self::default()
        };
        let (omitted, saturated) = activity_omission_sum(&activity.activity);
        activity.overflow.omitted_spans[2] = omitted;
        activity.overflow.outstanding_omitted_spans[2] = omitted;
        activity.activity.omission_counter_saturated |= saturated;
        let mut unmapped = [0_u64; 3];
        for span in &snapshot.spans {
            if span.cause != QualityCause::FarReferenceSilent {
                continue;
            }
            let exact_epoch = snapshot
                .epochs
                .iter()
                .find(|epoch| epoch.id == span.epoch)
                .is_some_and(|epoch| self.epochs.contains(epoch));
            if exact_epoch && activity.validate_span(span).is_ok() {
                activity.insert_span(*span)?;
            } else {
                unmapped[match span.stage {
                    QualityStage::Capture => 0,
                    QualityStage::Batch => 1,
                    QualityStage::Live => 2,
                }] += 1; // At most MAX_QUALITY_SPANS.
            }
        }
        for (scope, count) in [
            crate::ActivityScope::Capture,
            crate::ActivityScope::Offline,
            crate::ActivityScope::Live,
        ]
        .into_iter()
        .zip(unmapped)
        {
            if let Some(input) = activity.activity.get(scope).input() {
                let previous = activity.activity.get(scope).omitted_runs();
                activity.observe_activity_omissions(
                    scope,
                    input,
                    previous.saturating_add(count),
                    previous.checked_add(count).is_none(),
                )?;
            } else {
                for _ in 0..count {
                    activity.record_omitted_activity_run()?;
                }
            }
        }
        self.merge_snapshot(&activity)
    }

    /// Accounting loss makes completeness unknown even when no positioned loss survived.
    pub fn record_omitted_epochs(&mut self, count: u64) {
        saturating_count(
            &mut self.overflow.omitted_epochs,
            count,
            &mut self.overflow.counter_saturated,
        );
    }

    /// Union snapshots of the same recording without treating a recovered copy as permission to
    /// resolve an outstanding copy. Conflicting epoch mappings return an error without mutation.
    ///
    /// Overflow counts merge by maximum: they are conservative lower bounds if independently
    /// capped histories diverged, not exact totals across independent streams. Append fresh stage
    /// events to the carried record instead of constructing independent capped ledgers.
    pub fn merge_snapshot(&mut self, other: &Self) -> Result<(), QualityError> {
        self.validate()?;
        other.validate()?;
        // Temporary unions are bounded by twice the persisted caps (128 epochs, 512 spans).
        let mut epochs = self.epochs.clone();
        for epoch in &other.epochs {
            if let Some(previous) = epochs.iter().find(|previous| previous.id == epoch.id) {
                if previous != epoch {
                    return Err(QualityError::ConflictingEpoch);
                }
            } else {
                epochs.push(*epoch);
            }
        }
        epochs.sort_unstable_by_key(|epoch| epoch.id);
        validate_epochs(&epochs)?;
        let mut merged = Self {
            activity: self.activity,
            ..Self::default()
        };
        merged.activity.merge(other.activity);
        if self.coverage == QualityCoverage::Observed && other.coverage == QualityCoverage::Observed
        {
            merged.coverage = QualityCoverage::Observed;
        }
        merged.overflow.omitted_epochs = epochs.len().saturating_sub(MAX_QUALITY_EPOCHS) as u64;
        epochs.truncate(MAX_QUALITY_EPOCHS);
        merged.epochs = epochs;
        let mut spans = Vec::with_capacity(self.spans.len() + other.spans.len());
        for span in self.spans.iter().chain(&other.spans) {
            let span = coalesce(&mut spans, *span);
            spans.push(span);
        }
        spans.sort_unstable();
        let mut omitted_activity = [0_u64; 3];
        for span in spans {
            if merged.spans.len() < MAX_QUALITY_SPANS
                && merged.epochs.iter().any(|epoch| epoch.id == span.epoch)
            {
                merged.spans.push(span);
            } else {
                merged
                    .overflow
                    .omit_span(span.class, span.resolution, 1, None);
                if span.cause == QualityCause::FarReferenceSilent {
                    omitted_activity[match span.stage {
                        QualityStage::Capture => 0,
                        QualityStage::Batch => 1,
                        QualityStage::Live => 2,
                    }] += 1; // At most twice MAX_QUALITY_SPANS in this temporary union.
                }
            }
        }
        for (scope, count) in [
            crate::ActivityScope::Capture,
            crate::ActivityScope::Offline,
            crate::ActivityScope::Live,
        ]
        .into_iter()
        .zip(omitted_activity)
        {
            if let Some(input) = merged.activity.get(scope).input() {
                merged.activity.observe(
                    scope,
                    crate::quality_activity::ActivityEvidence {
                        input: Some(input),
                        omitted_runs: count,
                        ..Default::default()
                    },
                );
            }
        }
        merged.overflow.merge_lower_bounds(&self.overflow);
        merged.overflow.merge_lower_bounds(&other.overflow);
        // Stage observations are distinct even when their snapshots were made independently.
        // Generic degraded history remains max-merged; adding it would recount shared histories.
        let (activity_omissions, activity_saturated) = activity_omission_sum(&merged.activity);
        merged.overflow.omitted_spans[2] = merged.overflow.omitted_spans[2].max(activity_omissions);
        merged.overflow.outstanding_omitted_spans[2] = merged.overflow.omitted_spans[2];
        merged.activity.omission_counter_saturated |= activity_saturated;
        merged.validate()?;
        *self = merged;
        Ok(())
    }

    /// Resolve only processing loss after verified complete recovery. Source loss, degraded facts,
    /// unknown coverage, epoch omissions and saturation survive every successful retry/redo.
    pub fn resolve_processing_after_batch(
        &mut self,
        outcome: BatchRecoveryOutcome,
    ) -> Result<bool, QualityError> {
        self.validate()?;
        if !matches!(
            outcome,
            BatchRecoveryOutcome::SameBackendWholeFileSuccess
                | BatchRecoveryOutcome::ExplicitLocalWholeFileSuccess
        ) {
            return Ok(false);
        }
        let old_spans = std::mem::take(&mut self.spans);
        for mut span in old_spans {
            if span.class == QualityClass::ProcessingLoss {
                span.resolution = QualityResolution::RecoveredByBatch;
            }
            let span = coalesce(&mut self.spans, span);
            self.spans.push(span);
        }
        self.spans.sort_unstable();
        self.overflow.outstanding_omitted_spans[QualityClass::ProcessingLoss.index()] = 0;
        Ok(true)
    }

    /// Completeness of captured source audio, independent of decoding success.
    pub fn source_completeness(&self) -> SourceCompleteness {
        if self.has_outstanding(QualityClass::SourceLoss) {
            return SourceCompleteness::Incomplete;
        }
        if self.validate().is_err()
            || self.coverage == QualityCoverage::LegacyUnknown
            || self.overflow.omitted_epochs != 0
            || self.overflow.counter_saturated
            || self
                .epochs
                .iter()
                .any(|epoch| epoch.source_start_ns.is_none())
        {
            SourceCompleteness::Unknown
        } else {
            SourceCompleteness::Complete
        }
    }

    /// Combined source/processing status. Degraded-only facts never make it incomplete.
    pub fn completeness(&self) -> SourceCompleteness {
        if self.has_outstanding(QualityClass::SourceLoss)
            || self.has_outstanding(QualityClass::ProcessingLoss)
        {
            SourceCompleteness::Incomplete
        } else {
            self.source_completeness()
        }
    }

    /// Only outstanding processing loss calls for whole-file recovery. Unknown/source-only records
    /// cannot be certified complete, but decoding cannot repair their missing capture history.
    pub fn requires_batch_recovery(&self) -> bool {
        self.has_outstanding(QualityClass::ProcessingLoss)
    }

    /// Project known source loss onto retained WAV frames for post-filter availability masking.
    /// The temporary list has at most one entry per retained span (256). Every interval is clipped
    /// to its epoch's retained boundary: a compressed-away discontinuity must not mask the first
    /// retained audio of the following epoch. Omitted source losses or saturated accounting make
    /// exact masking unavailable: callers must preserve the raw PCM instead of filtering a subset.
    /// Legacy coverage alone retains historical behavior without certifying source completeness.
    pub fn retained_source_loss_intervals(
        &self,
        total_retained_frames: u64,
    ) -> Result<Vec<(u64, u64, QualityTrack)>, QualityError> {
        self.validate()?;
        if self.overflow.omitted_spans[QualityClass::SourceLoss.index()] != 0
            || self.overflow.counter_saturated
        {
            return Err(QualityError::UnknownAvailability);
        }
        let mut intervals = Vec::new();
        for span in self
            .spans
            .iter()
            .filter(|span| span.class == QualityClass::SourceLoss)
        {
            let index = self
                .epochs
                .iter()
                .position(|epoch| epoch.id == span.epoch)
                .ok_or(QualityError::UnknownEpoch)?;
            let epoch = &self.epochs[index];
            if index + 1 == self.epochs.len()
                && self.overflow.omitted_epochs != 0
                && epoch.retained_frame(span.start_frame)? < total_retained_frames
            {
                // The writer may have retained later PCM after the epoch cap. Without its next retained
                // boundary, a source gap in this last known epoch could cover compressed-away time.
                return Err(QualityError::UnknownEpoch);
            }
            let limit = self
                .epochs
                .get(index + 1)
                .map_or(total_retained_frames, |next| {
                    next.retained_start_frame.min(total_retained_frames)
                });
            let start = epoch
                .retained_frame(span.start_frame)?
                .max(epoch.retained_start_frame)
                .min(limit);
            let end = epoch.retained_frame(span.end_frame)?.min(limit);
            if start < end {
                intervals.push((start, end, span.track));
            }
        }
        Ok(intervals)
    }

    fn has_outstanding(&self, class: QualityClass) -> bool {
        // A public-field mutation must not hide a known source fact behind an invalid recovered
        // status. Validation rejects that record too, but completeness still reports the loss.
        if class == QualityClass::SourceLoss {
            return self.overflow.omitted_spans[class.index()] != 0
                || self.overflow.outstanding_omitted_spans[class.index()] != 0
                || self.spans.iter().any(|span| span.class == class);
        }
        self.overflow.outstanding_omitted_spans[class.index()] != 0
            || self.spans.iter().any(|span| {
                span.class == class && span.resolution == QualityResolution::Outstanding
            })
    }
}

fn activity_omission_sum(activity: &QualityActivity) -> (u64, bool) {
    let mut count = 0;
    let mut saturated = false;
    for scope in [
        crate::ActivityScope::Capture,
        crate::ActivityScope::Offline,
        crate::ActivityScope::Live,
    ] {
        saturating_count(
            &mut count,
            activity.get(scope).omitted_runs(),
            &mut saturated,
        );
    }
    (count, saturated)
}

fn validate_resolution(
    class: QualityClass,
    resolution: QualityResolution,
) -> Result<(), QualityError> {
    if resolution == QualityResolution::RecoveredByBatch && class != QualityClass::ProcessingLoss {
        Err(QualityError::InvalidResolution)
    } else {
        Ok(())
    }
}

fn validate_epoch_pair(previous: &CaptureEpoch, epoch: &CaptureEpoch) -> Result<(), QualityError> {
    if epoch.id <= previous.id
        || epoch.reason == CaptureEpochReason::Start
        || epoch.retained_rate_hz != previous.retained_rate_hz
        || epoch.retained_start_frame < previous.retained_start_frame
    {
        Err(QualityError::InvalidEpochOrder)
    } else {
        Ok(())
    }
}

fn validate_epochs(epochs: &[CaptureEpoch]) -> Result<(), QualityError> {
    let mut last_known_offset = None;
    for (index, epoch) in epochs.iter().enumerate() {
        epoch.validate()?;
        if index == 0 {
            if epoch.reason != CaptureEpochReason::Start {
                return Err(QualityError::InvalidEpochOrder);
            }
        } else {
            validate_epoch_pair(&epochs[index - 1], epoch)?;
        }
        if let Some(offset) = epoch.source_start_ns {
            if last_known_offset.is_some_and(|previous| offset < previous) {
                return Err(QualityError::InvalidEpochOrder);
            }
            last_known_offset = Some(offset);
        }
    }
    Ok(())
}

fn coalesce(spans: &mut Vec<QualitySpan>, mut span: QualitySpan) -> QualitySpan {
    // Restart after widening: the new interval can bridge an earlier non-overlapping candidate.
    let mut index = 0;
    while index < spans.len() {
        let previous = &spans[index];
        if previous.same_kind(&span)
            && previous.end_frame >= span.start_frame
            && span.end_frame >= previous.start_frame
        {
            span.start_frame = span.start_frame.min(previous.start_frame);
            span.end_frame = span.end_frame.max(previous.end_frame);
            spans.swap_remove(index);
            index = 0;
        } else {
            index += 1;
        }
    }
    span
}

impl Serialize for QualityRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.validate().map_err(serde::ser::Error::custom)?;
        let mut record = serializer.serialize_struct("QualityRecord", 6)?;
        record.serialize_field("schema_version", &self.schema_version)?;
        record.serialize_field("coverage", &self.coverage)?;
        record.serialize_field("epochs", &self.epochs)?;
        record.serialize_field("spans", &self.spans)?;
        record.serialize_field("overflow", &self.overflow)?;
        record.serialize_field("activity", &self.activity)?;
        record.end()
    }
}

impl<'de> Deserialize<'de> for QualityRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct WireRecord {
            schema_version: u32,
            coverage: QualityCoverage,
            #[serde(deserialize_with = "bounded_vec::<_, _, MAX_QUALITY_EPOCHS>")]
            epochs: Vec<CaptureEpoch>,
            #[serde(deserialize_with = "bounded_vec::<_, _, MAX_QUALITY_SPANS>")]
            spans: Vec<QualitySpan>,
            overflow: QualityOverflow,
            #[serde(default)]
            activity: QualityActivity,
        }
        // Derived enum/type errors can include arbitrary payload values. The record crosses durable
        // carriers whose callers report errors, so its boundary must retain only a static category.
        let wire = WireRecord::deserialize(deserializer)
            .map_err(|_| de::Error::custom("invalid quality record encoding"))?;
        let record = Self {
            schema_version: wire.schema_version,
            coverage: wire.coverage,
            epochs: wire.epochs,
            spans: wire.spans,
            overflow: wire.overflow,
            activity: wire.activity,
        };
        record.validate().map_err(de::Error::custom)?;
        Ok(record)
    }
}

fn bounded_vec<'de, D, T, const LIMIT: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const LIMIT: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> Visitor<'de> for Bounded<T, LIMIT> {
        type Value = Vec<T>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "at most {LIMIT} quality entries")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut entries = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(LIMIT));
            while let Some(entry) = seq.next_element()? {
                if entries.len() == LIMIT {
                    return Err(de::Error::custom(QualityError::CapacityExceeded));
                }
                entries.push(entry);
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, LIMIT>(std::marker::PhantomData))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn epoch(id: u32) -> CaptureEpoch {
        CaptureEpoch {
            id,
            source_start_ns: Some(u64::from(id) * 1_000_000_000),
            source_rate_hz: 48_000,
            source_frame_base: u64::from(id) * 48_000,
            retained_start_frame: u64::from(id) * 48_000,
            retained_rate_hz: 48_000,
            mic_channels: 1,
            tap_channels: 1,
            reason: if id == 0 {
                CaptureEpochReason::Start
            } else {
                CaptureEpochReason::HostDiscontinuity
            },
        }
    }

    fn observed() -> QualityRecord {
        let mut record = QualityRecord::observed();
        record.insert_epoch(epoch(0)).unwrap();
        record
    }

    fn span(start_frame: u64, end_frame: u64, class: QualityClass) -> QualitySpan {
        QualitySpan {
            epoch: 0,
            start_frame,
            end_frame,
            track: QualityTrack::Both,
            cause: match class {
                QualityClass::SourceLoss => QualityCause::RingOverflow,
                QualityClass::ProcessingLoss => QualityCause::TeeDrop,
                QualityClass::DegradedOnly => QualityCause::ForcedBoundary,
            },
            class,
            stage: QualityStage::Live,
            resolution: QualityResolution::Outstanding,
        }
    }

    fn full_spans() -> QualityRecord {
        let mut record = observed();
        for index in 0..MAX_QUALITY_SPANS as u64 {
            record
                .insert_span(span(index * 4, index * 4 + 2, QualityClass::ProcessingLoss))
                .unwrap();
        }
        record
    }

    #[test]
    fn serde_shape_and_legacy_carriers_do_not_invent_clean_capture() {
        #[derive(Default, Deserialize)]
        struct Carrier {
            #[serde(default)]
            quality: Option<QualityRecord>,
        }
        let carrier: Carrier = serde_json::from_str("{}").unwrap();
        assert!(carrier.quality.is_none());
        assert_eq!(
            carrier.quality.unwrap_or_default().source_completeness(),
            SourceCompleteness::Unknown
        );
        let mut record = observed();
        record
            .insert_span(span(12, 48, QualityClass::SourceLoss))
            .unwrap();
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["coverage"], "observed");
        assert_eq!(value["epochs"][0]["reason"], "start");
        assert_eq!(value["spans"][0]["class"], "source_loss");
        assert_eq!(value["spans"][0]["cause"], "ring_overflow");
        assert_eq!(value["spans"][0]["track"], "both");
        assert_eq!(value["spans"][0]["resolution"], "outstanding");
        assert_eq!(value["overflow"]["omitted_spans"], json!([0, 0, 0]));
        assert_eq!(
            serde_json::from_value::<QualityRecord>(value).unwrap(),
            record
        );
    }

    #[test]
    fn activity_schema_one_wire_round_trip_preserves_summary_only_observations() {
        // Uses only the pre-existing record API so this same assertion exercises the old reader.
        let mut value = serde_json::to_value(observed()).unwrap();
        value["activity"] = json!({"live": {
            "input": {"identity": ([1; 32]), "retained_rate_hz": 48_000},
            "observed_tap": true,
            "nonzero_tap": true,
            "sustained_zero_tap": false,
            "absent_tap_layout": false,
            "suspected": false,
            "uncertain": true,
            "binding_conflict": false,
            "proof_conflict": false,
            "complete": null
        }});
        let decoded: QualityRecord = serde_json::from_value(value).unwrap();
        let output = serde_json::to_value(decoded).unwrap();
        assert_eq!(output["schema_version"], 1);
        assert_eq!(output["activity"]["live"]["nonzero_tap"], true);
        assert_eq!(output["activity"]["live"]["uncertain"], true);
        assert_eq!(output["spans"], json!([]));
    }

    #[test]
    fn activity_saturation_preserves_later_source_processing_and_generic_degraded_authority() {
        let mut activity = full_spans();
        activity
            .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        activity.overflow.omitted_spans[2] = u64::MAX;
        activity.overflow.outstanding_omitted_spans[2] = u64::MAX;
        let mut suspected = span(2_000, 2_020, QualityClass::DegradedOnly);
        suspected.cause = QualityCause::FarReferenceSilent;
        suspected.track = QualityTrack::Tap;
        assert!(!activity.insert_span(suspected).unwrap());
        assert!(activity.activity.omissions_saturated());
        assert!(!activity.overflow.counter_saturated);
        assert_eq!(activity.completeness(), SourceCompleteness::Complete);
        assert_eq!(activity.overflow.omitted_spans[2], u64::MAX);
        assert_eq!(activity.overflow.outstanding_omitted_spans[2], u64::MAX);

        let mut processing = activity.clone();
        processing
            .record_omitted_spans(
                QualityClass::ProcessingLoss,
                QualityResolution::Outstanding,
                1,
            )
            .unwrap();
        assert!(processing.requires_batch_recovery());
        assert_eq!(
            processing.source_completeness(),
            SourceCompleteness::Complete
        );
        assert_eq!(processing.completeness(), SourceCompleteness::Incomplete);
        processing
            .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        assert_eq!(processing.completeness(), SourceCompleteness::Complete);
        assert!(processing.activity.omissions_saturated());

        let mut source = activity.clone();
        source
            .record_omitted_spans(QualityClass::SourceLoss, QualityResolution::Outstanding, 1)
            .unwrap();
        source
            .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        assert_eq!(source.completeness(), SourceCompleteness::Incomplete);
        assert!(source.activity.omissions_saturated());

        let mut generic = activity.clone();
        generic
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                1,
            )
            .unwrap();
        assert!(generic.overflow.counter_saturated);
        assert_eq!(generic.source_completeness(), SourceCompleteness::Unknown);
        generic.merge_snapshot(&activity).unwrap();
        assert!(generic.overflow.counter_saturated && generic.activity.omissions_saturated());
        let wire = serde_json::to_vec(&generic).unwrap();
        assert_eq!(
            serde_json::from_slice::<QualityRecord>(&wire).unwrap(),
            generic
        );
    }

    #[test]
    fn activity_stage_sum_saturates_only_diagnostic_authority_and_preserves_serde_history() {
        let mut capture = observed();
        capture
            .observe_activity_omissions(
                crate::ActivityScope::Capture,
                crate::ActivityInput::new([1; 32], 48_000).unwrap(),
                u64::MAX,
                false,
            )
            .unwrap();
        let mut live = observed();
        live.observe_activity_omissions(
            crate::ActivityScope::Live,
            crate::ActivityInput::new([1; 32], 48_000).unwrap(),
            1,
            false,
        )
        .unwrap();
        capture.merge_snapshot(&live).unwrap();
        assert_eq!(capture.overflow.omitted_spans, [0, 0, u64::MAX]);
        assert!(capture.activity.omissions_saturated());
        assert!(!capture.overflow.counter_saturated);
        assert_eq!(capture.completeness(), SourceCompleteness::Complete);
        let frozen = capture.clone();
        capture.merge_snapshot(&live).unwrap();
        capture.merge_snapshot(&frozen).unwrap();
        assert_eq!(capture, frozen);
        let wire = serde_json::to_vec(&capture).unwrap();
        assert_eq!(
            serde_json::from_slice::<QualityRecord>(&wire).unwrap(),
            capture
        );
    }

    #[test]
    fn coalesces_adjacent_overlapping_and_bridging_intervals_before_cap() {
        let mut record = full_spans();
        assert_eq!(record.spans.len(), MAX_QUALITY_SPANS);
        assert!(
            record
                .insert_span(span(2, 8, QualityClass::ProcessingLoss))
                .unwrap()
        );
        assert_eq!(record.spans.len(), MAX_QUALITY_SPANS - 2);
        assert_eq!(record.spans[0].start_frame, 0);
        assert_eq!(record.spans[0].end_frame, 10);
        assert_eq!(record.overflow, QualityOverflow::default());
        record
            .insert_span(span(1, 9, QualityClass::ProcessingLoss))
            .unwrap();
        assert_eq!(record.spans.len(), MAX_QUALITY_SPANS - 2);
    }

    #[test]
    fn coalescing_requires_all_metadata_to_agree() {
        let original = span(48_000, 48_010, QualityClass::ProcessingLoss);
        for field in 0..7 {
            let mut spans = vec![original];
            let mut different = original;
            match field {
                0 => different.epoch = 1,
                1 => different.track = QualityTrack::Mic,
                2 => different.cause = QualityCause::DecodeFailure,
                3 => different.class = QualityClass::SourceLoss,
                4 => different.stage = QualityStage::Batch,
                5 => different.resolution = QualityResolution::RecoveredByBatch,
                6 => {
                    different.start_frame += 11;
                    different.end_frame += 11;
                }
                _ => unreachable!(),
            }
            let different = coalesce(&mut spans, different);
            spans.push(different);
            assert_eq!(spans.len(), 2, "metadata field {field}");
        }
    }

    #[test]
    fn span_cap_retains_each_class_and_recovery_status() {
        let mut record = full_spans();
        for class in [
            QualityClass::SourceLoss,
            QualityClass::ProcessingLoss,
            QualityClass::DegradedOnly,
        ] {
            assert!(!record.insert_span(span(2_000, 2_010, class)).unwrap());
        }
        let mut recovered = span(2_020, 2_030, QualityClass::ProcessingLoss);
        recovered.resolution = QualityResolution::RecoveredByBatch;
        assert!(!record.insert_span(recovered).unwrap());
        assert_eq!(record.spans.len(), MAX_QUALITY_SPANS);
        assert_eq!(record.overflow.omitted_spans, [1, 2, 1]);
        assert_eq!(record.overflow.outstanding_omitted_spans, [1, 1, 1]);
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn epoch_cap_and_saturation_prevent_exact_completeness() {
        let mut record = observed();
        for id in 1..MAX_QUALITY_EPOCHS as u32 {
            assert!(record.insert_epoch(epoch(id)).unwrap());
        }
        assert!(!record.insert_epoch(epoch(64)).unwrap());
        assert!(!record.insert_epoch(epoch(65)).unwrap());
        assert_eq!(record.epochs.len(), MAX_QUALITY_EPOCHS);
        assert_eq!(record.overflow.omitted_epochs, 2);
        assert_eq!(record.source_completeness(), SourceCompleteness::Unknown);
        record.record_omitted_epochs(u64::MAX);
        assert_eq!(record.overflow.omitted_epochs, u64::MAX);
        assert!(record.overflow.counter_saturated);
        record
            .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        assert_eq!(record.completeness(), SourceCompleteness::Unknown);

        let mut record = observed();
        record
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                u64::MAX,
            )
            .unwrap();
        assert!(!record.overflow.counter_saturated);
        record
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                1,
            )
            .unwrap();
        assert_eq!(record.overflow.omitted_spans[2], u64::MAX);
        assert!(record.overflow.counter_saturated);
        assert_eq!(record.completeness(), SourceCompleteness::Unknown);
    }

    #[test]
    fn batch_recovers_only_processing_and_keeps_every_source_fact() {
        let mut record = observed();
        for class in [
            QualityClass::SourceLoss,
            QualityClass::ProcessingLoss,
            QualityClass::DegradedOnly,
        ] {
            record.insert_span(span(0, 10, class)).unwrap();
            record
                .record_omitted_spans(class, QualityResolution::Outstanding, 4)
                .unwrap();
        }
        for outcome in [
            BatchRecoveryOutcome::Failed,
            BatchRecoveryOutcome::Partial,
            BatchRecoveryOutcome::DifferentBackend,
            BatchRecoveryOutcome::UnknownBackend,
        ] {
            let previous = record.clone();
            assert!(!record.resolve_processing_after_batch(outcome).unwrap());
            assert_eq!(record, previous);
        }
        assert!(record.requires_batch_recovery());
        assert!(
            record
                .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
                .unwrap()
        );
        assert!(!record.requires_batch_recovery());
        assert_eq!(record.spans.len(), 3);
        assert_eq!(record.overflow.omitted_spans, [4, 4, 4]);
        assert_eq!(record.overflow.outstanding_omitted_spans, [4, 0, 4]);
        assert!(record.spans.iter().all(|span| {
            (span.resolution == QualityResolution::RecoveredByBatch)
                == (span.class == QualityClass::ProcessingLoss)
        }));
        let encoded = serde_json::to_vec(&record).unwrap();
        let mut redo: QualityRecord = serde_json::from_slice(&encoded).unwrap();
        redo.resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        assert_eq!(redo, record);
        assert_eq!(redo.completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn explicit_local_batch_recovers_only_processing_without_certifying_source_uncertainty() {
        let mut captured = observed();
        captured
            .insert_span(span(0, 10, QualityClass::ProcessingLoss))
            .unwrap();
        captured
            .record_omitted_spans(
                QualityClass::ProcessingLoss,
                QualityResolution::Outstanding,
                3,
            )
            .unwrap();
        for outcome in [
            BatchRecoveryOutcome::Failed,
            BatchRecoveryOutcome::Partial,
            BatchRecoveryOutcome::UnknownBackend,
            BatchRecoveryOutcome::DifferentBackend,
        ] {
            let mut failed = captured.clone();
            assert!(!failed.resolve_processing_after_batch(outcome).unwrap());
            assert_eq!(failed, captured);
        }
        let mut recovered = captured.clone();
        assert!(
            recovered
                .resolve_processing_after_batch(BatchRecoveryOutcome::ExplicitLocalWholeFileSuccess)
                .unwrap()
        );
        assert_eq!(recovered.completeness(), SourceCompleteness::Complete);
        assert_eq!(recovered.overflow.omitted_spans, [0, 3, 0]);
        assert_eq!(recovered.overflow.outstanding_omitted_spans, [0; 3]);
        assert_eq!(
            recovered.spans[0].resolution,
            QualityResolution::RecoveredByBatch
        );

        let mut uncertain = Vec::new();
        let mut source = captured.clone();
        source
            .insert_span(span(20, 30, QualityClass::SourceLoss))
            .unwrap();
        uncertain.push((source, SourceCompleteness::Incomplete));
        let mut legacy = captured.clone();
        legacy.coverage = QualityCoverage::LegacyUnknown;
        uncertain.push((legacy, SourceCompleteness::Unknown));
        let mut unmapped = captured.clone();
        unmapped.epochs[0].source_start_ns = None;
        uncertain.push((unmapped, SourceCompleteness::Unknown));
        let mut omitted = captured.clone();
        omitted.record_omitted_epochs(1);
        uncertain.push((omitted, SourceCompleteness::Unknown));
        let mut saturated = captured.clone();
        saturated.overflow.counter_saturated = true;
        uncertain.push((saturated, SourceCompleteness::Unknown));
        for (mut quality, expected) in uncertain {
            let source_spans: Vec<_> = quality
                .spans
                .iter()
                .copied()
                .filter(|span| span.class == QualityClass::SourceLoss)
                .collect();
            let coverage = quality.coverage;
            let epochs = quality.epochs.clone();
            let overflow = quality.overflow.clone();
            quality
                .resolve_processing_after_batch(BatchRecoveryOutcome::ExplicitLocalWholeFileSuccess)
                .unwrap();
            assert!(!quality.requires_batch_recovery());
            assert_eq!(quality.completeness(), expected);
            assert_eq!(quality.coverage, coverage);
            assert_eq!(quality.epochs, epochs);
            assert_eq!(quality.overflow.omitted_spans, overflow.omitted_spans);
            assert_eq!(quality.overflow.omitted_epochs, overflow.omitted_epochs);
            assert_eq!(
                quality.overflow.counter_saturated,
                overflow.counter_saturated
            );
            assert_eq!(
                quality
                    .spans
                    .iter()
                    .copied()
                    .filter(|span| span.class == QualityClass::SourceLoss)
                    .collect::<Vec<_>>(),
                source_spans
            );
        }
    }

    #[test]
    fn tee_only_recovery_completes_but_degraded_alone_never_needs_recovery() {
        let mut record = observed();
        record
            .insert_span(span(0, 10, QualityClass::DegradedOnly))
            .unwrap();
        record
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                12,
            )
            .unwrap();
        assert_eq!(record.completeness(), SourceCompleteness::Complete);
        assert!(!record.requires_batch_recovery());
        record
            .insert_span(span(0, 10, QualityClass::ProcessingLoss))
            .unwrap();
        record
            .record_omitted_spans(
                QualityClass::ProcessingLoss,
                QualityResolution::Outstanding,
                3,
            )
            .unwrap();
        assert_eq!(record.source_completeness(), SourceCompleteness::Complete);
        assert_eq!(record.completeness(), SourceCompleteness::Incomplete);
        record
            .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        assert_eq!(record.completeness(), SourceCompleteness::Complete);
    }

    #[test]
    fn known_source_loss_dominates_unknown_coverage_and_omissions() {
        let mut record = QualityRecord::default();
        record.record_omitted_epochs(1);
        assert_eq!(record.completeness(), SourceCompleteness::Unknown);
        record
            .record_omitted_spans(QualityClass::SourceLoss, QualityResolution::Outstanding, 1)
            .unwrap();
        assert_eq!(record.completeness(), SourceCompleteness::Incomplete);
        record
            .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        assert_eq!(record.completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn source_offsets_and_fixed_rate_retained_bases_are_distinct_checked_coordinates() {
        let epoch = CaptureEpoch {
            source_start_ns: Some(120_000_000_000),
            source_frame_base: 96_000,
            retained_start_frame: 48_000,
            source_rate_hz: 44_100,
            retained_rate_hz: 48_000,
            ..epoch(1)
        };
        assert_eq!(
            epoch.source_time_ns(140_100).unwrap(),
            Some(121_000_000_000)
        );
        assert_eq!(epoch.retained_frame(140_100).unwrap(), 96_000);
        assert_eq!(epoch.retained_frame(96_001).unwrap(), 48_001);
        assert_eq!(epoch.retained_frame(96_010).unwrap(), 48_010);
        assert_eq!(epoch.retained_frame(96_441).unwrap(), 48_480);
        assert_eq!(
            epoch.retained_frame(95_999),
            Err(QualityError::InvalidInterval)
        );
        let mut unknown = epoch;
        unknown.source_start_ns = None;
        assert_eq!(unknown.source_time_ns(140_100).unwrap(), None);
        let mut overflowing = epoch;
        overflowing.source_start_ns = Some(u64::MAX);
        assert_eq!(
            overflowing.source_time_ns(96_001),
            Err(QualityError::ArithmeticOverflow)
        );
        overflowing.retained_start_frame = u64::MAX;
        assert_eq!(
            overflowing.retained_frame(96_001),
            Err(QualityError::ArithmeticOverflow)
        );
    }

    #[test]
    fn source_loss_mask_clips_unfilled_epoch_gaps_without_masking_later_pcm() {
        let mut record = observed();
        record
            .insert_span(span(8, 12, QualityClass::SourceLoss))
            .unwrap();
        let mut unfilled = span(20, 4_800_000, QualityClass::SourceLoss);
        unfilled.cause = QualityCause::HostDiscontinuity;
        record.insert_span(unfilled).unwrap();
        let mut next = epoch(1);
        next.source_start_ns = Some(100_000_000_000);
        next.source_frame_base = 4_800_000;
        next.retained_start_frame = 20;
        record.insert_epoch(next).unwrap();
        let mut later = span(4_800_004, 4_800_008, QualityClass::SourceLoss);
        later.epoch = 1;
        later.track = QualityTrack::Tap;
        record.insert_span(later).unwrap();
        assert_eq!(
            record.retained_source_loss_intervals(30).unwrap(),
            vec![(8, 12, QualityTrack::Both), (24, 28, QualityTrack::Tap)]
        );
        assert_eq!(
            record.retained_source_loss_intervals(26).unwrap(),
            vec![(8, 12, QualityTrack::Both), (24, 26, QualityTrack::Tap)]
        );
        assert_eq!(
            record.retained_source_loss_intervals(10).unwrap(),
            vec![(8, 10, QualityTrack::Both)]
        );
    }

    #[test]
    fn source_loss_mask_refuses_an_omitted_next_epoch_boundary() {
        let mut record = observed();
        let mut gap = span(20, 4_800_000, QualityClass::SourceLoss);
        gap.cause = QualityCause::HostDiscontinuity;
        record.insert_span(gap).unwrap();
        record.record_omitted_epochs(1);
        assert_eq!(
            record.retained_source_loss_intervals(30),
            Err(QualityError::UnknownEpoch)
        );
        // A prefix ending before that unavailable interval requires no speculative masking.
        assert!(
            record
                .retained_source_loss_intervals(20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn source_loss_mask_refuses_omitted_source_spans_and_saturated_accounting() {
        let mut omitted = observed();
        for index in 0..=MAX_QUALITY_SPANS as u64 {
            omitted
                .insert_span(span(index * 4, index * 4 + 2, QualityClass::SourceLoss))
                .unwrap();
        }
        assert_eq!(omitted.spans.len(), MAX_QUALITY_SPANS);
        assert_eq!(omitted.overflow.omitted_spans, [1, 0, 0]);
        for coverage in [QualityCoverage::Observed, QualityCoverage::LegacyUnknown] {
            omitted.coverage = coverage;
            omitted.validate().unwrap();
            assert_eq!(
                omitted.retained_source_loss_intervals(2_000),
                Err(QualityError::UnknownAvailability)
            );
        }

        let mut saturated = observed();
        saturated
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                u64::MAX,
            )
            .unwrap();
        saturated
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                1,
            )
            .unwrap();
        assert!(saturated.overflow.counter_saturated);
        saturated.validate().unwrap();
        assert_eq!(
            saturated.retained_source_loss_intervals(2_000),
            Err(QualityError::UnknownAvailability)
        );
    }

    #[test]
    fn source_loss_mask_does_not_infer_loss_from_legacy_or_processing_omissions() {
        let mut record = observed();
        record.coverage = QualityCoverage::LegacyUnknown;
        record
            .insert_span(span(8, 12, QualityClass::SourceLoss))
            .unwrap();
        for class in [QualityClass::ProcessingLoss, QualityClass::DegradedOnly] {
            record
                .record_omitted_spans(class, QualityResolution::Outstanding, 1)
                .unwrap();
        }
        assert_eq!(
            record.retained_source_loss_intervals(30).unwrap(),
            vec![(8, 12, QualityTrack::Both)]
        );
    }

    #[test]
    fn merging_is_idempotent_and_cannot_authorize_processing_recovery() {
        let mut record = full_spans();
        record
            .record_omitted_spans(
                QualityClass::ProcessingLoss,
                QualityResolution::Outstanding,
                9,
            )
            .unwrap();
        let original = record.clone();
        record.merge_snapshot(&original).unwrap();
        assert_eq!(record, original);

        let mut recovered = record.clone();
        recovered
            .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        record.merge_snapshot(&recovered).unwrap();
        assert!(record.requires_batch_recovery());
        let merged = record.clone();
        record.merge_snapshot(&recovered).unwrap();
        assert_eq!(record, merged);
        assert_eq!(record.overflow.outstanding_omitted_spans[1], 128);
        record
            .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        assert_eq!(record.completeness(), SourceCompleteness::Complete);
    }

    #[test]
    fn merging_legacy_never_manufactures_observation_and_conflicts_leave_inputs_intact() {
        let mut record = observed();
        record.merge_snapshot(&QualityRecord::default()).unwrap();
        assert_eq!(record.completeness(), SourceCompleteness::Unknown);
        let mut conflicting = observed();
        conflicting.epochs[0].tap_channels = 2;
        let previous = record.clone();
        let other_previous = conflicting.clone();
        assert_eq!(
            record.merge_snapshot(&conflicting),
            Err(QualityError::ConflictingEpoch)
        );
        assert_eq!(record, previous);
        assert_eq!(conflicting, other_previous);
    }

    #[test]
    fn divergent_capped_snapshots_preserve_source_overflow_on_repeated_merge() {
        let mut left = full_spans();
        let mut right = observed();
        right
            .insert_span(span(5_000, 6_000, QualityClass::SourceLoss))
            .unwrap();
        left.merge_snapshot(&right).unwrap();
        assert_eq!(left.overflow.omitted_spans[0], 1);
        assert_eq!(left.source_completeness(), SourceCompleteness::Incomplete);
        let previous = left.clone();
        left.merge_snapshot(&right).unwrap();
        assert_eq!(left, previous);
    }

    #[test]
    fn malformed_and_oversized_records_are_rejected_on_both_serde_paths() {
        let valid = serde_json::to_value(observed()).unwrap();
        let invalid = |edit: fn(&mut Value)| {
            let mut value = valid.clone();
            edit(&mut value);
            assert!(serde_json::from_value::<QualityRecord>(value).is_err());
        };
        invalid(|value| value["schema_version"] = json!(2));
        invalid(|value| value["coverage"] = json!("clean"));
        invalid(|value| value["epochs"][0]["source_rate_hz"] = json!(0));
        invalid(|value| value["epochs"][0]["retained_rate_hz"] = json!(0));
        invalid(|value| value["epochs"][0]["mic_channels"] = json!(u32::MAX));
        invalid(|value| value["overflow"]["outstanding_omitted_spans"] = json!([0, 1, 0]));
        invalid(|value| value["overflow"]["omitted_spans"] = json!([1, 0, 0]));
        invalid(|value| {
            value["epochs"] = json!(
                (0..=MAX_QUALITY_EPOCHS as u32)
                    .map(epoch)
                    .collect::<Vec<_>>()
            );
        });
        invalid(|value| {
            value["spans"] = json!(vec![
                span(0, 1, QualityClass::SourceLoss);
                MAX_QUALITY_SPANS + 1
            ]);
        });
        let mut oversized = observed();
        oversized.spans = vec![span(0, 1, QualityClass::SourceLoss); MAX_QUALITY_SPANS + 1];
        assert!(serde_json::to_vec(&oversized).is_err());
        oversized.spans.clear();
        oversized.epochs = (0..=MAX_QUALITY_EPOCHS as u32).map(epoch).collect();
        assert!(serde_json::to_vec(&oversized).is_err());
        assert_eq!(oversized.completeness(), SourceCompleteness::Unknown);
    }

    #[test]
    fn invalid_spans_and_epoch_reuse_fail_without_mutating_history() {
        let mut record = observed();
        let previous = record.clone();
        for invalid in [
            span(10, 10, QualityClass::SourceLoss),
            span(20, 10, QualityClass::SourceLoss),
            QualitySpan {
                epoch: 1,
                ..span(0, 10, QualityClass::SourceLoss)
            },
            QualitySpan {
                resolution: QualityResolution::RecoveredByBatch,
                ..span(0, 10, QualityClass::SourceLoss)
            },
        ] {
            assert!(record.insert_span(invalid).is_err());
            assert_eq!(record, previous);
        }
        let mut reused = epoch(0);
        reused.source_rate_hz = 44_100;
        assert_eq!(
            record.insert_epoch(reused),
            Err(QualityError::ConflictingEpoch)
        );
        assert_eq!(record, previous);
        let mut rate_change = epoch(1);
        rate_change.reason = CaptureEpochReason::RateChange;
        rate_change.source_rate_hz = 44_100;
        assert!(record.insert_epoch(rate_change).unwrap());
        let mut bad_output_rate = epoch(2);
        bad_output_rate.retained_rate_hz = 44_100;
        assert_eq!(
            record.insert_epoch(bad_output_rate),
            Err(QualityError::InvalidEpochOrder)
        );
        let mut layout_change = epoch(2);
        layout_change.reason = CaptureEpochReason::LayoutChange;
        layout_change.tap_channels = 2;
        assert!(record.insert_epoch(layout_change).unwrap());
        assert_eq!(record.epochs.len(), 3);
    }

    #[test]
    fn missing_offsets_and_invalid_classifications_cannot_certify_complete() {
        let mut record = observed();
        record.epochs[0].source_start_ns = None;
        assert_eq!(record.source_completeness(), SourceCompleteness::Unknown);
        let value = serde_json::to_value(&record).unwrap();
        assert_eq!(value["epochs"][0]["source_start_ns"], Value::Null);
        let decoded: QualityRecord = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.source_completeness(), SourceCompleteness::Unknown);
        let mut invalid = span(0, 10, QualityClass::SourceLoss);
        invalid.class = QualityClass::ProcessingLoss;
        assert_eq!(
            record.insert_span(invalid),
            Err(QualityError::InvalidClassification)
        );
        let mut capture_failure = span(0, 10, QualityClass::SourceLoss);
        capture_failure.cause = QualityCause::ResamplerFailure;
        capture_failure.stage = QualityStage::Capture;
        record.insert_span(capture_failure).unwrap();
        record
            .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn forged_source_recovery_is_rejected_and_cannot_hide_known_loss() {
        let mut record = observed();
        record
            .insert_span(span(0, 10, QualityClass::SourceLoss))
            .unwrap();
        record.spans[0].resolution = QualityResolution::RecoveredByBatch;
        assert_eq!(record.validate(), Err(QualityError::InvalidResolution));
        assert!(serde_json::to_vec(&record).is_err());
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
        record.spans.clear();
        record.overflow.omitted_spans[0] = 1;
        assert_eq!(record.validate(), Err(QualityError::InvalidOverflow));
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn malformed_quality_errors_never_echo_payload_values() {
        let original = serde_json::to_value(observed()).unwrap();
        for field in ["coverage", "schema_version", "epochs", "spans", "overflow"] {
            let mut malformed = original.clone();
            malformed[field] = serde_json::json!("SYNTHETIC_QUALITY_SENTINEL");
            let error = serde_json::from_value::<QualityRecord>(malformed).unwrap_err();
            assert!(
                !format!("{error:?}: {error}").contains("SYNTHETIC_QUALITY_SENTINEL"),
                "malformed {field} leaked its payload"
            );
        }
    }

    #[test]
    fn epoch_union_cap_keeps_missing_epoch_source_losses_in_overflow() {
        let mut left = observed();
        for id in 1..MAX_QUALITY_EPOCHS as u32 {
            left.insert_epoch(epoch(id)).unwrap();
        }
        let mut right = observed();
        right.insert_epoch(epoch(64)).unwrap();
        let mut loss = span(64 * 48_000, 64 * 48_000 + 48, QualityClass::SourceLoss);
        loss.epoch = 64;
        right.insert_span(loss).unwrap();
        left.merge_snapshot(&right).unwrap();
        assert_eq!(left.overflow.omitted_epochs, 1);
        assert_eq!(left.overflow.omitted_spans[0], 1);
        assert_eq!(left.source_completeness(), SourceCompleteness::Incomplete);
        let previous = left.clone();
        left.merge_snapshot(&right).unwrap();
        assert_eq!(left, previous);
        left.resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
            .unwrap();
        assert_eq!(left.source_completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn spans_cannot_cross_a_known_source_epoch_boundary() {
        let mut record = observed();
        record.insert_epoch(epoch(1)).unwrap();
        record
            .insert_span(span(0, 48_000, QualityClass::SourceLoss))
            .unwrap();
        let previous = record.clone();
        assert_eq!(
            record.insert_span(span(0, 48_001, QualityClass::SourceLoss)),
            Err(QualityError::InvalidInterval)
        );
        assert_eq!(record, previous);

        let mut record = observed();
        record
            .insert_span(span(0, 96_000, QualityClass::SourceLoss))
            .unwrap();
        let previous = record.clone();
        assert_eq!(
            record.insert_epoch(epoch(1)),
            Err(QualityError::InvalidInterval)
        );
        assert_eq!(record, previous);
        let mut next = epoch(2);
        next.retained_start_frame = 48;
        record.insert_epoch(next).unwrap();
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn epoch_boundaries_allow_nanosecond_quantization_at_fractional_rates() {
        let mut record = QualityRecord::observed();
        let first = CaptureEpoch {
            source_rate_hz: 44_100,
            ..epoch(0)
        };
        record.insert_epoch(first).unwrap();
        record
            .insert_span(span(0, 1, QualityClass::SourceLoss))
            .unwrap();
        record
            .insert_epoch(CaptureEpoch {
                source_start_ns: first.source_time_ns(1).unwrap(),
                source_frame_base: 1,
                retained_start_frame: 1,
                source_rate_hz: 44_100,
                ..epoch(1)
            })
            .unwrap();
        assert_eq!(record.epochs[1].source_start_ns, Some(22_675));
        assert_eq!(
            record.insert_span(span(0, 2, QualityClass::SourceLoss)),
            Err(QualityError::InvalidInterval)
        );
    }
}
