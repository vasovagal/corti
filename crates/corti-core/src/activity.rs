//! Bounded observation of normalized/downmixed pre-AEC mic and tap samples.
//!
//! The caller owns one tracker per input/stage for the entire traversal (including live windows).
//! Feed actual retained offsets and CaptureEpoch bindings before AEC or mismatch padding. Numerical
//! unavailable fill is never observation; None availability is legacy/unknown and observes nothing.
//! Publish into the carried QualityRecord and persist that same record through existing authority.
//! No tracker method establishes audio durability or authorizes fallback, recovery, or note writes.

#[cfg(test)]
use crate::FarReference;
use crate::quality_activity::{ActivityEvidence, ActivityExtent, MAX_ACTIVITY_RATE_HZ};
use crate::{
    ActivityInput, ActivityScope, CaptureEpoch, QualityCause, QualityClass, QualityError,
    QualityRecord, QualityResolution, QualitySpan, QualityTrack,
};

// Accepted approach NOTE: five one-second bins and -60 dBFS; these remain uncalibrated diagnostics.
pub const UNCALIBRATED_MIC_ACTIVE_DBFS: f64 = -60.0;
pub const UNCALIBRATED_MIC_ACTIVE_MEAN_SQUARE: f64 = 0.000_001;
pub const UNCALIBRATED_SUSTAINED_SECONDS: u8 = 5;
const MAX_RETAINED_RATE_HZ: u32 = MAX_ACTIVITY_RATE_HZ;
pub const SUSPECTED_LABEL: &str = "suspected silent far reference";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Epoch {
    id: u32,
    retained_start: u64,
    rate: u32,
    mic_expected: bool,
    tap_expected: bool,
}

struct Block<'a> {
    epoch: Epoch,
    start: u64,
    mic: &'a [f32],
    tap: &'a [f32],
    // Existing CaptureChunk convention: empty means all frames available.
    // A nonempty bitmap must cover every input frame exactly.
    unavailable: &'a [bool],
    availability_known: bool,
}

