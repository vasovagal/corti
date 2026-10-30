//! Bounded, transient decode facts. No words, audio or backend error strings belong in these types.
//! Input frames count all samples supplied to one channel, cumulatively across input-rate changes.
//! They are neither VAD-rate frames nor retained-WAV positions; received chunks bind those clocks.

use corti_core::quality_mapping::insert_retained_forced_boundary;
use corti_core::{
    CaptureEpoch, MAX_QUALITY_SPANS, QualityCause, QualityClass, QualityError, QualityRecord,
    QualityResolution, QualityStage, QualityTrack, insert_retained_processing_loss,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputSpan {
    pub start_frame: u64,
    pub end_frame: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeErrorCategory {
    NoResult,
    BackendFailure,
    Aborted,
    OutputTruncated,
    UnmappableResult,
    ResamplerUnavailable,
}

impl DecodeErrorCategory {
    pub fn quality_cause(self) -> QualityCause {
        match self {
            Self::NoResult => QualityCause::NoResult,
            Self::ResamplerUnavailable => QualityCause::ResamplerFailure,
            Self::BackendFailure
            | Self::Aborted
            | Self::OutputTruncated
            | Self::UnmappableResult => QualityCause::DecodeFailure,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionStatus {
    Complete,
    RecognizedEmpty,
    Failed(DecodeErrorCategory),
    Partial(DecodeErrorCategory),
}

impl RegionStatus {
    pub fn is_complete(self) -> bool {
        matches!(self, Self::Complete | Self::RecognizedEmpty)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionOutcome {
    pub span: InputSpan,
    pub status: RegionStatus,
}

/// A drainable report, separate from words. Success saturation records metadata degradation without
/// changing completeness or hiding a later failure: a failure replaces a stored success, or increments
/// the failure overflow count. Metadata and failure counter saturation have separate semantics.
/// Channel owners retain their own sticky completeness flag when they drain this report.
#[derive(Debug, Clone, Default)]
pub struct DecodeReport {
    outcomes: Vec<RegionOutcome>,
    omitted_failures: u64,
    counter_saturated: bool,
    omitted_metadata: u64,
    metadata_counter_saturated: bool,
}

impl DecodeReport {
    pub fn outcomes(&self) -> &[RegionOutcome] {
        &self.outcomes
    }

    pub fn omitted_failures(&self) -> u64 {
        self.omitted_failures
    }

    pub fn counter_saturated(&self) -> bool {
        self.counter_saturated || self.metadata_counter_saturated
    }

    pub fn omitted_metadata(&self) -> u64 {
        self.omitted_metadata
    }

    pub fn metadata_counter_saturated(&self) -> bool {
        self.metadata_counter_saturated
    }

    pub fn is_complete(&self) -> bool {
        self.omitted_failures == 0
            && !self.counter_saturated
            && self
                .outcomes
                .iter()
                .all(|outcome| outcome.status.is_complete())
    }

    pub fn record(&mut self, outcome: RegionOutcome) {
        if outcome.span.start_frame >= outcome.span.end_frame {
            self.omit_failure();
            return;
        }
        if self.outcomes.len() < MAX_QUALITY_SPANS {
            self.outcomes.push(outcome);
        } else if !outcome.status.is_complete() {
            if let Some(index) = self
                .outcomes
                .iter()
                .position(|saved| saved.status.is_complete())
            {
                self.outcomes.remove(index);
                self.omit_metadata();
                self.outcomes.push(outcome);
            } else {
                self.omit_failure();
            }
        } else {
            self.omit_metadata();
        }
    }

    /// A dropped successful report or validated clock-history entry is metadata loss, not failed ASR.
    pub fn omit_metadata(&mut self) {
        add_count(
            &mut self.omitted_metadata,
            1,
            &mut self.metadata_counter_saturated,
        );
    }

    pub fn omit_failure(&mut self) {
        add_count(&mut self.omitted_failures, 1, &mut self.counter_saturated);
    }

    pub fn merge(&mut self, other: Self) {
        for outcome in other.outcomes {
            self.record(outcome);
        }
        add_count(
            &mut self.omitted_failures,
            other.omitted_failures,
            &mut self.counter_saturated,
        );
        self.counter_saturated |= other.counter_saturated;
        add_count(
            &mut self.omitted_metadata,
            other.omitted_metadata,
            &mut self.metadata_counter_saturated,
        );
        self.metadata_counter_saturated |= other.metadata_counter_saturated;
    }
}

fn add_count(count: &mut u64, amount: u64, saturated: &mut bool) {
    match count.checked_add(amount) {
        Some(total) => *count = total,
        None => {
            *count = u64::MAX;
            *saturated = true;
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ReceivedRun {
    input: InputSpan,
    epoch: CaptureEpoch,
    retained_start: u64,
}

/// One channel's exact received-input mapping, bounded to 256 coalesced runs. Missing runs are gaps,
/// never permission to extrapolate. Each dropped nonempty run retains a degraded-only omission
/// count. This is transient mapping, not another persisted quality model.
#[derive(Debug, Default)]
pub struct ReceivedAudioMap {
    frames: u64,
    runs: Vec<ReceivedRun>,
    clock_overflowed: bool,
    omitted_runs: u64,
    counter_saturated: bool,
}

impl ReceivedAudioMap {
    pub fn input_frames(&self) -> u64 {
        self.frames
    }

    /// Call before AEC buffering, once per received channel interval (including aligned padding).
    pub fn observe_input(
        &mut self,
        epoch: CaptureEpoch,
        retained_start: u64,
        len: u64,
    ) -> InputSpan {
        let input = InputSpan {
            start_frame: self.frames,
            end_frame: self.frames.saturating_add(len),
        };
        let overflowed = self.frames.checked_add(len).is_none();
        self.frames = input.end_frame;
        if len == 0 {
            return input;
        }
        if self.clock_overflowed || overflowed {
            self.clock_overflowed = true;
            self.omit_run();
            return input;
        }
        if retained_start < epoch.retained_start_frame
            || retained_start.checked_add(len).is_none()
            || epoch.retained_rate_hz == 0
            || epoch.source_rate_hz == 0
        {
            self.omit_run();
            return input;
        }
        if let Some(previous) = self.runs.last_mut() {
            let previous_end =
                previous.retained_start + (previous.input.end_frame - previous.input.start_frame);
            if retained_start < previous_end {
                self.omit_run();
                return input;
            }
            if previous.epoch == epoch
                && previous.input.end_frame == input.start_frame
                && previous_end == retained_start
            {
                previous.input.end_frame = input.end_frame;
                return input;
            }
        }
        if self.runs.len() < MAX_QUALITY_SPANS {
            self.runs.push(ReceivedRun {
                input,
                epoch,
                retained_start,
            });
        } else {
            self.omit_run();
        }
        input
    }

    fn omit_run(&mut self) {
        add_count(&mut self.omitted_runs, 1, &mut self.counter_saturated);
    }
}

#[derive(Debug, Clone, Copy)]
struct RetainedFact {
    epoch: Option<CaptureEpoch>,
    start: u64,
    end: u64,
    track: QualityTrack,
    cause: QualityCause,
    class: QualityClass,
}

impl RetainedFact {
    fn class(self) -> QualityClass {
        self.class
    }
}

fn fact_class(cause: QualityCause) -> QualityClass {
    if cause == QualityCause::ForcedBoundary {
        QualityClass::DegradedOnly
    } else {
        QualityClass::ProcessingLoss
    }
}

/// Keeps bounded processing-loss and boundary facts until Finish capture authority is available. A
/// first received epoch greater than zero stays exactly bound even though a provisional record
/// cannot describe it without the missing Start epoch. Only `settle` creates durable quality spans.
#[derive(Debug, Default)]
pub struct DecodeQualityTracker {
    mic: ReceivedAudioMap,
    tap: ReceivedAudioMap,
    facts: Vec<RetainedFact>,
    omitted_failures: u64,
    omitted_metadata: u64,
    counter_saturated: bool,
}

impl DecodeQualityTracker {
    /// Mark the last received input frame before an established nonempty cut: `[cut - 1, cut)`.
    /// This is a boundary location, not the extent of affected speech or lost audio. `input_frame`
    /// uses this track's cumulative input clock, never the common app clock without origin mapping.
    /// Zero means no owned input and records nothing. Use each track separately with unequal origins;
    /// `Both` is valid only for equal local input cuts. Call once when selecting a forced fence;
    /// repeated settlement uses the same original capture authority and preserves the marker.
    pub fn mark_forced_boundary(&mut self, track: QualityTrack, input_frame: u64) {
        if let Some(start_frame) = input_frame.checked_sub(1) {
            self.record_input_fact(
                track,
                InputSpan {
                    start_frame,
                    end_frame: input_frame,
                },
                QualityCause::ForcedBoundary,
            );
        }
    }

    pub fn input_frames(&self, track: QualityTrack) -> u64 {
        match track {
            QualityTrack::Mic => self.mic.input_frames(),
            QualityTrack::Tap => self.tap.input_frames(),
            QualityTrack::Both => self.mic.input_frames().max(self.tap.input_frames()),
        }
    }

    /// Record each track independently; Both is suitable only when their input clocks coincide.
    pub fn observe_input(
        &mut self,
        track: QualityTrack,
        epoch: CaptureEpoch,
        retained_start: u64,
        len: u64,
    ) -> InputSpan {
        match track {
            QualityTrack::Mic => self.mic.observe_input(epoch, retained_start, len),
            QualityTrack::Tap => self.tap.observe_input(epoch, retained_start, len),
            QualityTrack::Both => {
                self.mic.observe_input(epoch, retained_start, len);
                self.tap.observe_input(epoch, retained_start, len)
            }
        }
    }

    pub fn record_report(&mut self, track: QualityTrack, report: DecodeReport) {
        add_count(
            &mut self.omitted_failures,
            report.omitted_failures,
            &mut self.counter_saturated,
        );
        add_count(
            &mut self.omitted_metadata,
            report.omitted_metadata,
            &mut self.counter_saturated,
        );
        self.counter_saturated |= report.counter_saturated();
        for outcome in report.outcomes {
            match outcome.status {
                RegionStatus::Complete | RegionStatus::RecognizedEmpty => {}
                RegionStatus::Failed(category) | RegionStatus::Partial(category) => {
                    self.record_input_fact(track, outcome.span, category.quality_cause());
                }
            }
        }
    }

    /// A failed append loses the actual per-track input interval belonging to that window. Cache
    /// exhaustion after a successful append is not a window loss and must not call this method.
    pub fn record_window_not_filed(&mut self, track: QualityTrack, span: InputSpan) {
        if span.start_frame == span.end_frame {
            return;
        }
        self.record_input_fact(track, span, QualityCause::WindowNotFiled);
    }

    /// For callers already holding a WAV interval. Unlike logical-input failures, no received-run
    /// binding is required; settlement splits it against the final authoritative capture epochs.
    pub fn record_retained_processing_loss(
        &mut self,
        track: QualityTrack,
        start: u64,
        end: u64,
        cause: QualityCause,
    ) {
        self.remember_fact(RetainedFact {
            epoch: None,
            start,
            end,
            track,
            cause,
            class: QualityClass::ProcessingLoss,
        });
    }

    fn record_input_fact(&mut self, track: QualityTrack, span: InputSpan, cause: QualityCause) {
        if track == QualityTrack::Both {
            self.record_input_fact(QualityTrack::Mic, span, cause);
            self.record_input_fact(QualityTrack::Tap, span, cause);
            return;
        }
        if span.start_frame >= span.end_frame {
            self.omit_fact(fact_class(cause));
            return;
        }
        let map = if track == QualityTrack::Mic {
            &self.mic
        } else {
            &self.tap
        };
        // Bounded copy lets us retain the facts while walking the immutable mapping inventory.
        let runs = map.runs.clone();
        let mut cursor = span.start_frame;
        for run in runs {
            let start = cursor.max(run.input.start_frame);
            let end = span.end_frame.min(run.input.end_frame);
            if start >= end {
                continue;
            }
            if cursor < start {
                self.omit_fact(fact_class(cause));
            }
            self.remember_fact(RetainedFact {
                epoch: Some(run.epoch),
                start: run.retained_start + (start - run.input.start_frame),
                end: run.retained_start + (end - run.input.start_frame),
                track,
                cause,
                class: fact_class(cause),
            });
            cursor = end;
            if cursor == span.end_frame {
                break;
            }
        }
        if cursor < span.end_frame {
            self.omit_fact(fact_class(cause));
        }
    }

    fn omit_failure(&mut self) {
        add_count(&mut self.omitted_failures, 1, &mut self.counter_saturated);
    }

    fn omit_fact(&mut self, class: QualityClass) {
        if class == QualityClass::DegradedOnly {
            add_count(&mut self.omitted_metadata, 1, &mut self.counter_saturated);
        } else {
            self.omit_failure();
        }
    }

    fn remember_fact(&mut self, failure: RetainedFact) {
        if failure.start >= failure.end {
            self.omit_fact(failure.class());
            return;
        }
        if let Some(previous) = self.facts.last_mut()
            && previous.epoch == failure.epoch
            && previous.track == failure.track
            && previous.cause == failure.cause
            && previous.class == failure.class
            && previous.end == failure.start
        {
            previous.end = failure.end;
        } else if self.facts.len() < MAX_QUALITY_SPANS {
            self.facts.push(failure);
        } else {
            self.omit_fact(failure.class());
        }
    }

    /// Append this tracker's facts once to the original capture authority. Repeating settlement
    /// with that same authority is idempotent; an already-settled result is not the capture baseline.
    pub fn settle(&self, capture: &QualityRecord) -> Result<QualityRecord, QualityError> {
        capture.validate()?;
        let mut quality = capture.clone();
        let (omitted_metadata, metadata_counter_saturated) = self.metadata_omissions();
        // Dropped successful outcomes and clock/mapping bookkeeping do not mean that decoding failed.
        // An actual failure in an unmapped interval is counted independently as processing loss.
        quality.record_omitted_spans(
            QualityClass::DegradedOnly,
            QualityResolution::Outstanding,
            omitted_metadata,
        )?;
        quality.record_omitted_spans(
            QualityClass::ProcessingLoss,
            QualityResolution::Outstanding,
            self.omitted_failures,
        )?;
        quality.overflow.counter_saturated |= self.counter_saturated || metadata_counter_saturated;
        for failure in &self.facts {
            let binding_valid = failure.epoch.is_none_or(|epoch| {
                capture
                    .epochs
                    .iter()
                    .position(|saved| *saved == epoch)
                    .is_some_and(|index| {
                        failure.start >= epoch.retained_start_frame
                            && capture
                                .epochs
                                .get(index + 1)
                                .is_none_or(|next| failure.end <= next.retained_start_frame)
                    })
            });
            if binding_valid {
                if failure.class() == QualityClass::DegradedOnly {
                    insert_retained_forced_boundary(
                        &mut quality,
                        failure.start,
                        failure.end,
                        failure.track,
                    )?;
                } else {
                    insert_retained_processing_loss(
                        &mut quality,
                        failure.start,
                        failure.end,
                        failure.track,
                        failure.cause,
                        QualityStage::Live,
                    )?;
                }
            } else {
                quality.record_omitted_spans(failure.class(), QualityResolution::Outstanding, 1)?;
            }
        }
        Ok(quality)
    }

    fn metadata_omissions(&self) -> (u64, bool) {
        let mut omitted = self.omitted_metadata;
        let mut saturated = self.mic.counter_saturated || self.tap.counter_saturated;
        add_count(&mut omitted, self.mic.omitted_runs, &mut saturated);
        add_count(&mut omitted, self.tap.omitted_runs, &mut saturated);
        (omitted, saturated)
    }

    /// Best-effort panic/error carrier only. It cannot certify capture or storage; final settlement
    /// must use the original tracker plus the authoritative Finish record, not this approximation.
    pub fn provisional_quality(&self) -> QualityRecord {
        let mut capture = QualityRecord::observed();
        let mut epochs: Vec<_> = self
            .mic
            .runs
            .iter()
            .chain(&self.tap.runs)
            .map(|run| run.epoch)
            .collect();
        epochs.sort_unstable_by_key(|epoch| epoch.id);
        epochs.dedup();
        for epoch in &epochs {
            if !epochs
                .iter()
                .any(|other| other.id == epoch.id && other != epoch)
            {
                let _ = capture.insert_epoch(*epoch);
            }
        }
        self.settle(&capture).unwrap_or_else(|_| {
            let mut quality = QualityRecord::observed();
            let mut omitted = self.omitted_failures;
            let mut saturated = self.counter_saturated;
            let (mut omitted_metadata, metadata_counter_saturated) = self.metadata_omissions();
            for fact in &self.facts {
                let count = if fact.class() == QualityClass::DegradedOnly {
                    &mut omitted_metadata
                } else {
                    &mut omitted
                };
                add_count(count, 1, &mut saturated);
            }
            quality.overflow.omitted_spans[1] = omitted;
            quality.overflow.outstanding_omitted_spans[1] = omitted;
            quality.overflow.omitted_spans[2] = omitted_metadata;
            quality.overflow.outstanding_omitted_spans[2] = omitted_metadata;
            quality.overflow.counter_saturated = saturated || metadata_counter_saturated;
            quality
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{CaptureEpochReason, SourceCompleteness};

    fn epoch() -> CaptureEpoch {
        CaptureEpoch {
            id: 0,
            source_start_ns: Some(0),
            source_rate_hz: 48_000,
            source_frame_base: 0,
            retained_start_frame: 0,
            retained_rate_hz: 48_000,
            mic_channels: 1,
            tap_channels: 1,
            reason: CaptureEpochReason::Start,
        }
    }

    fn capture(epochs: &[CaptureEpoch]) -> QualityRecord {
        let mut quality = QualityRecord::observed();
        for epoch in epochs {
            quality.insert_epoch(*epoch).unwrap();
        }
        quality
    }

    fn outcome(start: u64, end: u64, status: RegionStatus) -> RegionOutcome {
        RegionOutcome {
            span: InputSpan {
                start_frame: start,
                end_frame: end,
            },
            status,
        }
    }

    fn fail(tracker: &mut DecodeQualityTracker, track: QualityTrack, start: u64, end: u64) {
        let mut report = DecodeReport::default();
        report.record(outcome(
            start,
            end,
            RegionStatus::Failed(DecodeErrorCategory::BackendFailure),
        ));
        tracker.record_report(track, report);
    }

    #[test]
    fn forced_boundary_marks_preceding_frame_at_epoch_cut_and_late_track_origin() {
        let later = CaptureEpoch {
            id: 1,
            source_start_ns: Some(10_000_000_000),
            source_frame_base: 480_000,
            retained_start_frame: 10,
            reason: CaptureEpochReason::HostDiscontinuity,
            ..epoch()
        };
        let authority = capture(&[epoch(), later]);
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 0, 10);
        tracker.observe_input(QualityTrack::Mic, later, 10, 10);
        tracker.observe_input(QualityTrack::Tap, later, 15, 10);
        tracker.mark_forced_boundary(QualityTrack::Mic, 10);
        tracker.mark_forced_boundary(QualityTrack::Mic, 11);
        tracker.mark_forced_boundary(QualityTrack::Tap, 3);
        let settled = tracker.settle(&authority).unwrap();
        assert_eq!(
            settled
                .spans
                .iter()
                .map(|s| (s.epoch, s.start_frame, s.end_frame, s.track))
                .collect::<Vec<_>>(),
            [
                (0, 9, 10, QualityTrack::Mic),
                (1, 480_000, 480_001, QualityTrack::Mic),
                (1, 480_007, 480_008, QualityTrack::Tap)
            ]
        );
        assert!(
            settled
                .spans
                .iter()
                .all(|s| s.cause == QualityCause::ForcedBoundary
                    && s.class == QualityClass::DegradedOnly
                    && s.stage == QualityStage::Live)
        );
        assert_eq!(settled.completeness(), SourceCompleteness::Complete);
        assert!(!settled.requires_batch_recovery());
        assert_eq!(tracker.settle(&authority).unwrap(), settled);
        let mut merged = settled.clone();
        merged.merge_snapshot(&settled).unwrap();
        assert_eq!(merged, settled);
    }

    #[test]
    fn forced_boundary_zero_cut_and_provisional_late_epoch_do_not_invent_prefix() {
        let later = CaptureEpoch {
            id: 1,
            source_start_ns: Some(10_000_000_000),
            source_frame_base: 480_000,
            retained_start_frame: 10,
            reason: CaptureEpochReason::HostDiscontinuity,
            ..epoch()
        };
        let authority = capture(&[epoch(), later]);
        let mut tracker = DecodeQualityTracker::default();
        tracker.mark_forced_boundary(QualityTrack::Both, 0);
        assert_eq!(tracker.settle(&authority).unwrap(), authority);
        tracker.observe_input(QualityTrack::Tap, later, 15, 10);
        tracker.mark_forced_boundary(QualityTrack::Tap, 3);
        let provisional = tracker.provisional_quality();
        assert!(provisional.spans.is_empty());
        assert_eq!(provisional.overflow.outstanding_omitted_spans, [0, 0, 1]);
        let settled = tracker.settle(&authority).unwrap();
        assert_eq!(settled.spans.len(), 1);
        assert_eq!(
            (settled.spans[0].start_frame, settled.spans[0].end_frame),
            (480_007, 480_008)
        );
        assert_eq!(settled.overflow.outstanding_omitted_spans, [0, 0, 0]);
    }

    #[test]
    fn forced_boundary_unmapped_and_conflicting_binding_are_degraded_not_processing_loss() {
        let mut tracker = DecodeQualityTracker::default();
        tracker.mark_forced_boundary(QualityTrack::Mic, 5);
        let authority = capture(&[epoch()]);
        let settled = tracker.settle(&authority).unwrap();
        assert!(settled.spans.is_empty());
        assert_eq!(settled.overflow.outstanding_omitted_spans, [0, 0, 1]);
        assert!(!settled.requires_batch_recovery());
        tracker.observe_input(QualityTrack::Tap, epoch(), 0, 10);
        tracker.mark_forced_boundary(QualityTrack::Tap, 5);
        let mut conflict = authority.clone();
        conflict.epochs[0].tap_channels = 2;
        let settled = tracker.settle(&conflict).unwrap();
        assert!(settled.spans.is_empty());
        assert_eq!(settled.overflow.outstanding_omitted_spans, [0, 0, 2]);
        assert!(!settled.requires_batch_recovery());
    }

    #[test]
    fn forced_boundary_fact_and_mapping_caps_keep_class_without_hiding_real_failure() {
        let authority = capture(&[epoch()]);
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 0, 2000);
        for cut in (1..).step_by(2).take(MAX_QUALITY_SPANS + 2) {
            tracker.mark_forced_boundary(QualityTrack::Mic, cut);
        }
        let settled = tracker.settle(&authority).unwrap();
        assert_eq!(settled.spans.len(), MAX_QUALITY_SPANS);
        assert_eq!(settled.overflow.outstanding_omitted_spans, [0, 0, 2]);
        assert!(!settled.requires_batch_recovery());
        fail(&mut tracker, QualityTrack::Mic, 1990, 1991);
        let failed = tracker.settle(&authority).unwrap();
        assert_eq!(failed.overflow.outstanding_omitted_spans, [0, 1, 2]);
        assert!(failed.requires_batch_recovery());
        let mut mapping_capped = DecodeQualityTracker::default();
        for index in 0..=MAX_QUALITY_SPANS as u64 {
            mapping_capped.observe_input(QualityTrack::Tap, epoch(), 2 * index, 1);
        }
        mapping_capped.mark_forced_boundary(QualityTrack::Tap, MAX_QUALITY_SPANS as u64 + 1);
        let capped = mapping_capped.settle(&authority).unwrap();
        assert!(capped.spans.is_empty());
        assert_eq!(capped.overflow.outstanding_omitted_spans, [0, 0, 2]);
        assert!(!capped.requires_batch_recovery());
        assert_eq!(mapping_capped.settle(&authority).unwrap(), capped);
    }

    #[test]
    fn forced_boundary_preserves_source_and_processing_history_without_reclassification() {
        let mut authority = capture(&[epoch()]);
        authority
            .insert_span(corti_core::QualitySpan {
                epoch: 0,
                start_frame: 0,
                end_frame: 2,
                track: QualityTrack::Both,
                cause: QualityCause::RingOverflow,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 0, 20);
        fail(&mut tracker, QualityTrack::Mic, 5, 6);
        let prior = tracker.settle(&authority).unwrap();
        tracker.mark_forced_boundary(QualityTrack::Mic, 10);
        let settled = tracker.settle(&authority).unwrap();
        assert_eq!(settled.spans.len(), 3);
        assert!(prior.spans.iter().all(|span| settled.spans.contains(span)));
        assert_eq!(settled.completeness(), SourceCompleteness::Incomplete);
        assert!(settled.requires_batch_recovery());
        assert_eq!(tracker.settle(&authority).unwrap(), settled);
        // The existing loss API must not silently accept a degraded-only cause.
        let mut wrong_api = DecodeQualityTracker::default();
        wrong_api.record_retained_processing_loss(
            QualityTrack::Mic,
            9,
            10,
            QualityCause::ForcedBoundary,
        );
        assert_eq!(
            wrong_api.settle(&authority),
            Err(QualityError::InvalidClassification)
        );
    }

    #[test]
    fn forced_boundary_saturated_degradation_remains_unknown_without_recovery() {
        let authority = capture(&[epoch()]);
        let mut tracker = DecodeQualityTracker {
            omitted_metadata: u64::MAX,
            ..Default::default()
        };
        tracker.mark_forced_boundary(QualityTrack::Mic, 1);
        let quality = tracker.settle(&authority).unwrap();
        assert_eq!(quality.overflow.outstanding_omitted_spans, [0, 0, u64::MAX]);
        assert!(quality.overflow.counter_saturated);
        assert_eq!(quality.completeness(), SourceCompleteness::Unknown);
        assert!(!quality.requires_batch_recovery());
        assert_eq!(tracker.settle(&authority).unwrap(), quality);
    }

    #[test]
    fn complete_and_empty_report_caps_retain_degradation_without_recovery() {
        for status in [RegionStatus::Complete, RegionStatus::RecognizedEmpty] {
            let mut report = DecodeReport::default();
            for frame in 0..=MAX_QUALITY_SPANS as u64 {
                report.record(outcome(frame, frame + 1, status));
            }
            assert!(report.is_complete());
            assert_eq!(report.omitted_failures(), 0);
            assert_eq!(report.outcomes().len(), MAX_QUALITY_SPANS);
            let mut tracker = DecodeQualityTracker::default();
            tracker.record_report(QualityTrack::Mic, report);
            let quality = tracker.settle(&capture(&[epoch()])).unwrap();
            assert_eq!(quality.overflow.omitted_spans, [0, 0, 1]);
            assert_eq!(quality.overflow.outstanding_omitted_spans, [0, 0, 1]);
            assert_eq!(quality.completeness(), SourceCompleteness::Complete);
            assert!(!quality.requires_batch_recovery());
            assert!(quality.spans.is_empty());
        }
    }

    #[test]
    fn failed_report_replacement_counts_displaced_success_and_preserves_failure() {
        let mut report = DecodeReport::default();
        for frame in 0..MAX_QUALITY_SPANS as u64 {
            report.record(outcome(frame, frame + 1, RegionStatus::Complete));
        }
        report.record(outcome(
            300,
            301,
            RegionStatus::Partial(DecodeErrorCategory::OutputTruncated),
        ));
        assert!(!report.is_complete());
        assert_eq!(report.omitted_failures(), 0);
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 0, 400);
        tracker.record_report(QualityTrack::Mic, report);
        let quality = tracker.settle(&capture(&[epoch()])).unwrap();
        assert_eq!(quality.overflow.omitted_spans, [0, 0, 1]);
        assert_eq!(quality.overflow.outstanding_omitted_spans, [0, 0, 1]);
        assert_eq!(quality.spans.len(), 1);
        assert_eq!(
            (quality.spans[0].start_frame, quality.spans[0].end_frame),
            (300, 301)
        );
        assert_eq!(quality.spans[0].class, QualityClass::ProcessingLoss);
        assert!(quality.requires_batch_recovery());
    }

    #[test]
    fn merged_report_caps_and_drains_count_each_omission_once() {
        let mut left = DecodeReport::default();
        let mut right = DecodeReport::default();
        for frame in 0..MAX_QUALITY_SPANS as u64 + 1 {
            left.record(outcome(frame, frame + 1, RegionStatus::Complete));
        }
        for frame in 500..500 + MAX_QUALITY_SPANS as u64 + 2 {
            right.record(outcome(frame, frame + 1, RegionStatus::RecognizedEmpty));
        }
        left.merge(right);
        assert!(left.is_complete());
        let mut tracker = DecodeQualityTracker::default();
        tracker.record_report(QualityTrack::Mic, std::mem::take(&mut left));
        tracker.record_report(QualityTrack::Mic, std::mem::take(&mut left));
        let authority = capture(&[epoch()]);
        let quality = tracker.settle(&authority).unwrap();
        // One prior omission on the left, two on the right, and 256 during the bounded merge.
        assert_eq!(quality.overflow.omitted_spans, [0, 0, 259]);
        assert_eq!(quality.overflow.outstanding_omitted_spans, [0, 0, 259]);
        assert_eq!(tracker.settle(&authority).unwrap(), quality);
        assert_eq!(tracker.provisional_quality().overflow, quality.overflow);
        assert_eq!(quality.completeness(), SourceCompleteness::Complete);
        assert!(!quality.requires_batch_recovery());
    }

    #[test]
    fn metadata_saturation_keeps_decode_success_but_quality_unknown() {
        let mut report = DecodeReport {
            omitted_metadata: u64::MAX,
            ..Default::default()
        };
        report.omit_metadata();
        report.record(outcome(0, 1, RegionStatus::RecognizedEmpty));
        assert_eq!(report.omitted_metadata(), u64::MAX);
        assert!(report.metadata_counter_saturated());
        assert!(report.counter_saturated());
        assert!(report.is_complete());
        assert_eq!(report.omitted_failures(), 0);
        let mut merged = DecodeReport::default();
        merged.merge(report);
        assert!(merged.is_complete());
        assert!(merged.metadata_counter_saturated());
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 0, 10);
        tracker.record_report(QualityTrack::Mic, merged);
        let authority = capture(&[epoch()]);
        let quality = tracker.settle(&authority).unwrap();
        assert_eq!(quality.overflow.omitted_spans, [0, 0, u64::MAX]);
        assert_eq!(quality.completeness(), SourceCompleteness::Unknown);
        assert!(!quality.requires_batch_recovery());
        fail(&mut tracker, QualityTrack::Mic, 2, 3);
        let failed = tracker.settle(&authority).unwrap();
        assert_eq!(failed.overflow, quality.overflow);
        assert_eq!(failed.completeness(), SourceCompleteness::Incomplete);
        assert!(failed.requires_batch_recovery());
    }

    #[test]
    fn report_and_received_mapping_omissions_accumulate_without_losing_real_failure() {
        let mut tracker = DecodeQualityTracker::default();
        for index in 0..=MAX_QUALITY_SPANS as u64 {
            tracker.observe_input(QualityTrack::Mic, epoch(), index * 2, 1);
        }
        for track in [QualityTrack::Mic, QualityTrack::Tap] {
            let mut report = DecodeReport::default();
            report.omit_metadata();
            tracker.record_report(track, report);
        }
        let mut authority = capture(&[epoch()]);
        authority
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                7,
            )
            .unwrap();
        let quality = tracker.settle(&authority).unwrap();
        assert_eq!(quality.overflow.omitted_spans, [0, 0, 10]);
        assert_eq!(quality.completeness(), SourceCompleteness::Complete);
        assert!(!quality.requires_batch_recovery());
        // The last received run was omitted. Its actual failed decoding is independent loss.
        fail(
            &mut tracker,
            QualityTrack::Mic,
            MAX_QUALITY_SPANS as u64,
            MAX_QUALITY_SPANS as u64 + 1,
        );
        let failed = tracker.settle(&authority).unwrap();
        assert_eq!(failed.overflow.omitted_spans, [0, 1, 10]);
        assert_eq!(failed.overflow.outstanding_omitted_spans, [0, 1, 10]);
        assert!(failed.requires_batch_recovery());
        assert_eq!(tracker.settle(&authority).unwrap(), failed);
        assert_eq!(
            tracker.provisional_quality().overflow.omitted_spans,
            [0, 1, 3]
        );
    }

    #[test]
    fn successful_cap_never_hides_a_later_failure_or_invalidates_silence() {
        let mut report = DecodeReport::default();
        for frame in 0..MAX_QUALITY_SPANS as u64 + 100 {
            report.record(outcome(frame, frame + 1, RegionStatus::RecognizedEmpty));
        }
        assert_eq!(report.outcomes().len(), MAX_QUALITY_SPANS);
        assert!(report.is_complete());
        let failure = outcome(
            500,
            600,
            RegionStatus::Partial(DecodeErrorCategory::OutputTruncated),
        );
        report.record(failure);
        assert!(!report.is_complete());
        assert!(report.outcomes().contains(&failure));
        assert_eq!(report.omitted_failures(), 0);
    }

    #[test]
    fn report_merge_and_saturated_failure_cap_cannot_restore_complete_success() {
        let mut report = DecodeReport::default();
        for frame in 0..MAX_QUALITY_SPANS as u64 + 3 {
            report.record(outcome(
                frame,
                frame + 1,
                RegionStatus::Failed(DecodeErrorCategory::NoResult),
            ));
        }
        assert_eq!(report.outcomes().len(), MAX_QUALITY_SPANS);
        assert_eq!(report.omitted_failures(), 3);
        let mut omitted = DecodeReport {
            omitted_failures: u64::MAX,
            ..Default::default()
        };
        omitted.omit_failure();
        report.merge(omitted);
        assert_eq!(report.omitted_failures(), u64::MAX);
        assert!(report.counter_saturated());
        assert!(!report.is_complete());
        let mut invalid = DecodeReport::default();
        invalid.record(outcome(2, 1, RegionStatus::Complete));
        assert!(!invalid.is_complete());
        assert_eq!(invalid.omitted_failures(), 1);
    }

    #[test]
    fn late_first_received_epoch_settles_exactly_after_provisional_omission() {
        let later = CaptureEpoch {
            id: 1,
            source_start_ns: Some(100_000_000_000),
            source_frame_base: 4_800_000,
            retained_start_frame: 48_000,
            reason: CaptureEpochReason::HostDiscontinuity,
            ..epoch()
        };
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Tap, later, 50_000, 300);
        fail(&mut tracker, QualityTrack::Tap, 20, 40);
        assert_eq!(
            tracker
                .provisional_quality()
                .overflow
                .outstanding_omitted_spans[1],
            1
        );
        let settled = tracker.settle(&capture(&[epoch(), later])).unwrap();
        assert_eq!(settled.spans.len(), 1);
        let span = settled.spans[0];
        assert_eq!(
            (span.epoch, span.start_frame, span.end_frame, span.track),
            (1, 4_802_020, 4_802_040, QualityTrack::Tap)
        );
        assert_eq!(settled.overflow.outstanding_omitted_spans[1], 0);
    }

    #[test]
    fn buffered_aec_tee_gaps_and_compressed_epochs_do_not_shift_later_failures() {
        let later = CaptureEpoch {
            id: 1,
            source_start_ns: Some(12_000_000_000),
            source_frame_base: 576_000,
            retained_start_frame: 8_192,
            reason: CaptureEpochReason::HostDiscontinuity,
            ..epoch()
        };
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 0, 4_096);
        // The middle tee chunk was never received. Observe the next chunk before an AEC-delayed report.
        tracker.observe_input(QualityTrack::Mic, later, 8_192, 4_096);
        fail(&mut tracker, QualityTrack::Mic, 4_090, 4_110);
        let settled = tracker.settle(&capture(&[epoch(), later])).unwrap();
        assert_eq!(
            settled
                .spans
                .iter()
                .map(|span| (span.epoch, span.start_frame, span.end_frame))
                .collect::<Vec<_>>(),
            [(0, 4_090, 4_096), (1, 576_000, 576_014)]
        );
        assert_eq!(settled.overflow.outstanding_omitted_spans[1], 0);
    }

    #[test]
    fn resampler_failure_survives_recovery_rate_change_and_independent_track_clocks() {
        let later = CaptureEpoch {
            id: 1,
            source_start_ns: Some(1_000_000_000),
            source_frame_base: 48_000,
            source_rate_hz: 44_100,
            retained_start_frame: 48_000,
            reason: CaptureEpochReason::RateChange,
            ..epoch()
        };
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 0, 48_000);
        tracker.observe_input(QualityTrack::Mic, later, 48_000, 48_000);
        tracker.observe_input(QualityTrack::Tap, later, 72_000, 24_000);
        let mut report = DecodeReport::default();
        report.record(outcome(
            0,
            16_000,
            RegionStatus::Failed(DecodeErrorCategory::ResamplerUnavailable),
        ));
        report.record(outcome(16_000, 48_000, RegionStatus::RecognizedEmpty));
        report.record(outcome(48_000, 96_000, RegionStatus::Complete));
        tracker.record_report(QualityTrack::Mic, report);
        fail(&mut tracker, QualityTrack::Tap, 0, 48);
        let settled = tracker.settle(&capture(&[epoch(), later])).unwrap();
        assert_eq!(settled.spans.len(), 2);
        assert_eq!(
            (
                settled.spans[0].start_frame,
                settled.spans[0].end_frame,
                settled.spans[0].cause
            ),
            (0, 16_000, QualityCause::ResamplerFailure)
        );
        assert_eq!(
            (
                settled.spans[1].start_frame,
                settled.spans[1].end_frame,
                settled.spans[1].track
            ),
            (70_050, 70_095, QualityTrack::Tap)
        );
        assert_eq!(settled.completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn unmappable_gap_keeps_known_intersections_and_counts_missing_processing_loss() {
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 0, 10);
        tracker.observe_input(QualityTrack::Mic, epoch(), u64::MAX, 10);
        tracker.observe_input(QualityTrack::Mic, epoch(), 20, 10);
        fail(&mut tracker, QualityTrack::Mic, 5, 25);
        let settled = tracker.settle(&capture(&[epoch()])).unwrap();
        assert_eq!(
            settled
                .spans
                .iter()
                .map(|span| (span.start_frame, span.end_frame))
                .collect::<Vec<_>>(),
            [(5, 10), (20, 25)]
        );
        assert_eq!(settled.overflow.outstanding_omitted_spans[1], 1);
        let mut conflicting = capture(&[epoch()]);
        conflicting.epochs[0].mic_channels = 2;
        let settled = tracker.settle(&conflicting).unwrap();
        assert!(settled.spans.is_empty());
        assert_eq!(settled.overflow.outstanding_omitted_spans[1], 3);
    }

    #[test]
    fn mapping_and_failure_caps_coalesce_only_exact_adjacent_bindings() {
        let mut tracker = DecodeQualityTracker::default();
        for frame in 0..1_000 {
            tracker.observe_input(QualityTrack::Mic, epoch(), frame, 1);
        }
        assert_eq!(tracker.mic.runs.len(), 1);
        for frame in 0..MAX_QUALITY_SPANS as u64 + 2 {
            tracker.observe_input(QualityTrack::Tap, epoch(), frame * 2, 1);
        }
        assert_eq!(tracker.tap.runs.len(), MAX_QUALITY_SPANS);
        fail(
            &mut tracker,
            QualityTrack::Tap,
            0,
            MAX_QUALITY_SPANS as u64 + 2,
        );
        assert_eq!(tracker.facts.len(), MAX_QUALITY_SPANS);
        let settled = tracker.settle(&capture(&[epoch()])).unwrap();
        assert_eq!(settled.spans.len(), MAX_QUALITY_SPANS);
        assert_eq!(settled.overflow.outstanding_omitted_spans[1], 1);
        fail(&mut tracker, QualityTrack::Mic, 800, 801);
        assert_eq!(
            tracker
                .settle(&capture(&[epoch()]))
                .unwrap()
                .overflow
                .outstanding_omitted_spans[1],
            2
        );
    }

    #[test]
    fn mapping_omission_cap_records_degradation_without_inventing_decode_loss() {
        let mut tracker = DecodeQualityTracker::default();
        for track in [QualityTrack::Mic, QualityTrack::Tap] {
            let mut report = DecodeReport::default();
            for frame in 0..=MAX_QUALITY_SPANS as u64 {
                let span = tracker.observe_input(track, epoch(), frame * 2, 1);
                report.record(RegionOutcome {
                    span,
                    status: RegionStatus::Complete,
                });
            }
            assert!(report.is_complete());
            assert_eq!(report.omitted_metadata(), 1);
            tracker.record_report(track, report);
        }
        let mut authority = capture(&[epoch()]);
        authority
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                7,
            )
            .unwrap();
        let settled = tracker.settle(&authority).unwrap();
        // Seven inherited facts, two omitted received runs and two omitted successful reports.
        assert_eq!(settled.overflow.omitted_spans, [0, 0, 11]);
        assert_eq!(settled.overflow.outstanding_omitted_spans, [0, 0, 11]);
        assert!(settled.spans.is_empty());
        assert_eq!(settled.completeness(), SourceCompleteness::Complete);
        assert!(!settled.requires_batch_recovery());
        assert_eq!(tracker.settle(&authority).unwrap(), settled);
        assert_eq!(authority.overflow.omitted_spans, [0, 0, 7]);
        let mut merged = authority.clone();
        merged.merge_snapshot(&settled).unwrap();
        merged.merge_snapshot(&settled).unwrap();
        assert_eq!(merged, settled);
        assert_eq!(
            tracker.provisional_quality().overflow.omitted_spans,
            [0, 0, 4]
        );

        // An actual decode failure in the omitted run is separately counted processing loss.
        fail(
            &mut tracker,
            QualityTrack::Tap,
            MAX_QUALITY_SPANS as u64,
            MAX_QUALITY_SPANS as u64 + 1,
        );
        let failed = tracker.settle(&authority).unwrap();
        assert_eq!(failed.overflow.omitted_spans, [0, 1, 11]);
        assert_eq!(failed.completeness(), SourceCompleteness::Incomplete);
        assert!(failed.requires_batch_recovery());
    }

    #[test]
    fn mapping_omission_invalid_inputs_and_clock_overflow_are_counted_once_per_run() {
        for (binding, retained_start, frames) in [
            (
                CaptureEpoch {
                    retained_start_frame: 8,
                    ..epoch()
                },
                7,
                0,
            ),
            (epoch(), u64::MAX, 0),
            (
                CaptureEpoch {
                    retained_rate_hz: 0,
                    ..epoch()
                },
                0,
                0,
            ),
            (
                CaptureEpoch {
                    source_rate_hz: 0,
                    ..epoch()
                },
                0,
                0,
            ),
            (epoch(), 0, u64::MAX),
        ] {
            let mut tracker = DecodeQualityTracker::default();
            tracker.mic.frames = frames;
            tracker.observe_input(QualityTrack::Mic, binding, retained_start, 1);
            let settled = tracker.settle(&capture(&[epoch()])).unwrap();
            assert_eq!(settled.overflow.omitted_spans, [0, 0, 1]);
            assert_eq!(settled.overflow.outstanding_omitted_spans, [0, 0, 1]);
            assert!(settled.spans.is_empty());
            assert!(!settled.requires_batch_recovery());
            if frames == u64::MAX {
                tracker.observe_input(QualityTrack::Mic, epoch(), 1, 1);
                assert_eq!(
                    tracker
                        .settle(&capture(&[epoch()]))
                        .unwrap()
                        .overflow
                        .omitted_spans,
                    [0, 0, 2]
                );
            }
        }
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 10, 10);
        tracker.observe_input(QualityTrack::Mic, epoch(), 5, 1);
        tracker.observe_input(QualityTrack::Mic, epoch(), 19, 1);
        assert_eq!(
            tracker
                .settle(&capture(&[epoch()]))
                .unwrap()
                .overflow
                .omitted_spans,
            [0, 0, 2]
        );

        let mut coalesced = DecodeQualityTracker::default();
        for frame in 0..1_000 {
            coalesced.observe_input(QualityTrack::Mic, epoch(), frame, 1);
        }
        coalesced.observe_input(
            QualityTrack::Mic,
            CaptureEpoch {
                retained_rate_hz: 0,
                ..epoch()
            },
            u64::MAX,
            0,
        );
        assert_eq!(coalesced.mic.runs.len(), 1);
        assert_eq!(
            coalesced.settle(&capture(&[epoch()])).unwrap().overflow,
            Default::default()
        );
    }

    #[test]
    fn mapping_omission_counter_saturation_remains_unknown_without_claiming_decode_loss() {
        for aggregate_overflow in [false, true] {
            let mut tracker = DecodeQualityTracker::default();
            tracker.mic.omitted_runs = u64::MAX;
            if aggregate_overflow {
                tracker.tap.omitted_runs = 1;
            } else {
                tracker.observe_input(QualityTrack::Mic, epoch(), u64::MAX, 1);
                assert!(tracker.mic.counter_saturated);
            }
            let authority = capture(&[epoch()]);
            let settled = tracker.settle(&authority).unwrap();
            assert_eq!(settled.overflow.omitted_spans, [0, 0, u64::MAX]);
            assert_eq!(settled.overflow.outstanding_omitted_spans, [0, 0, u64::MAX]);
            assert!(settled.overflow.counter_saturated);
            assert_eq!(settled.completeness(), SourceCompleteness::Unknown);
            assert!(!settled.requires_batch_recovery());
            assert!(settled.spans.is_empty());
            assert_eq!(tracker.settle(&authority).unwrap(), settled);
            assert_eq!(tracker.provisional_quality().overflow, settled.overflow);
            settled.validate().unwrap();
        }
    }

    #[test]
    fn window_not_filed_uses_logical_inputs_but_retained_losses_split_at_authority() {
        let later = CaptureEpoch {
            id: 1,
            source_start_ns: Some(1_000_000_000),
            source_frame_base: 48_000,
            retained_start_frame: 10,
            reason: CaptureEpochReason::HostDiscontinuity,
            ..epoch()
        };
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 0, 10);
        tracker.observe_input(QualityTrack::Mic, later, 20, 10);
        tracker.record_window_not_filed(
            QualityTrack::Mic,
            InputSpan {
                start_frame: 5,
                end_frame: 15,
            },
        );
        tracker.record_retained_processing_loss(
            QualityTrack::Tap,
            5,
            15,
            QualityCause::WindowNotFiled,
        );
        let settled = tracker.settle(&capture(&[epoch(), later])).unwrap();
        assert_eq!(
            settled
                .spans
                .iter()
                .map(|span| (span.epoch, span.start_frame, span.end_frame, span.track))
                .collect::<Vec<_>>(),
            [
                (0, 5, 10, QualityTrack::Mic),
                (0, 5, 10, QualityTrack::Tap),
                (1, 48_000, 48_005, QualityTrack::Tap),
                (1, 48_010, 48_015, QualityTrack::Mic)
            ]
        );
        assert!(
            settled
                .spans
                .iter()
                .all(|span| span.cause == QualityCause::WindowNotFiled)
        );
    }

    #[test]
    fn invalid_and_saturated_input_coordinates_never_panic_or_create_a_source_span() {
        let mut tracker = DecodeQualityTracker::default();
        tracker.observe_input(QualityTrack::Mic, epoch(), 1, u64::MAX);
        tracker.observe_input(QualityTrack::Mic, epoch(), 1, 2);
        fail(&mut tracker, QualityTrack::Mic, u64::MAX - 2, u64::MAX);
        let settled = tracker.settle(&capture(&[epoch()])).unwrap();
        assert!(settled.spans.is_empty());
        assert_eq!(settled.overflow.outstanding_omitted_spans[1], 1);
    }
}