/// Ephemeral retained-frame interval. An adapter must insert FarReferenceSilent /
/// DegradedOnly / Tap / its actual stage in the EXISTING QualityRecord, after mapping.
/// This is not a durable carrier or a source/completeness certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SuspectedRun {
    epoch: Epoch,
    start: u64,
    end: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActivityLevels {
    pub epoch: u32,
    pub start: u64,
    pub end: u64,
    pub complete_second: bool,
    pub mic_observed: u32,
    pub tap_observed: u32,
    pub mic_mean_square: Option<f64>,
    pub tap_mean_square: Option<f64>,
    pub mic_peak: Option<f32>,
    pub tap_peak: Option<f32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Facts {
    observed_tap: bool,
    nonzero_tap: bool,
    sustained_zero_tap: bool,
    absent_tap_layout: bool,
    suspected: bool,
    uncertain: bool,
    terminal_complete: bool,
    counter_saturated: bool,
}

#[cfg(test)]
impl Facts {
    fn far_reference(self) -> FarReference {
        if self.nonzero_tap {
            if self.sustained_zero_tap || self.absent_tap_layout {
                FarReference::Intermittent
            } else {
                FarReference::Present
            }
        } else if self.terminal_complete && !self.uncertain {
            FarReference::Absent
        } else {
            FarReference::Unknown
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Bin {
    start: u64,
    frames: u32,
    mic_observed: u32,
    tap_observed: u32,
    mic_energy: f64,
    tap_energy: f64,
    mic_peak: f32,
    tap_peak: f32,
    tap_nonzero: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityInputError {
    Closed,
    InvalidQuality,
    InvalidEpoch,
    InvalidAvailability,
    UnexpectedTrack,
    BackwardPosition,
    PositionOverflow,
}

/// No heap fields or recording-length history. Counters are local diagnostics, not
/// quantities to sum across retries/snapshots. Only boolean facts are union candidates.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Activity {
    epoch: Option<Epoch>,
    cursor: u64,
    bin: Bin,
    last_levels: Option<ActivityLevels>,
    zero_seconds: u8,
    candidate_seconds: u8,
    candidate_start: u64,
    active_run: Option<SuspectedRun>,
    facts: Facts,
    completed_bins: u64,
    closed_runs: u64,
    omitted_runs: u64,
    saw_frames: bool,
    closed: bool,
    halted: bool,
}

impl Activity {
    fn push(
        &mut self,
        block: Block<'_>,
        emit: &mut impl FnMut(SuspectedRun) -> bool,
    ) -> Result<(), ActivityInputError> {
        if self.closed || self.halted {
            return Err(ActivityInputError::Closed);
        }
        let epoch = block.epoch;
        let frames = block.mic.len().max(block.tap.len());
        let bad = if epoch.rate == 0
            || epoch.rate > MAX_RETAINED_RATE_HZ
            || (!epoch.mic_expected && !epoch.tap_expected)
            || block.start < epoch.retained_start
            || self
                .epoch
                .is_some_and(|old| epoch.id < old.id || (epoch.id == old.id && epoch != old))
        {
            Some(ActivityInputError::InvalidEpoch)
        } else if !block.unavailable.is_empty() && block.unavailable.len() != frames {
            Some(ActivityInputError::InvalidAvailability)
        } else if (!epoch.mic_expected && !block.mic.is_empty())
            || (!epoch.tap_expected && !block.tap.is_empty())
        {
            Some(ActivityInputError::UnexpectedTrack)
        } else if self.epoch.is_some()
            && (block.start < self.cursor
                || (self.epoch != Some(epoch) && epoch.retained_start < self.cursor))
        {
            Some(ActivityInputError::BackwardPosition)
        } else {
            None
        };
        if let Some(error) = bad {
            self.interrupt(emit);
            return Err(error);
        }
        let Some(end) = u64::try_from(frames)
            .ok()
            .and_then(|frames| block.start.checked_add(frames))
        else {
            self.interrupt(emit);
            self.halted = true;
            return Err(ActivityInputError::PositionOverflow);
        };
        if frames == 0 {
            // An empty push carries no observed timeline or epoch transition.
            return Ok(());
        }
        if self.epoch != Some(epoch) {
            self.seal_partial();
            self.close_run(emit);
            self.zero_seconds = 0;
            self.candidate_seconds = 0;
            if block.start != epoch.retained_start
                || (self.epoch.is_none() && block.start != 0)
                || (self.epoch.is_some() && block.start != self.cursor)
            {
                self.facts.uncertain = true;
            }
            self.epoch = Some(epoch);
            self.cursor = block.start;
            self.bin = Bin {
                start: block.start,
                ..Bin::default()
            };
        } else if block.start != self.cursor {
            self.interrupt(emit);
            self.cursor = block.start;
            self.bin.start = block.start;
        }
        self.saw_frames = true;
        self.facts.absent_tap_layout |= !epoch.tap_expected;
        for i in 0..frames {
            let available =
                block.availability_known && (block.unavailable.is_empty() || !block.unavailable[i]);
            let mic = block
                .mic
                .get(i)
                .copied()
                .filter(|v| available && v.is_finite());
            let tap = block
                .tap
                .get(i)
                .copied()
                .filter(|v| available && v.is_finite());
            let mic_unknown = epoch.mic_expected && mic.is_none();
            let tap_unknown = epoch.tap_expected && tap.is_none();
            if !available || mic_unknown || tap_unknown {
                self.facts.uncertain = true;
                self.close_run(emit);
                self.candidate_seconds = 0;
            }
            // Reference availability describes tap observations independently of mic
            // activity. Missing/invalid mic breaks suspicion, not valid tap history.
            if !available || tap_unknown {
                self.zero_seconds = 0;
            }
            if let Some(sample) = mic {
                self.bin.mic_observed += 1;
                self.bin.mic_energy += f64::from(sample) * f64::from(sample);
                self.bin.mic_peak = self.bin.mic_peak.max(sample.abs());
            }
            if let Some(sample) = tap {
                self.facts.observed_tap = true;
                self.bin.tap_observed += 1;
                self.bin.tap_energy += f64::from(sample) * f64::from(sample);
                self.bin.tap_peak = self.bin.tap_peak.max(sample.abs());
                if sample != 0.0 {
                    self.facts.nonzero_tap = true;
                    self.bin.tap_nonzero = true;
                    self.close_run(emit);
                    self.zero_seconds = 0;
                    self.candidate_seconds = 0;
                }
            }
            self.bin.frames += 1;
            self.cursor += 1; // Whole-block endpoint was checked before any admission.
            if self.bin.frames == epoch.rate {
                self.complete_bin(epoch, emit);
            }
        }
        debug_assert_eq!(self.cursor, end);
        Ok(())
    }

    fn complete_bin(&mut self, epoch: Epoch, emit: &mut impl FnMut(SuspectedRun) -> bool) {
        self.last_levels = Some(self.levels(true));
        increment(&mut self.completed_bins, &mut self.facts.counter_saturated);
        let zero =
            epoch.tap_expected && self.bin.tap_observed == epoch.rate && !self.bin.tap_nonzero;
        self.zero_seconds = if zero {
            (self.zero_seconds + 1).min(UNCALIBRATED_SUSTAINED_SECONDS)
        } else {
            0
        };
        self.facts.sustained_zero_tap |= self.zero_seconds == UNCALIBRATED_SUSTAINED_SECONDS;
        let active = epoch.mic_expected
            && self.bin.mic_observed == epoch.rate
            && self.bin.mic_energy / f64::from(epoch.rate) >= UNCALIBRATED_MIC_ACTIVE_MEAN_SQUARE;
        if zero && active {
            if self.candidate_seconds == 0 {
                self.candidate_start = self.bin.start;
            }
            self.candidate_seconds =
                (self.candidate_seconds + 1).min(UNCALIBRATED_SUSTAINED_SECONDS);
            if self.candidate_seconds == UNCALIBRATED_SUSTAINED_SECONDS {
                self.facts.suspected = true;
                self.active_run = Some(SuspectedRun {
                    epoch,
                    start: self.candidate_start,
                    end: self.cursor,
                });
            }
        } else {
            self.close_run(emit);
            self.candidate_seconds = 0;
        }
        self.bin = Bin {
            start: self.cursor,
            ..Bin::default()
        };
    }

    fn levels(&self, complete_second: bool) -> ActivityLevels {
        let b = self.bin;
        ActivityLevels {
            epoch: self.epoch.expect("levels require an observed epoch").id,
            start: b.start,
            end: self.cursor,
            complete_second,
            mic_observed: b.mic_observed,
            tap_observed: b.tap_observed,
            mic_mean_square: (b.mic_observed != 0)
                .then(|| b.mic_energy / f64::from(b.mic_observed)),
            tap_mean_square: (b.tap_observed != 0)
                .then(|| b.tap_energy / f64::from(b.tap_observed)),
            mic_peak: (b.mic_observed != 0).then_some(b.mic_peak),
            tap_peak: (b.tap_observed != 0).then_some(b.tap_peak),
        }
    }

    fn seal_partial(&mut self) {
        if self.bin.frames != 0 {
            self.last_levels = Some(self.levels(false));
        }
        self.bin = Bin {
            start: self.cursor,
            ..Bin::default()
        };
    }

    fn close_run(&mut self, emit: &mut impl FnMut(SuspectedRun) -> bool) {
        if let Some(run) = self.active_run.take() {
            increment(&mut self.closed_runs, &mut self.facts.counter_saturated);
            if !emit(run) {
                increment(&mut self.omitted_runs, &mut self.facts.counter_saturated);
            }
        }
    }

    fn interrupt(&mut self, emit: &mut impl FnMut(SuspectedRun) -> bool) {
        self.facts.uncertain = true;
        self.seal_partial();
        self.close_run(emit);
        self.zero_seconds = 0;
        self.candidate_seconds = 0;
    }

    /// Caller supplies the authoritative terminal retained endpoint, not merely the
    /// observed cursor. None means abort/missing verdict. Repeated finish is inert.
    fn finish(&mut self, expected_end: Option<u64>, emit: &mut impl FnMut(SuspectedRun) -> bool) {
        if self.closed {
            return;
        }
        self.seal_partial();
        self.close_run(emit);
        self.facts.terminal_complete = self.saw_frames
            && !self.halted
            && !self.facts.uncertain
            && expected_end == Some(self.cursor);
        self.facts.uncertain |= expected_end != Some(self.cursor) || self.halted;
        self.closed = true;
    }
}

fn increment(counter: &mut u64, saturated: &mut bool) {
    if let Some(next) = counter.checked_add(1) {
        *counter = next;
    } else {
        *saturated = true;
    }
}

/// Borrowed real pre-AEC samples. Empty Some bitmap means all available; None is unknown.
/// Length is max(mic.len(), tap.len()); missing expected samples remain unknown, never padded.
pub struct ActivityBlock<'a> {
    pub epoch: CaptureEpoch,
    pub retained_start: u64,
    pub mic: &'a [f32],
    pub tap: &'a [f32],
    pub unavailable: Option<&'a [bool]>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PublishedRun {
    epoch: u32,
    start: u64,
    end: u64,
    omitted: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct RunPublication {
    run: Option<PublishedRun>,
    omitted_runs: u64,
    counter_saturated: bool,
}

/// Fixed-memory tracker. The QualityRecord is borrowed on each call, never duplicated here.
/// Keep the same carried record across pushes: the tracker remembers which open run was published.
#[derive(Clone, Debug)]
pub struct ChannelActivity {
    scope: ActivityScope,
    input: ActivityInput,
    tracker: Activity,
    epoch: Option<CaptureEpoch>,
    publication: RunPublication,
}

impl ChannelActivity {
    pub fn new(scope: ActivityScope, input: ActivityInput) -> Result<Self, QualityError> {
        input.validate()?;
        Ok(Self {
            scope,
            input,
            tracker: Activity::default(),
            epoch: None,
            publication: RunPublication::default(),
        })
    }

    pub fn latest_levels(&self) -> Option<ActivityLevels> {
        self.tracker.last_levels
    }

    /// Observes without modifying samples. Invalid quality is rejected before admitting input.
    /// Observation errors retain already observed facts and closed/provisional suspected spans.
    pub fn push(
        &mut self,
        block: ActivityBlock<'_>,
        record: &mut QualityRecord,
    ) -> Result<(), ActivityInputError> {
        record
            .validate()
            .map_err(|_| ActivityInputError::InvalidQuality)?;
        let previous = self.epoch;
        let epoch = block.epoch;
        let mut emit = |run| {
            publish_run(
                record,
                self.scope,
                self.input,
                &mut self.publication,
                run,
                [previous, Some(epoch)],
            )
        };
        if epoch.retained_rate_hz != self.input.retained_rate_hz()
            || epoch.source_time_ns(epoch.source_frame_base).is_err()
            || previous.is_some_and(|old| old.id == epoch.id && old != epoch)
        {
            self.tracker.interrupt(&mut emit);
            self.publish_facts(record);
            return Err(ActivityInputError::InvalidEpoch);
        }
        let result = self.tracker.push(
            Block {
                epoch: Epoch {
                    id: epoch.id,
                    retained_start: epoch.retained_start_frame,
                    rate: epoch.retained_rate_hz,
                    mic_expected: epoch.mic_channels != 0,
                    tap_expected: epoch.tap_channels != 0,
                },
                start: block.retained_start,
                mic: block.mic,
                tap: block.tap,
                unavailable: block.unavailable.unwrap_or_default(),
                availability_known: block.unavailable.is_some(),
            },
            &mut emit,
        );
        if result.is_ok() && (!block.mic.is_empty() || !block.tap.is_empty()) {
            self.epoch = Some(epoch);
        }
        if let Some(run) = self.tracker.active_run {
            publish_run(
                record,
                self.scope,
                self.input,
                &mut self.publication,
                run,
                [previous, Some(epoch)],
            );
        }
        self.publish_facts(record);
        result
    }

    /// The endpoint must come from the independently checked complete input extent, not the last
    /// observed block. None records an aborted/missing verdict. Repeated finish is idempotent.
    pub fn finish(
        &mut self,
        authoritative_end: Option<u64>,
        record: &mut QualityRecord,
    ) -> Result<(), QualityError> {
        record.validate()?;
        self.tracker.finish(authoritative_end, &mut |run| {
            publish_run(
                record,
                self.scope,
                self.input,
                &mut self.publication,
                run,
                [self.epoch, None],
            )
        });
        self.publish_facts(record);
        Ok(())
    }

    fn publish_facts(&self, record: &mut QualityRecord) {
        let facts = self.tracker.facts;
        record.activity.observe(
            self.scope,
            ActivityEvidence {
                input: Some(self.input),
                observed_tap: facts.observed_tap,
                nonzero_tap: facts.nonzero_tap,
                sustained_zero_tap: facts.sustained_zero_tap,
                absent_tap_layout: facts.absent_tap_layout,
                suspected: facts.suspected,
                uncertain: facts.uncertain || facts.counter_saturated,
                complete: facts.terminal_complete.then_some(ActivityExtent {
                    end_frame: self.tracker.cursor,
                    all_zero: !facts.nonzero_tap,
                }),
                ..ActivityEvidence::default()
            },
        );
    }
}

/// Publish the open interval as soon as it is suspected, then extend it by coalescing. A rejected
/// run counts once, including every later snapshot/extension and its eventual closure. Exact
/// observed epoch binding is required; missing/capped/conflicting mappings retain only the fact.
fn publish_run(
    record: &mut QualityRecord,
    scope: ActivityScope,
    input: ActivityInput,
    publication: &mut RunPublication,
    run: SuspectedRun,
    observed: [Option<CaptureEpoch>; 2],
) -> bool {
    if let Some(old) = publication.run
        && old.epoch == run.epoch.id
        && old.start == run.start
        && (old.omitted || old.end >= run.end)
    {
        return !old.omitted;
    }
    let mapped = record
        .epochs
        .iter()
        .position(|epoch| epoch.id == run.epoch.id)
        .and_then(|index| {
            let epoch = record.epochs[index];
            let next = record.epochs.get(index + 1).copied();
            if !observed.contains(&Some(epoch))
                || next.is_some_and(|next| {
                    next.id != epoch.id.saturating_add(1) || run.end > next.retained_start_frame
                })
                || (next.is_none() && record.overflow.omitted_epochs != 0)
            {
                return None;
            }
            crate::quality_mapping::source_interval(epoch, next, run.start, run.end).map(
                |(start_frame, end_frame)| QualitySpan {
                    epoch: epoch.id,
                    start_frame,
                    end_frame,
                    track: QualityTrack::Tap,
                    cause: QualityCause::FarReferenceSilent,
                    class: QualityClass::DegradedOnly,
                    stage: scope.stage(),
                    resolution: QualityResolution::Outstanding,
                },
            )
        });
    let accepted = match mapped.map(|span| record.try_insert_span(span)) {
        Some(Ok(accepted)) => accepted,
        _ => false,
    };
    if !accepted {
        increment(
            &mut publication.omitted_runs,
            &mut publication.counter_saturated,
        );
        record
            .observe_activity_omissions(
                scope,
                input,
                publication.omitted_runs,
                publication.counter_saturated,
            )
            .expect("valid activity record");
    }
    publication.run = Some(PublishedRun {
        epoch: run.epoch.id,
        start: run.start,
        end: run.end,
        omitted: !accepted,
    });
    accepted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BatchRecoveryOutcome, CaptureEpochReason, MAX_QUALITY_SPANS, SourceCompleteness};

    fn capture_epoch(rate: u32) -> CaptureEpoch {
        CaptureEpoch {
            id: 0,
            source_start_ns: Some(0),
            source_rate_hz: rate,
            source_frame_base: 0,
            retained_start_frame: 0,
            retained_rate_hz: rate,
            mic_channels: 1,
            tap_channels: 1,
            reason: CaptureEpochReason::Start,
        }
    }

    fn record_tracker(scope: ActivityScope, rate: u32) -> (QualityRecord, ChannelActivity) {
        let mut record = QualityRecord::observed();
        record.insert_epoch(capture_epoch(rate)).unwrap();
        let tracker =
            ChannelActivity::new(scope, ActivityInput::new([1; 32], rate).unwrap()).unwrap();
        (record, tracker)
    }

    fn record_feed(
        tracker: &mut ChannelActivity,
        record: &mut QualityRecord,
        epoch: CaptureEpoch,
        start: u64,
        mic: &[f32],
        tap: &[f32],
        unavailable: Option<&[bool]>,
    ) {
        tracker
            .push(
                ActivityBlock {
                    epoch,
                    retained_start: start,
                    mic,
                    tap,
                    unavailable,
                },
                record,
            )
            .unwrap();
    }

    #[test]
    fn activity_publishes_open_run_before_finish_with_exact_degraded_stage_and_levels() {
        for scope in [
            ActivityScope::Capture,
            ActivityScope::Offline,
            ActivityScope::Live,
        ] {
            let (mut record, mut tracker) = record_tracker(scope, 4);
            record_feed(
                &mut tracker,
                &mut record,
                capture_epoch(4),
                0,
                &[0.1; 19],
                &[0.0; 19],
                Some(&[]),
            );
            assert!(record.spans.is_empty());
            record_feed(
                &mut tracker,
                &mut record,
                capture_epoch(4),
                19,
                &[0.1; 1],
                &[0.0; 1],
                Some(&[]),
            );
            assert_eq!(
                record.spans,
                [QualitySpan {
                    epoch: 0,
                    start_frame: 0,
                    end_frame: 20,
                    track: QualityTrack::Tap,
                    cause: QualityCause::FarReferenceSilent,
                    class: QualityClass::DegradedOnly,
                    stage: scope.stage(),
                    resolution: QualityResolution::Outstanding,
                }]
            );
            assert!(record.activity.get(scope).suspected());
            assert_eq!(
                record.activity.get(scope).far_reference(),
                FarReference::Unknown
            );
            let levels = tracker.latest_levels().unwrap();
            assert_eq!((levels.start, levels.end, levels.tap_observed), (16, 20, 4));
            assert_eq!(levels.tap_mean_square, Some(0.0));
            assert_eq!(levels.mic_mean_square, Some(f64::from(0.1_f32).powi(2)));
            record_feed(
                &mut tracker,
                &mut record,
                capture_epoch(4),
                20,
                &[0.1; 7],
                &[0.0; 7],
                Some(&[]),
            );
            tracker.finish(Some(27), &mut record).unwrap();
            assert_eq!(record.spans[0].end_frame, 24);
            assert_eq!(record.activity.get(scope).complete_end_frame(), Some(27));
            assert_eq!(
                record.activity.get(scope).far_reference(),
                FarReference::Absent
            );
            assert_eq!(record.completeness(), SourceCompleteness::Complete);
            assert!(!record.requires_batch_recovery());
            let frozen = record.clone();
            tracker.finish(None, &mut record).unwrap();
            record
                .resolve_processing_after_batch(BatchRecoveryOutcome::SameBackendWholeFileSuccess)
                .unwrap();
            assert_eq!(record, frozen);
        }
    }

    #[test]
    fn activity_actual_span_cap_counts_each_run_once_and_preserves_summary_before_finish() {
        let (mut record, mut tracker) = record_tracker(ActivityScope::Live, 1);
        for run in 0..300_u64 {
            let start = run * 12;
            record_feed(
                &mut tracker,
                &mut record,
                capture_epoch(1),
                start,
                &[0.1; 5],
                &[0.0; 5],
                Some(&[]),
            );
            // The cap rejection already exists at the threshold; extension/closure cannot recount.
            let omissions = run.saturating_sub(MAX_QUALITY_SPANS as u64 - 1);
            assert_eq!(record.overflow.omitted_spans[2], omissions);
            for offset in 5..11 {
                record_feed(
                    &mut tracker,
                    &mut record,
                    capture_epoch(1),
                    start + offset,
                    &[0.1],
                    &[0.0],
                    Some(&[]),
                );
                assert_eq!(record.overflow.omitted_spans[2], omissions);
            }
            record_feed(
                &mut tracker,
                &mut record,
                capture_epoch(1),
                start + 11,
                &[0.1],
                &[0.2],
                Some(&[]),
            );
            assert_eq!(record.overflow.omitted_spans, [0, 0, omissions]);
        }
        assert_eq!(record.spans.len(), MAX_QUALITY_SPANS);
        assert_eq!(record.overflow.omitted_spans, [0, 0, 44]);
        let open = serde_json::to_vec(&record).unwrap();
        let mut restarted: QualityRecord = serde_json::from_slice(&open).unwrap();
        restarted.merge_snapshot(&record).unwrap();
        assert_eq!(restarted, record);
        assert_eq!(
            record.activity.get(ActivityScope::Live).far_reference(),
            FarReference::Intermittent
        );
        assert_eq!(record.completeness(), SourceCompleteness::Complete);
        assert!(!record.requires_batch_recovery());
    }

    #[test]
    fn activity_same_input_retry_keeps_omission_lower_bound_and_stages_remain_distinct() {
        let (mut record, _) = record_tracker(ActivityScope::Live, 4);
        record.epochs.clear();
        record
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                7,
            )
            .unwrap();
        for (scope, identity, expected) in [
            (ActivityScope::Live, 1, 8),
            (ActivityScope::Live, 1, 8),
            (ActivityScope::Capture, 1, 9),
            (ActivityScope::Capture, 1, 9),
            (ActivityScope::Live, 2, 9),
        ] {
            let mut tracker =
                ChannelActivity::new(scope, ActivityInput::new([identity; 32], 4).unwrap())
                    .unwrap();
            record_feed(
                &mut tracker,
                &mut record,
                capture_epoch(4),
                0,
                &[0.1; 20],
                &[0.0; 20],
                Some(&[]),
            );
            tracker.finish(Some(20), &mut record).unwrap();
            assert_eq!(record.overflow.omitted_spans, [0, 0, expected]);
            let before = record.clone();
            record.merge_snapshot(&before).unwrap();
            assert_eq!(record, before);
        }
        assert_eq!(
            record.activity.get(ActivityScope::Live).far_reference(),
            FarReference::Unknown
        );
        assert_eq!(
            record.activity.get(ActivityScope::Capture).far_reference(),
            FarReference::Absent
        );
        assert_eq!(record.completeness(), SourceCompleteness::Complete);
        assert!(!record.requires_batch_recovery());
    }

    #[test]
    fn activity_span_dropped_by_merge_cap_is_not_recounted_by_same_input_retry() {
        let (mut record, mut tracker) = record_tracker(ActivityScope::Live, 4);
        record_feed(
            &mut tracker,
            &mut record,
            capture_epoch(4),
            2_000,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[]),
        );
        tracker.finish(None, &mut record).unwrap();
        assert_eq!(record.spans.len(), 1);
        let (mut full, _) = record_tracker(ActivityScope::Live, 4);
        for i in 0..MAX_QUALITY_SPANS as u64 {
            full.insert_span(QualitySpan {
                epoch: 0,
                start_frame: i * 4,
                end_frame: i * 4 + 1,
                track: QualityTrack::Both,
                cause: QualityCause::ForcedBoundary,
                class: QualityClass::DegradedOnly,
                stage: crate::QualityStage::Live,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        }
        record.merge_snapshot(&full).unwrap();
        assert_eq!(record.overflow.omitted_spans, [0, 0, 1]);
        assert_eq!(record.activity.get(ActivityScope::Live).omitted_runs(), 1);
        let frozen = record.clone();
        let mut retry =
            ChannelActivity::new(ActivityScope::Live, ActivityInput::new([1; 32], 4).unwrap())
                .unwrap();
        record_feed(
            &mut retry,
            &mut record,
            capture_epoch(4),
            2_000,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[]),
        );
        retry.finish(None, &mut record).unwrap();
        assert_eq!(record, frozen);
        record.merge_snapshot(&frozen).unwrap();
        assert_eq!(record, frozen);
    }

    #[test]
    fn activity_distinct_scope_merges_preserve_summed_lower_bounds_and_generic_history() {
        let runs = |scope, count| {
            let (mut record, mut tracker) = record_tracker(scope, 1);
            record.epochs.clear();
            for run in 0..count {
                record_feed(
                    &mut tracker,
                    &mut record,
                    capture_epoch(1),
                    run * 6,
                    &[0.1; 5],
                    &[0.0; 5],
                    Some(&[]),
                );
                record_feed(
                    &mut tracker,
                    &mut record,
                    capture_epoch(1),
                    run * 6 + 5,
                    &[0.1],
                    &[0.2],
                    Some(&[]),
                );
            }
            record
        };
        let mut capture = runs(ActivityScope::Capture, 5);
        let live = runs(ActivityScope::Live, 7);
        capture.merge_snapshot(&live).unwrap();
        assert_eq!(capture.overflow.omitted_spans, [0, 0, 12]);
        assert_eq!(
            capture.activity.get(ActivityScope::Capture).omitted_runs(),
            5
        );
        assert_eq!(capture.activity.get(ActivityScope::Live).omitted_runs(), 7);
        let frozen = capture.clone();
        capture.merge_snapshot(&live).unwrap();
        assert_eq!(capture, frozen);
        let mut generic = runs(ActivityScope::Offline, 0);
        generic
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                30,
            )
            .unwrap();
        capture.merge_snapshot(&generic).unwrap();
        assert_eq!(capture.overflow.omitted_spans, [0, 0, 30]);
        assert_eq!(
            capture.activity.get(ActivityScope::Capture).omitted_runs(),
            5
        );
        assert_eq!(capture.activity.get(ActivityScope::Live).omitted_runs(), 7);
        assert_eq!(capture.completeness(), SourceCompleteness::Complete);
        assert!(!capture.requires_batch_recovery());
    }

    #[test]
    fn activity_supplement_uses_authoritative_epochs_without_changing_source_authority() {
        let (mut supplement, mut tracker) = record_tracker(ActivityScope::Live, 1);
        record_feed(
            &mut tracker,
            &mut supplement,
            capture_epoch(1),
            0,
            &[0.1; 5],
            &[0.0; 5],
            Some(&[]),
        );
        tracker.finish(None, &mut supplement).unwrap();
        for mode in ["matching", "missing", "conflicting", "legacy", "full"] {
            let mut authority = if mode == "legacy" {
                QualityRecord::default()
            } else {
                QualityRecord::observed()
            };
            if mode != "missing" && mode != "legacy" {
                let mut epoch = capture_epoch(1);
                if mode == "conflicting" {
                    epoch.source_rate_hz = 2;
                }
                authority.insert_epoch(epoch).unwrap();
            }
            if mode == "full" {
                for index in 0..crate::MAX_QUALITY_SPANS {
                    authority
                        .insert_span(QualitySpan {
                            epoch: 0,
                            start_frame: 100 + index as u64 * 2,
                            end_frame: 101 + index as u64 * 2,
                            track: QualityTrack::Mic,
                            cause: QualityCause::ForcedBoundary,
                            class: QualityClass::DegradedOnly,
                            stage: crate::QualityStage::Live,
                            resolution: QualityResolution::Outstanding,
                        })
                        .unwrap();
                }
            }
            let before = authority.clone();
            authority.merge_activity_snapshot(&supplement).unwrap();
            assert_eq!(authority.epochs, before.epochs, "{mode}");
            assert_eq!(authority.completeness(), before.completeness(), "{mode}");
            assert_eq!(
                authority.requires_batch_recovery(),
                before.requires_batch_recovery(),
                "{mode}"
            );
            assert!(!authority.overflow.counter_saturated);
            assert_eq!(authority.overflow.omitted_epochs, 0);
            assert!(authority.activity.get(ActivityScope::Live).suspected());
            if mode == "missing" || mode == "conflicting" || mode == "legacy" {
                assert!(authority.spans.is_empty());
                assert_eq!(
                    authority.activity.get(ActivityScope::Live).omitted_runs(),
                    1
                );
            }
            let settled = authority.clone();
            authority.merge_activity_snapshot(&supplement).unwrap();
            assert_eq!(authority, settled, "{mode}");
            let wire = serde_json::to_vec(&authority).unwrap();
            assert_eq!(
                serde_json::from_slice::<QualityRecord>(&wire).unwrap(),
                authority
            );
        }
    }

    #[test]
    fn activity_actual_epoch_cap_retains_suspected_facts_and_one_omission_per_run() {
        let (mut record, mut tracker) = record_tracker(ActivityScope::Capture, 1);
        for id in 0..80 {
            let epoch = CaptureEpoch {
                id,
                source_start_ns: Some(u64::from(id) * 5_000_000_000),
                source_frame_base: u64::from(id) * 5,
                retained_start_frame: u64::from(id) * 5,
                reason: if id == 0 {
                    CaptureEpochReason::Start
                } else {
                    CaptureEpochReason::HostDiscontinuity
                },
                ..capture_epoch(1)
            };
            record.insert_epoch(epoch).unwrap();
            record_feed(
                &mut tracker,
                &mut record,
                epoch,
                u64::from(id) * 5,
                &[0.1; 5],
                &[0.0; 5],
                Some(&[]),
            );
        }
        tracker.finish(Some(400), &mut record).unwrap();
        assert_eq!(record.epochs.len(), 64);
        assert_eq!(record.spans.len(), 64);
        assert_eq!(record.overflow.omitted_epochs, 16);
        assert_eq!(record.overflow.omitted_spans, [0, 0, 16]);
        assert!(record.activity.get(ActivityScope::Capture).suspected());
        assert!(!record.requires_batch_recovery());
    }

    #[test]
    fn activity_projection_uses_observed_epoch_and_outward_source_rounding() {
        let (mut record, mut tracker) = record_tracker(ActivityScope::Live, 4);
        let first = CaptureEpoch {
            source_rate_hz: 7,
            source_frame_base: 100,
            ..capture_epoch(4)
        };
        record.epochs[0] = first;
        let second = CaptureEpoch {
            id: 1,
            source_start_ns: Some(100_000_000_000),
            source_frame_base: 700,
            retained_start_frame: 21,
            reason: CaptureEpochReason::HostDiscontinuity,
            ..first
        };
        record.insert_epoch(second).unwrap();
        record_feed(
            &mut tracker,
            &mut record,
            first,
            1,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[]),
        );
        record_feed(
            &mut tracker,
            &mut record,
            second,
            21,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[]),
        );
        tracker.finish(Some(41), &mut record).unwrap();
        assert_eq!(
            record
                .spans
                .iter()
                .map(|s| (s.epoch, s.start_frame, s.end_frame))
                .collect::<Vec<_>>(),
            [(0, 101, 137), (1, 700, 735)]
        );
        assert_eq!(
            record.activity.get(ActivityScope::Live).far_reference(),
            FarReference::Unknown
        );
        assert_eq!(record.overflow.omitted_spans, [0, 0, 0]);
        record.validate().unwrap();

        let (mut conflicting, mut tracker) = record_tracker(ActivityScope::Live, 4);
        conflicting.epochs[0].source_frame_base = 99;
        record_feed(
            &mut tracker,
            &mut conflicting,
            capture_epoch(4),
            0,
            &[0.1; 40],
            &[0.0; 40],
            Some(&[]),
        );
        tracker.finish(Some(40), &mut conflicting).unwrap();
        assert!(conflicting.spans.is_empty());
        assert_eq!(conflicting.overflow.omitted_spans, [0, 0, 1]);
        assert!(conflicting.activity.get(ActivityScope::Live).suspected());
    }

    #[test]
    fn activity_legacy_unknown_availability_does_not_observe_numerical_fill() {
        let (mut record, mut tracker) = record_tracker(ActivityScope::Offline, 4);
        record.coverage = crate::QualityCoverage::LegacyUnknown;
        record_feed(
            &mut tracker,
            &mut record,
            capture_epoch(4),
            0,
            &[0.1; 20],
            &[0.0; 20],
            None,
        );
        tracker.finish(Some(20), &mut record).unwrap();
        assert_eq!(tracker.latest_levels().unwrap().tap_observed, 0);
        assert!(record.spans.is_empty());
        assert!(!record.activity.get(ActivityScope::Offline).suspected());
        assert_eq!(
            record.activity.get(ActivityScope::Offline).far_reference(),
            FarReference::Unknown
        );
        assert_eq!(record.source_completeness(), SourceCompleteness::Unknown);
    }

    #[test]
    fn activity_complete_zero_proof_conflicts_with_nonzero_and_never_erases_uncertainty() {
        let (mut zeros, mut zero_tracker) = record_tracker(ActivityScope::Live, 4);
        record_feed(
            &mut zero_tracker,
            &mut zeros,
            capture_epoch(4),
            0,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[]),
        );
        zero_tracker.finish(Some(20), &mut zeros).unwrap();
        let (mut positive, mut positive_tracker) = record_tracker(ActivityScope::Live, 4);
        record_feed(
            &mut positive_tracker,
            &mut positive,
            capture_epoch(4),
            0,
            &[0.1],
            &[f32::from_bits(1)],
            Some(&[]),
        );
        let mut a = zeros.clone();
        a.merge_snapshot(&positive).unwrap();
        positive.merge_snapshot(&zeros).unwrap();
        assert_eq!(a, positive);
        assert_eq!(
            a.activity.get(ActivityScope::Live).far_reference(),
            FarReference::Unknown
        );
        assert!(a.activity.get(ActivityScope::Live).uncertain());
        a.merge_snapshot(&zeros).unwrap();
        assert_eq!(a, positive);

        let (mut partial, mut partial_tracker) = record_tracker(ActivityScope::Live, 4);
        record_feed(
            &mut partial_tracker,
            &mut partial,
            capture_epoch(4),
            0,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[true; 20]),
        );
        partial_tracker.finish(None, &mut partial).unwrap();
        partial.merge_snapshot(&zeros).unwrap();
        assert!(partial.activity.get(ActivityScope::Live).uncertain());
        assert_eq!(
            partial
                .activity
                .get(ActivityScope::Live)
                .complete_end_frame(),
            Some(20)
        );
        assert_eq!(
            partial.activity.get(ActivityScope::Live).far_reference(),
            FarReference::Absent
        );
    }

    #[test]
    fn activity_partial_union_never_certifies_absence_and_stage_bindings_do_not_pool() {
        let (mut first, mut tracker) = record_tracker(ActivityScope::Live, 4);
        record_feed(
            &mut tracker,
            &mut first,
            capture_epoch(4),
            0,
            &[0.1; 12],
            &[0.0; 12],
            Some(&[]),
        );
        let (mut second, mut tracker) = record_tracker(ActivityScope::Live, 4);
        record_feed(
            &mut tracker,
            &mut second,
            capture_epoch(4),
            12,
            &[0.1; 8],
            &[0.0; 8],
            Some(&[]),
        );
        tracker.finish(Some(20), &mut second).unwrap();
        first.merge_snapshot(&second).unwrap();
        assert_eq!(
            first.activity.get(ActivityScope::Live).complete_end_frame(),
            None
        );
        assert_eq!(
            first.activity.get(ActivityScope::Live).far_reference(),
            FarReference::Unknown
        );
        let (mut offline, mut tracker) = record_tracker(ActivityScope::Offline, 4);
        record_feed(
            &mut tracker,
            &mut offline,
            capture_epoch(4),
            0,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[]),
        );
        tracker.finish(Some(20), &mut offline).unwrap();
        first.merge_snapshot(&offline).unwrap();
        assert_eq!(
            first.activity.get(ActivityScope::Offline).far_reference(),
            FarReference::Absent
        );
        assert_eq!(
            first.activity.get(ActivityScope::Live).far_reference(),
            FarReference::Unknown
        );
        assert_eq!(
            first.activity.get(ActivityScope::Capture).far_reference(),
            FarReference::Unknown
        );
    }

    #[test]
    fn activity_complete_retry_preserves_uncertainty_but_supplies_its_own_extent() {
        let (mut partial, mut first) = record_tracker(ActivityScope::Offline, 4);
        record_feed(
            &mut first,
            &mut partial,
            capture_epoch(4),
            0,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[true; 20]),
        );
        first.finish(None, &mut partial).unwrap();
        assert!(partial.activity.get(ActivityScope::Offline).uncertain());
        assert_eq!(
            partial
                .activity
                .get(ActivityScope::Offline)
                .complete_end_frame(),
            None
        );

        let (mut complete, mut retry) = record_tracker(ActivityScope::Offline, 4);
        record_feed(
            &mut retry,
            &mut complete,
            capture_epoch(4),
            0,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[]),
        );
        retry.finish(Some(20), &mut complete).unwrap();
        partial.merge_snapshot(&complete).unwrap();
        assert!(partial.activity.get(ActivityScope::Offline).uncertain());
        assert_eq!(
            partial
                .activity
                .get(ActivityScope::Offline)
                .complete_end_frame(),
            Some(20)
        );
        assert_eq!(
            partial.activity.get(ActivityScope::Offline).far_reference(),
            FarReference::Absent
        );
        let wire = serde_json::to_vec(&partial).unwrap();
        assert_eq!(
            serde_json::from_slice::<QualityRecord>(&wire).unwrap(),
            partial
        );
        partial.merge_snapshot(&complete).unwrap();
        assert!(partial.activity.get(ActivityScope::Offline).uncertain());
        assert_eq!(
            partial.activity.get(ActivityScope::Offline).far_reference(),
            FarReference::Absent
        );
    }

    #[test]
    fn activity_conflicting_input_rate_or_extent_is_conservative_and_merge_is_associative() {
        let complete = |identity, end| {
            let (mut record, _) = record_tracker(ActivityScope::Live, 4);
            let mut tracker = ChannelActivity::new(
                ActivityScope::Live,
                ActivityInput::new([identity; 32], 4).unwrap(),
            )
            .unwrap();
            record_feed(
                &mut tracker,
                &mut record,
                capture_epoch(4),
                0,
                &vec![0.1; end],
                &vec![0.0; end],
                Some(&[]),
            );
            tracker.finish(Some(end as u64), &mut record).unwrap();
            record
        };
        let records = [complete(1, 20), complete(1, 21), complete(2, 20)];
        let mut expected = records[0].clone();
        expected.merge_snapshot(&records[1]).unwrap();
        expected.merge_snapshot(&records[2]).unwrap();
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let mut actual = records[order[0]].clone();
            actual.merge_snapshot(&records[order[1]]).unwrap();
            actual.merge_snapshot(&records[order[2]]).unwrap();
            actual.merge_snapshot(&actual.clone()).unwrap();
            assert_eq!(actual, expected);
        }
        assert_eq!(
            expected.activity.get(ActivityScope::Live).far_reference(),
            FarReference::Unknown
        );
        assert_eq!(
            expected
                .activity
                .get(ActivityScope::Live)
                .complete_end_frame(),
            None
        );
        let (mut rate_record, mut tracker) = record_tracker(ActivityScope::Live, 4);
        assert_eq!(
            tracker.push(
                ActivityBlock {
                    epoch: capture_epoch(8),
                    retained_start: 0,
                    mic: &[0.1; 40],
                    tap: &[0.0; 40],
                    unavailable: Some(&[])
                },
                &mut rate_record
            ),
            Err(ActivityInputError::InvalidEpoch)
        );
        assert!(rate_record.activity.get(ActivityScope::Live).uncertain());
        assert!(rate_record.spans.is_empty());
    }

    #[test]
    fn activity_actual_record_serde_defaults_and_fixed_memory_reject_malformed_proofs() {
        let (record, _) = record_tracker(ActivityScope::Live, 4);
        let mut legacy = serde_json::to_value(&record).unwrap();
        legacy.as_object_mut().unwrap().remove("activity");
        let decoded: QualityRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.activity, crate::QualityActivity::default());
        assert_eq!(decoded.schema_version, 1);
        for value in [
            serde_json::json!({"suspected": true}),
            serde_json::json!({"input": {"identity": [0], "retained_rate_hz": 4}}),
        ] {
            let mut malformed = serde_json::to_value(&record).unwrap();
            malformed["activity"]["live"] = value;
            assert!(serde_json::from_value::<QualityRecord>(malformed).is_err());
        }
        assert!(!std::mem::needs_drop::<ChannelActivity>());
        assert!(std::mem::size_of::<ChannelActivity>() <= 512);
        assert!(!std::mem::needs_drop::<crate::QualityActivity>());
        assert!(std::mem::size_of::<crate::QualityActivity>() <= 384);
    }

    #[test]
    fn activity_omission_saturation_does_not_change_source_completeness_or_recovery() {
        let (mut record, mut tracker) = record_tracker(ActivityScope::Live, 4);
        record.overflow.omitted_spans[2] = u64::MAX;
        record.overflow.outstanding_omitted_spans[2] = u64::MAX;
        // Deliberate absent mapping retains only the qualitative evidence and one omission.
        record.epochs.clear();
        record_feed(
            &mut tracker,
            &mut record,
            capture_epoch(4),
            0,
            &[0.1; 40],
            &[0.0; 40],
            Some(&[]),
        );
        tracker.finish(Some(40), &mut record).unwrap();
        assert_eq!(record.overflow.omitted_spans, [0, 0, u64::MAX]);
        assert!(record.activity.omissions_saturated());
        assert!(!record.overflow.counter_saturated);
        assert_eq!(record.source_completeness(), SourceCompleteness::Complete);
        assert!(!record.requires_batch_recovery());
        let mut decoded: QualityRecord =
            serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        decoded.merge_snapshot(&record).unwrap();
        assert_eq!(decoded, record);
        // Existing source/processing uncertainty is never cleared by the diagnostic path.
        decoded.overflow.counter_saturated = true;
        let mut retry =
            ChannelActivity::new(ActivityScope::Live, ActivityInput::new([1; 32], 4).unwrap())
                .unwrap();
        record_feed(
            &mut retry,
            &mut decoded,
            capture_epoch(4),
            0,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[]),
        );
        assert!(decoded.overflow.counter_saturated);
        assert_eq!(decoded.source_completeness(), SourceCompleteness::Unknown);
    }

    #[test]
    fn activity_invalid_proofs_reject_on_both_record_serde_paths_without_payload_diagnostics() {
        let (mut valid, mut tracker) = record_tracker(ActivityScope::Live, 4);
        record_feed(
            &mut tracker,
            &mut valid,
            capture_epoch(4),
            0,
            &[0.1; 20],
            &[0.0; 20],
            Some(&[]),
        );
        tracker.finish(Some(20), &mut valid).unwrap();
        for fault in 0..5 {
            let mut bad = valid.clone();
            let mut evidence = *bad.activity.get(ActivityScope::Live);
            match fault {
                0 => evidence.complete.as_mut().unwrap().end_frame = 0,
                1 => evidence.nonzero_tap = true,
                2 => evidence.observed_tap = false,
                3 => evidence.input = None,
                _ => evidence.complete.as_mut().unwrap().all_zero = false,
            }
            // Replace this test's scope using serde, then exercise actual record validation.
            let mut wire = serde_json::to_value(&valid).unwrap();
            wire["activity"]["live"] = serde_json::to_value(evidence).unwrap();
            let error = serde_json::from_value::<QualityRecord>(wire).unwrap_err();
            assert_eq!(error.to_string(), "invalid quality activity evidence");
            bad.activity = serde_json::from_value(serde_json::json!({"live": evidence})).unwrap();
            assert_eq!(
                serde_json::to_vec(&bad).unwrap_err().to_string(),
                "invalid quality activity evidence"
            );
        }
    }

    fn epoch(rate: u32) -> Epoch {
        Epoch {
            id: 0,
            retained_start: 0,
            rate,
            mic_expected: true,
            tap_expected: true,
        }
    }

    fn feed(
        tracker: &mut Activity,
        epoch: Epoch,
        start: u64,
        mic: &[f32],
        tap: &[f32],
        unavailable: &[bool],
        runs: &mut Vec<SuspectedRun>,
    ) {
        tracker
            .push(
                Block {
                    epoch,
                    start,
                    mic,
                    tap,
                    unavailable,
                    availability_known: true,
                },
                &mut |run| {
                    runs.push(run);
                    true
                },
            )
            .unwrap();
    }

    fn constant(
        tracker: &mut Activity,
        ep: Epoch,
        start: u64,
        frames: usize,
        values: (f32, f32),
        runs: &mut Vec<SuspectedRun>,
    ) {
        let mic = vec![values.0; if ep.mic_expected { frames } else { 0 }];
        let tap = vec![values.1; if ep.tap_expected { frames } else { 0 }];
        feed(tracker, ep, start, &mic, &tap, &vec![false; frames], runs);
    }

    fn finish(tracker: &mut Activity, end: Option<u64>, runs: &mut Vec<SuspectedRun>) {
        tracker.finish(end, &mut |run| {
            runs.push(run);
            true
        });
    }

    #[test]
    fn dead_tap_active_mic_opens_at_five_seconds_and_finishes_once() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let ep = epoch(10);
        constant(&mut tracker, ep, 0, 49, (0.1, -0.0), &mut runs);
        assert!(!tracker.facts.suspected);
        constant(&mut tracker, ep, 49, 1, (0.1, 0.0), &mut runs);
        assert_eq!(
            tracker.active_run,
            Some(SuspectedRun {
                epoch: ep,
                start: 0,
                end: 50
            })
        );
        assert_eq!(tracker.facts.far_reference(), FarReference::Unknown);
        constant(&mut tracker, ep, 50, 10, (0.1, 0.0), &mut runs);
        finish(&mut tracker, Some(60), &mut runs);
        finish(&mut tracker, Some(60), &mut runs);
        assert_eq!(
            runs,
            vec![SuspectedRun {
                epoch: ep,
                start: 0,
                end: 60
            }]
        );
        assert_eq!(tracker.closed_runs, 1);
        assert_eq!(tracker.facts.far_reference(), FarReference::Absent);
        assert_eq!(SUSPECTED_LABEL, "suspected silent far reference");
    }

    #[test]
    fn both_quiet_is_not_suspected_and_levels_are_measured() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        constant(&mut tracker, epoch(10), 0, 70, (0.0, -0.0), &mut runs);
        finish(&mut tracker, Some(70), &mut runs);
        assert!(!tracker.facts.suspected);
        assert!(runs.is_empty());
        assert!(tracker.facts.sustained_zero_tap);
        assert_eq!(tracker.facts.far_reference(), FarReference::Absent);
        let levels = tracker.last_levels.unwrap();
        assert_eq!(
            (levels.mic_mean_square, levels.tap_mean_square),
            (Some(0.0), Some(0.0))
        );
        assert_eq!((levels.mic_observed, levels.tap_observed), (10, 10));
    }

    #[test]
    fn intermittent_keeps_global_evidence_and_partial_nonzero_closes_run() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let ep = epoch(10);
        constant(&mut tracker, ep, 0, 10, (0.1, 0.2), &mut runs);
        constant(&mut tracker, ep, 10, 50, (0.1, 0.0), &mut runs);
        constant(&mut tracker, ep, 60, 1, (0.1, f32::from_bits(1)), &mut runs);
        assert_eq!(
            runs,
            vec![SuspectedRun {
                epoch: ep,
                start: 10,
                end: 60
            }]
        );
        finish(&mut tracker, Some(61), &mut runs);
        assert_eq!(tracker.facts.far_reference(), FarReference::Intermittent);
        assert!(!tracker.last_levels.unwrap().complete_second);
        assert!(tracker.last_levels.unwrap().tap_peak.unwrap() > 0.0);
    }

    #[test]
    fn quiet_zero_stretch_is_intermittent_without_suspicion() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        constant(&mut tracker, epoch(4), 0, 20, (0.0, 0.0), &mut runs);
        constant(&mut tracker, epoch(4), 20, 1, (0.0, 0.2), &mut runs);
        assert_eq!(tracker.facts.far_reference(), FarReference::Intermittent);
        assert!(!tracker.facts.suspected);
        assert!(runs.is_empty());
    }

    #[test]
    fn short_zero_stretches_do_not_sum_into_sustained_silence() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        for second in 0..12 {
            let tap = if second % 4 == 3 { 0.2 } else { 0.0 };
            constant(&mut tracker, epoch(4), second * 4, 4, (0.1, tap), &mut runs);
        }
        finish(&mut tracker, Some(48), &mut runs);
        assert!(!tracker.facts.suspected);
        assert!(!tracker.facts.sustained_zero_tap);
        assert_eq!(tracker.facts.far_reference(), FarReference::Present);
    }

    #[test]
    fn unavailable_fill_is_unknown_and_a_later_observed_run_can_trigger() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let ep = epoch(4);
        feed(
            &mut tracker,
            ep,
            0,
            &[0.1; 20],
            &[0.0; 20],
            &[true; 20],
            &mut runs,
        );
        assert!(!tracker.facts.observed_tap);
        assert_eq!(tracker.last_levels.unwrap().tap_mean_square, None);
        assert!(!tracker.facts.suspected);
        constant(&mut tracker, ep, 20, 20, (0.1, 0.0), &mut runs);
        finish(&mut tracker, Some(40), &mut runs);
        assert_eq!(
            runs,
            vec![SuspectedRun {
                epoch: ep,
                start: 20,
                end: 40
            }]
        );
        assert!(tracker.facts.suspected && tracker.facts.uncertain);
        assert_eq!(tracker.facts.far_reference(), FarReference::Unknown);
    }

    #[test]
    fn invalid_and_short_tracks_never_become_silence_or_active_mic() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let ep = epoch(4);
        feed(
            &mut tracker,
            ep,
            0,
            &[f32::NAN; 20],
            &[0.0; 20],
            &[false; 20],
            &mut runs,
        );
        assert_eq!(tracker.last_levels.unwrap().mic_mean_square, None);
        feed(
            &mut tracker,
            ep,
            20,
            &[0.1; 20],
            &[f32::INFINITY; 19],
            &[false; 20],
            &mut runs,
        );
        finish(&mut tracker, Some(40), &mut runs);
        assert!(runs.is_empty());
        assert!(!tracker.facts.suspected);
        assert_eq!(tracker.facts.far_reference(), FarReference::Unknown);
        assert_eq!(tracker.last_levels.unwrap().tap_observed, 0);
    }

    fn assert_tap_history_survives_invalid_mic(mic: &[f32]) {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let ep = epoch(4);
        constant(&mut tracker, ep, 0, 4, (0.1, 0.2), &mut runs);
        feed(
            &mut tracker,
            ep,
            4,
            mic,
            &[0.0; 20],
            &[false; 20],
            &mut runs,
        );
        finish(&mut tracker, Some(24), &mut runs);
        assert_eq!(tracker.facts.far_reference(), FarReference::Intermittent);
        assert!(tracker.facts.sustained_zero_tap);
        assert!(tracker.facts.observed_tap && tracker.facts.nonzero_tap);
        assert!(tracker.facts.uncertain);
        assert!(!tracker.facts.terminal_complete);
        assert!(!tracker.facts.suspected);
        assert!(tracker.active_run.is_none() && runs.is_empty());
        let levels = tracker.last_levels.unwrap();
        assert_eq!((levels.mic_observed, levels.tap_observed), (0, 4));
        assert_eq!(levels.tap_mean_square, Some(0.0));
    }

    #[test]
    fn observed_tap_zero_streak_survives_nonfinite_mic() {
        assert_tap_history_survives_invalid_mic(&[f32::NAN; 20]);
    }

    #[test]
    fn observed_tap_zero_streak_survives_missing_mic() {
        assert_tap_history_survives_invalid_mic(&[]);
    }

    #[test]
    fn mic_invalidity_breaks_suspicion_but_preserves_reference_history() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let ep = epoch(4);
        constant(&mut tracker, ep, 0, 4, (0.1, 0.2), &mut runs);
        constant(&mut tracker, ep, 4, 16, (0.1, 0.0), &mut runs);
        feed(
            &mut tracker,
            ep,
            20,
            &[f32::NAN; 4],
            &[0.0; 4],
            &[false; 4],
            &mut runs,
        );
        assert_eq!(tracker.facts.far_reference(), FarReference::Intermittent);
        assert!(!tracker.facts.suspected);
        constant(&mut tracker, ep, 24, 16, (0.1, 0.0), &mut runs);
        assert!(!tracker.facts.suspected);
        constant(&mut tracker, ep, 40, 4, (0.1, 0.0), &mut runs);
        finish(&mut tracker, Some(44), &mut runs);
        assert_eq!(
            runs,
            vec![SuspectedRun {
                epoch: ep,
                start: 24,
                end: 44,
            }]
        );
        assert!(tracker.facts.uncertain && !tracker.facts.terminal_complete);
    }

    #[test]
    fn invalid_tap_missing_tap_and_unavailable_fill_still_break_zero_history() {
        for kind in 0..3 {
            let (mut tracker, mut runs) = (Activity::default(), Vec::new());
            let ep = epoch(4);
            constant(&mut tracker, ep, 0, 4, (0.1, 0.2), &mut runs);
            feed(
                &mut tracker,
                ep,
                4,
                &[f32::NAN; 16],
                &[0.0; 16],
                &[false; 16],
                &mut runs,
            );
            let tap: &[f32] = match kind {
                0 => &[f32::NAN],
                1 => &[],
                _ => &[0.0],
            };
            feed(&mut tracker, ep, 20, &[0.1], tap, &[kind == 2], &mut runs);
            feed(
                &mut tracker,
                ep,
                21,
                &[f32::NAN; 19],
                &[0.0; 19],
                &[false; 19],
                &mut runs,
            );
            assert!(!tracker.facts.sustained_zero_tap);
            assert_eq!(tracker.facts.far_reference(), FarReference::Present);
            feed(
                &mut tracker,
                ep,
                40,
                &[f32::NAN; 4],
                &[0.0; 4],
                &[false; 4],
                &mut runs,
            );
            finish(&mut tracker, Some(44), &mut runs);
            assert!(tracker.facts.sustained_zero_tap);
            assert_eq!(tracker.facts.far_reference(), FarReference::Intermittent);
            assert!(!tracker.facts.suspected);
            assert!(runs.is_empty());
        }
    }

    #[test]
    fn empty_mask_matches_explicit_all_available_without_an_adapter_bitmap() {
        let mut expected = None;
        for unavailable in [&[false; 24][..], &[][..]] {
            let (mut tracker, mut runs) = (Activity::default(), Vec::new());
            feed(
                &mut tracker,
                epoch(4),
                0,
                &[0.1; 24],
                &[0.0; 24],
                unavailable,
                &mut runs,
            );
            finish(&mut tracker, Some(24), &mut runs);
            assert!(tracker.facts.suspected && tracker.facts.terminal_complete);
            let actual = (tracker, runs);
            if let Some(expected) = &expected {
                assert_eq!(&actual, expected);
            } else {
                expected = Some(actual);
            }
        }
    }

    #[test]
    fn one_missing_tap_frame_cannot_be_padded_to_the_threshold() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let ep = epoch(4);
        feed(
            &mut tracker,
            ep,
            0,
            &[0.1; 20],
            &[0.0; 19],
            &[false; 20],
            &mut runs,
        );
        assert!(!tracker.facts.suspected);
        assert_eq!(tracker.last_levels.unwrap().tap_observed, 3);
        constant(&mut tracker, ep, 20, 20, (0.1, 0.0), &mut runs);
        finish(&mut tracker, Some(40), &mut runs);
        assert_eq!(
            runs,
            vec![SuspectedRun {
                epoch: ep,
                start: 20,
                end: 40
            }]
        );
        assert_eq!(tracker.facts.far_reference(), FarReference::Unknown);
    }

    #[test]
    fn activity_accepted_minus_sixty_dbfs_threshold_has_exact_f32_boundary_controls() {
        assert_eq!(UNCALIBRATED_MIC_ACTIVE_DBFS, -60.0);
        assert_eq!(UNCALIBRATED_SUSTAINED_SECONDS, 5);
        let above = 0.001_f32;
        let below = f32::from_bits(above.to_bits() - 1);
        assert!(f64::from(below).powi(2) < UNCALIBRATED_MIC_ACTIVE_MEAN_SQUARE);
        assert!(f64::from(above).powi(2) >= UNCALIBRATED_MIC_ACTIVE_MEAN_SQUARE);
        for (mic, suspected) in [(below, false), (above, true)] {
            let (mut record, mut tracker) = record_tracker(ActivityScope::Live, 4);
            record_feed(
                &mut tracker,
                &mut record,
                capture_epoch(4),
                0,
                &[mic; 19],
                &[0.0; 19],
                Some(&[]),
            );
            assert!(!record.activity.get(ActivityScope::Live).suspected());
            record_feed(
                &mut tracker,
                &mut record,
                capture_epoch(4),
                19,
                &[mic],
                &[0.0],
                Some(&[]),
            );
            assert_eq!(
                record.activity.get(ActivityScope::Live).suspected(),
                suspected
            );
            assert_eq!(record.spans.len(), usize::from(suspected));
        }
    }

    #[test]
    fn threshold_is_mean_square_not_peak_and_is_explicitly_uncalibrated() {
        assert!(
            (10.0_f64.powf(UNCALIBRATED_MIC_ACTIVE_DBFS / 10.0)
                - UNCALIBRATED_MIC_ACTIVE_MEAN_SQUARE)
                .abs()
                < 1e-18
        );
        for (amplitude, expected) in [(0.0009, false), (0.0011, true)] {
            let (mut tracker, mut runs) = (Activity::default(), Vec::new());
            constant(&mut tracker, epoch(10), 0, 50, (amplitude, 0.0), &mut runs);
            assert_eq!(tracker.facts.suspected, expected);
        }
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let mut mic = [0.0; 50];
        for i in (0..50).step_by(10) {
            mic[i] = 0.002;
        }
        feed(
            &mut tracker,
            epoch(10),
            0,
            &mic,
            &[0.0; 50],
            &[false; 50],
            &mut runs,
        );
        assert!(!tracker.facts.suspected);
        assert_eq!(tracker.last_levels.unwrap().mic_peak, Some(0.002));
    }

    #[test]
    fn waveform_partition_sweep_keeps_identical_levels_runs_and_facts() {
        for rate in [7, 44_100] {
            let n = (rate * 13 + 3) as usize;
            let mic = vec![0.1; n];
            let tap: Vec<_> = (0..n)
                .map(|i| if i / rate as usize == 6 { 0.2 } else { 0.0 })
                .collect();
            let mut unavailable = vec![false; n];
            unavailable[rate as usize * 12] = true;
            let mut expected = None;
            for chunks in [&[n][..], &[1], &[2, 13, 257, 4096], &[31, 17, 3], &[44_100]] {
                let (mut tracker, mut runs) = (Activity::default(), Vec::new());
                let (mut start, mut chunk) = (0, 0);
                while start < n {
                    let end = (start + chunks[chunk % chunks.len()]).min(n);
                    feed(
                        &mut tracker,
                        epoch(rate),
                        start as u64,
                        &mic[start..end],
                        &tap[start..end],
                        &unavailable[start..end],
                        &mut runs,
                    );
                    start = end;
                    chunk += 1;
                }
                finish(&mut tracker, Some(n as u64), &mut runs);
                let actual = (tracker, runs);
                if let Some(expected) = &expected {
                    assert_eq!(&actual, expected);
                } else {
                    expected = Some(actual);
                }
            }
            let (_, runs) = expected.unwrap();
            assert_eq!(runs.len(), 2);
            assert_eq!((runs[0].start, runs[0].end), (0, u64::from(rate) * 6));
            assert_eq!(
                (runs[1].start, runs[1].end),
                (u64::from(rate) * 7, u64::from(rate) * 12)
            );
        }
    }

    #[test]
    fn rate_and_epoch_changes_restart_bins_without_bridging_source_jumps() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        constant(
            &mut tracker,
            epoch(44_100),
            0,
            44_100 * 4,
            (0.1, 0.0),
            &mut runs,
        );
        let ep = Epoch {
            id: 1,
            retained_start: 176_400,
            ..epoch(48_000)
        };
        constant(&mut tracker, ep, 176_400, 48_000 * 5, (0.1, 0.0), &mut runs);
        let next = Epoch {
            id: 2,
            retained_start: 416_400,
            ..ep
        };
        constant(&mut tracker, next, 416_400, 48_000, (0.1, 0.0), &mut runs);
        finish(&mut tracker, Some(464_400), &mut runs);
        assert_eq!(
            runs,
            vec![SuspectedRun {
                epoch: ep,
                start: 176_400,
                end: 416_400
            }]
        );
        assert_eq!(tracker.completed_bins, 10);
        assert!(tracker.facts.terminal_complete);
    }

    #[test]
    fn conflicting_or_reused_epoch_is_rejected_without_counting_samples() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        constant(&mut tracker, epoch(4), 0, 16, (0.1, 0.0), &mut runs);
        let bad = Block {
            epoch: epoch(8),
            start: 16,
            mic: &[0.1; 8],
            tap: &[0.0; 8],
            unavailable: &[false; 8],
            availability_known: true,
        };
        assert_eq!(
            tracker.push(bad, &mut |_| true),
            Err(ActivityInputError::InvalidEpoch)
        );
        assert_eq!(tracker.cursor, 16);
        assert!(!tracker.facts.suspected);
        assert!(tracker.facts.uncertain);
        let ep = Epoch {
            id: 2,
            retained_start: 16,
            ..epoch(4)
        };
        constant(&mut tracker, ep, 16, 4, (0.1, 0.0), &mut runs);
        let reused = Block {
            epoch: epoch(4),
            start: 20,
            mic: &[0.1; 4],
            tap: &[0.0; 4],
            unavailable: &[false; 4],
            availability_known: true,
        };
        assert_eq!(
            tracker.push(reused, &mut |_| true),
            Err(ActivityInputError::InvalidEpoch)
        );
    }

    #[test]
    fn final_partial_second_neither_opens_nor_extends_suspicion() {
        for (frames, expected_end) in [(49, None), (59, Some(50))] {
            let (mut tracker, mut runs) = (Activity::default(), Vec::new());
            constant(&mut tracker, epoch(10), 0, frames, (0.1, 0.0), &mut runs);
            finish(&mut tracker, Some(frames as u64), &mut runs);
            assert_eq!(runs.first().map(|run| run.end), expected_end);
            let levels = tracker.last_levels.unwrap();
            assert!(!levels.complete_second);
            assert_eq!(levels.mic_observed, 9);
            assert_eq!(levels.end, frames as u64);
            assert!(tracker.facts.terminal_complete);
        }
    }

    #[test]
    fn retained_holes_close_runs_in_constant_state_without_synthetic_seconds() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let ep = epoch(2);
        constant(&mut tracker, ep, 0, 10, (0.1, 0.0), &mut runs);
        constant(
            &mut tracker,
            ep,
            1_000_000_000_000,
            10,
            (0.1, 0.0),
            &mut runs,
        );
        finish(&mut tracker, Some(1_000_000_000_010), &mut runs);
        assert_eq!(
            runs.iter().map(|r| (r.start, r.end)).collect::<Vec<_>>(),
            vec![(0, 10), (1_000_000_000_000, 1_000_000_000_010)]
        );
        assert_eq!(tracker.completed_bins, 10);
        assert_eq!(tracker.facts.far_reference(), FarReference::Unknown);
    }

    #[test]
    fn leading_or_trailing_holes_missing_verdict_and_empty_input_cannot_certify_absent() {
        for (start, end) in [(1, Some(21)), (0, Some(21)), (0, None)] {
            let (mut tracker, mut runs) = (Activity::default(), Vec::new());
            constant(&mut tracker, epoch(4), start, 20, (0.0, 0.0), &mut runs);
            finish(&mut tracker, end, &mut runs);
            assert_eq!(tracker.facts.far_reference(), FarReference::Unknown);
            let observed_end = tracker.cursor;
            finish(&mut tracker, Some(observed_end), &mut runs);
            assert_eq!(tracker.facts.far_reference(), FarReference::Unknown);
        }
        let mut empty = Activity::default();
        empty.finish(Some(0), &mut |_| panic!("empty input emits no run"));
        assert_eq!(empty.facts.far_reference(), FarReference::Unknown);
    }

    #[test]
    fn structurally_absent_tracks_are_not_observed_zeros_and_never_suspected() {
        for (mic_expected, tap_expected) in [(true, false), (false, true)] {
            let (mut tracker, mut runs) = (Activity::default(), Vec::new());
            let ep = Epoch {
                mic_expected,
                tap_expected,
                ..epoch(4)
            };
            constant(&mut tracker, ep, 0, 20, (0.1, 0.0), &mut runs);
            finish(&mut tracker, Some(20), &mut runs);
            assert!(!tracker.facts.suspected);
            assert!(runs.is_empty());
            assert_eq!(tracker.facts.observed_tap, tap_expected);
            assert_eq!(
                tracker.last_levels.unwrap().mic_observed,
                if mic_expected { 4 } else { 0 }
            );
            assert_eq!(tracker.facts.far_reference(), FarReference::Absent);
        }
    }

    #[test]
    fn absent_layout_then_positive_tap_is_intermittent() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        constant(
            &mut tracker,
            Epoch {
                tap_expected: false,
                ..epoch(4)
            },
            0,
            1,
            (0.1, 0.0),
            &mut runs,
        );
        constant(
            &mut tracker,
            Epoch {
                id: 1,
                retained_start: 1,
                ..epoch(4)
            },
            1,
            1,
            (0.1, 0.2),
            &mut runs,
        );
        assert_eq!(tracker.facts.far_reference(), FarReference::Intermittent);
        assert!(!tracker.facts.suspected);
    }

    #[test]
    fn bounded_sink_counts_each_omitted_run_once_including_a_long_run() {
        let mut tracker = Activity::default();
        let mut kept = [None; 256];
        let mut calls = 0;
        let mut emit = |run| {
            calls += 1;
            if let Some(slot) = kept.iter_mut().find(|s| s.is_none()) {
                *slot = Some(run);
                true
            } else {
                false
            }
        };
        for run in 0..300 {
            let start = run * 24;
            tracker
                .push(
                    Block {
                        epoch: epoch(2),
                        start,
                        mic: &[0.1; 22],
                        tap: &[0.0; 22],
                        unavailable: &[false; 22],
                        availability_known: true,
                    },
                    &mut emit,
                )
                .unwrap();
            tracker
                .push(
                    Block {
                        epoch: epoch(2),
                        start: start + 22,
                        mic: &[0.1; 2],
                        tap: &[0.2; 2],
                        unavailable: &[false; 2],
                        availability_known: true,
                    },
                    &mut emit,
                )
                .unwrap();
        }
        tracker.finish(Some(7200), &mut emit);
        assert_eq!(calls, 300);
        assert_eq!(tracker.closed_runs, 300);
        assert_eq!(tracker.omitted_runs, 44);
        assert!(tracker.facts.suspected);
        assert_eq!(tracker.facts.far_reference(), FarReference::Intermittent);
        assert!(!std::mem::needs_drop::<Activity>());
        assert!(std::mem::size_of::<Activity>() <= 512);
        println!(
            "Activity state: {} bytes, no heap-owned fields",
            std::mem::size_of::<Activity>()
        );
    }

    #[test]
    fn external_epoch_binding_cap_rejects_once_per_run_without_growing_tracker() {
        let mut tracker = Activity::default();
        let mut calls = 0;
        let mut emit = |run: SuspectedRun| {
            calls += 1;
            run.epoch.id < 64
        };
        for id in 0..80 {
            let start = u64::from(id) * 10;
            tracker
                .push(
                    Block {
                        epoch: Epoch {
                            id,
                            retained_start: start,
                            ..epoch(2)
                        },
                        start,
                        mic: &[0.1; 10],
                        tap: &[0.0; 10],
                        unavailable: &[false; 10],
                        availability_known: true,
                    },
                    &mut emit,
                )
                .unwrap();
        }
        tracker.finish(Some(800), &mut emit);
        assert_eq!(calls, 80);
        assert_eq!((tracker.closed_runs, tracker.omitted_runs), (80, 16));
        assert!(tracker.facts.suspected);
    }

    #[test]
    fn replayed_positions_do_not_double_count_and_empty_pushes_do_not_reset() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let ep = epoch(4);
        constant(&mut tracker, ep, 0, 16, (0.1, 0.0), &mut runs);
        feed(
            &mut tracker,
            Epoch {
                id: 1,
                retained_start: 16,
                ..ep
            },
            16,
            &[],
            &[],
            &[],
            &mut runs,
        );
        constant(&mut tracker, ep, 16, 4, (0.1, 0.0), &mut runs);
        assert_eq!(tracker.active_run.unwrap().start, 0);
        let replay = Block {
            epoch: ep,
            start: 16,
            mic: &[0.1; 4],
            tap: &[0.0; 4],
            unavailable: &[false; 4],
            availability_known: true,
        };
        assert_eq!(
            tracker.push(replay, &mut |run| {
                runs.push(run);
                true
            }),
            Err(ActivityInputError::BackwardPosition)
        );
        assert_eq!(
            (tracker.cursor, tracker.completed_bins, tracker.closed_runs),
            (20, 5, 1)
        );
        finish(&mut tracker, Some(20), &mut runs);
        assert_eq!(runs.len(), 1);
        assert_eq!(tracker.facts.far_reference(), FarReference::Unknown);
    }

    #[test]
    fn counter_overflow_saturates_and_does_not_wrap_or_erase_positive_facts() {
        let mut tracker = Activity {
            completed_bins: u64::MAX,
            closed_runs: u64::MAX,
            omitted_runs: u64::MAX,
            ..Activity::default()
        };
        tracker
            .push(
                Block {
                    epoch: epoch(2),
                    start: 0,
                    mic: &[0.1; 20],
                    tap: &[0.0; 20],
                    unavailable: &[false; 20],
                    availability_known: true,
                },
                &mut |_| false,
            )
            .unwrap();
        tracker.finish(Some(20), &mut |_| false);
        assert_eq!(
            (
                tracker.completed_bins,
                tracker.closed_runs,
                tracker.omitted_runs
            ),
            (u64::MAX, u64::MAX, u64::MAX)
        );
        assert!(tracker.facts.counter_saturated && tracker.facts.suspected);
    }

    #[test]
    fn malformed_input_and_position_overflow_fail_without_admission() {
        let bads = [
            (
                Block {
                    epoch: epoch(0),
                    start: 0,
                    mic: &[0.1],
                    tap: &[0.0],
                    unavailable: &[false],
                    availability_known: true,
                },
                ActivityInputError::InvalidEpoch,
            ),
            (
                Block {
                    epoch: epoch(MAX_RETAINED_RATE_HZ + 1),
                    start: 0,
                    mic: &[0.1],
                    tap: &[0.0],
                    unavailable: &[false],
                    availability_known: true,
                },
                ActivityInputError::InvalidEpoch,
            ),
            (
                Block {
                    epoch: epoch(4),
                    start: 0,
                    mic: &[0.1; 2],
                    tap: &[0.0; 2],
                    unavailable: &[false],
                    availability_known: true,
                },
                ActivityInputError::InvalidAvailability,
            ),
            (
                Block {
                    epoch: Epoch {
                        tap_expected: false,
                        ..epoch(4)
                    },
                    start: 0,
                    mic: &[0.1],
                    tap: &[0.0],
                    unavailable: &[false],
                    availability_known: true,
                },
                ActivityInputError::UnexpectedTrack,
            ),
            (
                Block {
                    epoch: epoch(4),
                    start: u64::MAX,
                    mic: &[0.1],
                    tap: &[0.0],
                    unavailable: &[false],
                    availability_known: true,
                },
                ActivityInputError::PositionOverflow,
            ),
        ];
        for (block, expected) in bads {
            let mut tracker = Activity::default();
            assert_eq!(
                tracker.push(block, &mut |_| panic!("invalid input has no run")),
                Err(expected)
            );
            assert_eq!(tracker.cursor, 0);
            assert!(!tracker.facts.observed_tap);
            tracker.finish(Some(0), &mut |_| true);
            assert_eq!(tracker.facts.far_reference(), FarReference::Unknown);
        }
    }

    #[test]
    fn observed_pcm_is_borrowed_and_extreme_finite_levels_stay_finite() {
        let (mut tracker, mut runs) = (Activity::default(), Vec::new());
        let mic = [f32::MAX, -f32::MAX, -0.0, 0.0];
        let tap = [f32::MIN_POSITIVE, -0.0, 0.0, f32::MAX];
        let original = (mic.map(f32::to_bits), tap.map(f32::to_bits));
        feed(
            &mut tracker,
            epoch(4),
            0,
            &mic,
            &tap,
            &[false; 4],
            &mut runs,
        );
        assert_eq!((mic.map(f32::to_bits), tap.map(f32::to_bits)), original);
        let levels = tracker.last_levels.unwrap();
        assert!(levels.mic_mean_square.unwrap().is_finite());
        assert!(levels.tap_mean_square.unwrap().is_finite());
        assert_eq!(tracker.facts.far_reference(), FarReference::Present);
    }
}
