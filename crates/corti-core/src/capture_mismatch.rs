//! Bounded mic/tap alignment-loss accounting, independent of capture and decoder implementations.

const MAX_LIVE_MISMATCH_INTERVALS: usize = crate::MAX_QUALITY_SPANS;
const MISMATCH_WARN_EVERY: u64 = 100;

#[derive(Clone, Copy)]
struct MismatchInterval {
    epoch: crate::CaptureEpoch,
    start: u64,
    end: u64,
    track: crate::QualityTrack,
}

/// Transient retained-frame facts awaiting the immutable capture snapshot. Each chunk supplies its
/// writer position: neither tee drops nor unfilled source jumps are inferred from received lengths.
#[derive(Default)]
pub struct MicTapMismatches {
    count: u64,
    intervals: Vec<MismatchInterval>,
    omitted: u64,
    counter_saturated: bool,
    /// Good chunks still establish the Start epoch and intervening source/layout boundaries needed
    /// to encode a later mismatch. The bool rejects conflicting reuse of a retained epoch ID.
    epochs: Vec<(crate::CaptureEpoch, bool)>,
    omitted_epochs: u64,
    last_epoch_id: Option<u32>,
}

impl MicTapMismatches {
    /// Observe one received chunk. Returns whether this new mismatch should emit a rate-limited warning.
    pub fn observe(
        &mut self,
        epoch: crate::CaptureEpoch,
        retained_start_frame: u64,
        mic_len: usize,
        tap_len: usize,
        sample_rate: u32,
    ) -> bool {
        use crate::QualityTrack;
        self.remember_epoch(epoch);
        let mic_expected = epoch.mic_channels != 0;
        let tap_expected = epoch.tap_channels != 0;
        let frames = mic_len.max(tap_len);
        let mismatch = if mic_expected && tap_expected && mic_len != tap_len {
            Some((
                mic_len.min(tap_len),
                if mic_len < tap_len {
                    QualityTrack::Mic
                } else {
                    QualityTrack::Tap
                },
            ))
        } else if (!mic_expected && mic_len != 0) || (!tap_expected && tap_len != 0) {
            // Preserve unexpected samples too, but never certify a contradictory layout.
            Some((0, QualityTrack::Both))
        } else {
            None
        };
        let Some((missing_start, track)) = mismatch else {
            return false;
        };
        self.counter_saturated |= self.count == u64::MAX;
        self.count = self.count.saturating_add(1);
        let warning_due = self.should_warn();
        let interval = retained_start_frame
            .checked_add(missing_start as u64)
            .zip(retained_start_frame.checked_add(frames as u64))
            .filter(|(start, end)| {
                start < end
                    && *start >= epoch.retained_start_frame
                    && epoch.retained_rate_hz == sample_rate
                    && sample_rate != 0
                    && epoch.source_rate_hz != 0
            })
            .map(|(start, end)| MismatchInterval {
                epoch,
                start,
                end,
                track,
            });
        if let Some(interval) = interval {
            if let Some(previous) = self.intervals.last_mut()
                && previous.epoch == interval.epoch
                && previous.track == interval.track
                && previous.end == interval.start
            {
                previous.end = interval.end;
                return warning_due;
            }
            if self.intervals.len() < MAX_LIVE_MISMATCH_INTERVALS {
                self.intervals.push(interval);
                return warning_due;
            }
        }
        self.counter_saturated |= self.omitted == u64::MAX;
        self.omitted = self.omitted.saturating_add(1);
        warning_due
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    fn remember_epoch(&mut self, epoch: crate::CaptureEpoch) {
        if let Some((previous, consistent)) = self
            .epochs
            .iter_mut()
            .find(|(saved, _)| saved.id == epoch.id)
        {
            *consistent &= *previous == epoch;
        } else if self.epochs.len() < crate::MAX_QUALITY_EPOCHS {
            self.epochs.push((epoch, true));
        } else if self.last_epoch_id != Some(epoch.id) {
            self.counter_saturated |= self.omitted_epochs == u64::MAX;
            self.omitted_epochs = self.omitted_epochs.saturating_add(1);
        }
        self.last_epoch_id = Some(epoch.id);
    }

    pub fn should_warn(&self) -> bool {
        self.count == 1 || (self.count != 0 && self.count.is_multiple_of(MISMATCH_WARN_EVERY))
    }

    /// An error before the finish verdict has no complete capture snapshot. Keep the source bindings
    /// we did receive. This supplementary processing record is never capture/storage authority: the
    /// receiver must merge it into independently loaded capture quality before recovery. Missing
    /// bindings remain counted processing loss; a panic cannot erase an already-observed mismatch.
    pub fn provisional_quality(&self) -> crate::QualityRecord {
        let mut capture = crate::QualityRecord::observed();
        capture.record_omitted_epochs(self.omitted_epochs);
        for &(epoch, consistent) in &self.epochs {
            if consistent {
                let _ = capture.insert_epoch(epoch);
            }
        }
        self.settle(&capture).unwrap_or_else(|_| {
            let mut unknown = crate::QualityRecord::observed();
            unknown.overflow.omitted_spans[1] = self.count;
            unknown.overflow.outstanding_omitted_spans[1] = self.count;
            unknown.overflow.counter_saturated = self.counter_saturated;
            unknown
        })
    }

    pub fn settle(
        &self,
        capture: &crate::QualityRecord,
    ) -> Result<crate::QualityRecord, crate::QualityError> {
        use crate::{QualityClass, QualityResolution};
        let mut quality = capture.clone();
        quality.record_omitted_spans(
            QualityClass::ProcessingLoss,
            QualityResolution::Outstanding,
            self.omitted,
        )?;
        quality.overflow.counter_saturated |= self.counter_saturated;
        for interval in &self.intervals {
            let span = self.source_span(interval, capture);
            if span.is_none_or(|span| quality.insert_span(span).is_err()) {
                quality.record_omitted_spans(
                    QualityClass::ProcessingLoss,
                    QualityResolution::Outstanding,
                    1,
                )?;
            }
        }
        Ok(quality)
    }

    fn source_span(
        &self,
        interval: &MismatchInterval,
        capture: &crate::QualityRecord,
    ) -> Option<crate::QualitySpan> {
        let index = capture
            .epochs
            .iter()
            .position(|epoch| *epoch == interval.epoch)?;
        if capture
            .epochs
            .get(index + 1)
            .is_some_and(|next| interval.end > next.retained_start_frame)
        {
            return None;
        }
        let epoch = interval.epoch;
        let source = |retained: u64, ceil: bool| -> Option<u64> {
            let delta = retained.checked_sub(epoch.retained_start_frame)?;
            let numerator = u128::from(delta) * u128::from(epoch.source_rate_hz);
            let divisor = u128::from(epoch.retained_rate_hz);
            let frames = if ceil {
                numerator.div_ceil(divisor)
            } else {
                numerator / divisor
            };
            epoch
                .source_frame_base
                .checked_add(u64::try_from(frames).ok()?)
        };
        Some(crate::QualitySpan {
            epoch: epoch.id,
            start_frame: source(interval.start, false)?,
            end_frame: source(interval.end, true)?,
            track: interval.track,
            cause: crate::QualityCause::MicTapMismatch,
            class: crate::QualityClass::ProcessingLoss,
            stage: crate::QualityStage::Live,
            resolution: crate::QualityResolution::Outstanding,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureEpoch, CaptureEpochReason, QualityCause, QualityRecord, QualityTrack};

    fn epoch() -> CaptureEpoch {
        CaptureEpoch {
            id: 0,
            source_start_ns: Some(0),
            source_rate_hz: 8_000,
            source_frame_base: 0,
            retained_start_frame: 0,
            retained_rate_hz: 8_000,
            mic_channels: 1,
            tap_channels: 1,
            reason: CaptureEpochReason::Start,
        }
    }

    fn capture(epoch: CaptureEpoch) -> QualityRecord {
        let mut quality = QualityRecord::observed();
        quality.insert_epoch(epoch).unwrap();
        quality
    }

    #[test]
    fn both_short_sides_and_empty_expected_tracks_keep_exact_missing_tail() {
        for (mic, tap, track) in [
            (2, 4, QualityTrack::Mic),
            (4, 2, QualityTrack::Tap),
            (0, 4, QualityTrack::Mic),
            (4, 0, QualityTrack::Tap),
        ] {
            let mut mismatch = MicTapMismatches::default();
            assert!(mismatch.observe(epoch(), 100, mic, tap, 8_000));
            let original = capture(epoch());
            let quality = mismatch.settle(&original).unwrap();
            assert_eq!(mismatch.count(), 1);
            assert_eq!(quality.spans.len(), 1);
            let span = quality.spans[0];
            assert_eq!(
                (span.start_frame, span.end_frame, span.track),
                (100 + mic.min(tap) as u64, 104, track)
            );
            assert_eq!(span.cause, QualityCause::MicTapMismatch);
            assert!(quality.requires_batch_recovery());
            assert!(original.spans.is_empty());
        }
        for (mic, tap) in [(0, 4), (4, 0)] {
            let layout = CaptureEpoch {
                mic_channels: u32::from(mic != 0),
                tap_channels: u32::from(tap != 0),
                ..epoch()
            };
            let mut mismatch = MicTapMismatches::default();
            assert!(!mismatch.observe(layout, 0, mic, tap, 8_000));
            assert_eq!(mismatch.count(), 0);
            assert!(
                !mismatch
                    .settle(&capture(layout))
                    .unwrap()
                    .requires_batch_recovery()
            );
        }
    }

    #[test]
    fn retained_positions_survive_tee_drops_unfilled_jumps_and_rate_rounding() {
        let first = epoch();
        let later = CaptureEpoch {
            id: 1,
            source_start_ns: Some(12_000_000_000),
            source_frame_base: 96_000,
            retained_start_frame: 8_192,
            reason: CaptureEpochReason::HostDiscontinuity,
            ..first
        };
        let mut quality = capture(first);
        quality.insert_epoch(later).unwrap();
        let mut mismatch = MicTapMismatches::default();
        mismatch.observe(first, 0, 4_096, 4_096, 8_000);
        // The middle tee chunk was not received; a large source jump also consumed no retained frames.
        mismatch.observe(later, 8_192, 4_059, 4_096, 8_000);
        mismatch.observe(later, 12_288, 4_096, 4_096, 8_000);
        let settled = mismatch.settle(&quality).unwrap();
        assert_eq!(
            (
                settled.spans[0].epoch,
                settled.spans[0].start_frame,
                settled.spans[0].end_frame
            ),
            (1, 100_059, 100_096)
        );

        let fractional = CaptureEpoch {
            source_rate_hz: 11_025,
            source_frame_base: 100,
            retained_start_frame: 8,
            ..first
        };
        let mut mismatch = MicTapMismatches::default();
        mismatch.observe(fractional, 9, 2, 4, 8_000);
        let settled = mismatch.settle(&capture(fractional)).unwrap();
        assert_eq!(
            (settled.spans[0].start_frame, settled.spans[0].end_frame),
            (104, 107)
        );
    }

    #[test]
    fn absent_conflicting_cross_epoch_or_arithmetic_mapping_remains_counted_loss() {
        let base = CaptureEpoch {
            source_frame_base: 100,
            retained_start_frame: 8,
            ..epoch()
        };
        let mut mismatch = MicTapMismatches::default();
        mismatch.observe(base, 9, 2, 4, 8_000);
        let mut crossed = capture(base);
        crossed
            .insert_epoch(CaptureEpoch {
                id: 1,
                source_start_ns: Some(1_000_000_000),
                source_frame_base: 1_000,
                retained_start_frame: 12,
                reason: CaptureEpochReason::HostDiscontinuity,
                ..base
            })
            .unwrap();
        for unknown in [
            QualityRecord::observed(),
            capture(CaptureEpoch {
                source_frame_base: 101,
                ..base
            }),
            crossed,
        ] {
            let settled = mismatch.settle(&unknown).unwrap();
            assert!(settled.spans.is_empty());
            assert_eq!(settled.overflow.outstanding_omitted_spans[1], 1);
        }
        for (start, rate) in [(0, 8_000), (u64::MAX, 8_000), (9, 0), (9, 16_000)] {
            let mut invalid = MicTapMismatches::default();
            invalid.observe(base, start, 2, 4, rate);
            let settled = invalid.settle(&capture(base)).unwrap();
            assert!(settled.spans.is_empty());
            assert_eq!(settled.overflow.outstanding_omitted_spans[1], 1);
        }
        let overflow = CaptureEpoch {
            source_frame_base: u64::MAX - 1,
            ..epoch()
        };
        let mut mismatch = MicTapMismatches::default();
        mismatch.observe(overflow, 0, 2, 4, 8_000);
        assert_eq!(
            mismatch
                .settle(&capture(overflow))
                .unwrap()
                .overflow
                .outstanding_omitted_spans[1],
            1
        );
    }

    #[test]
    fn cap_coalescing_warning_cadence_and_saturation_keep_all_loss_accounted() {
        let mut mismatch = MicTapMismatches::default();
        let mut warnings = Vec::new();
        assert!(!mismatch.should_warn());
        for index in 0..300 {
            if mismatch.observe(epoch(), index * 4, 2, 4, 8_000) {
                warnings.push(mismatch.count());
            }
            assert!(
                !mismatch.observe(epoch(), index * 4, 4, 4, 8_000),
                "good chunks cannot repeat a warning"
            );
        }
        assert_eq!(warnings, [1, 100, 200, 300]);
        assert_eq!(mismatch.count(), 300);
        assert_eq!(mismatch.intervals.len(), MAX_LIVE_MISMATCH_INTERVALS);
        let settled = mismatch.settle(&capture(epoch())).unwrap();
        assert_eq!(settled.spans.len(), 256);
        assert_eq!(settled.overflow.outstanding_omitted_spans[1], 44);
        let mut adjacent = MicTapMismatches::default();
        for index in 0..300 {
            adjacent.observe(epoch(), index * 4, 0, 4, 8_000);
        }
        assert_eq!(adjacent.intervals.len(), 1);
        assert_eq!(adjacent.omitted, 0);
        assert_eq!(
            adjacent.settle(&capture(epoch())).unwrap().spans[0].end_frame,
            1_200
        );
        adjacent.count = u64::MAX;
        adjacent.observe(epoch(), 1_200, 0, 4, 8_000);
        assert!(
            adjacent
                .settle(&capture(epoch()))
                .unwrap()
                .overflow
                .counter_saturated
        );
    }

    #[test]
    fn good_start_epoch_keeps_a_later_mismatch_exact_before_finish() {
        let first = epoch();
        let later = CaptureEpoch {
            id: 1,
            source_start_ns: Some(500_000),
            source_frame_base: 4,
            retained_start_frame: 4,
            mic_channels: 2,
            reason: CaptureEpochReason::LayoutChange,
            ..first
        };
        let mut mismatch = MicTapMismatches::default();
        assert!(!mismatch.observe(first, 0, 4, 4, 8_000));
        assert!(mismatch.observe(later, 4, 2, 4, 8_000));
        let provisional = mismatch.provisional_quality();
        assert_eq!(provisional.epochs, [first, later]);
        assert_eq!(provisional.spans.len(), 1);
        assert_eq!(
            (
                provisional.spans[0].epoch,
                provisional.spans[0].start_frame,
                provisional.spans[0].end_frame
            ),
            (1, 6, 8)
        );
        assert_eq!(provisional.overflow, Default::default());
        let mut authority = capture(first);
        authority.insert_epoch(later).unwrap();
        authority.merge_snapshot(&provisional).unwrap();
        assert_eq!(authority.spans, provisional.spans);
        authority
            .resolve_processing_after_batch(
                crate::BatchRecoveryOutcome::SameBackendWholeFileSuccess,
            )
            .unwrap();
        assert_eq!(
            authority.spans[0].resolution,
            crate::QualityResolution::RecoveredByBatch
        );
        assert_eq!(
            authority.completeness(),
            crate::SourceCompleteness::Complete
        );
    }

    #[test]
    fn observed_epoch_inventory_is_bounded_and_conflicts_remain_counted_loss() {
        let mut mismatch = MicTapMismatches::default();
        for index in 0..=crate::MAX_QUALITY_EPOCHS {
            let binding = CaptureEpoch {
                id: index as u32,
                source_start_ns: Some(index as u64 * 500_000),
                source_frame_base: index as u64 * 4,
                retained_start_frame: index as u64 * 4,
                reason: if index == 0 {
                    CaptureEpochReason::Start
                } else {
                    CaptureEpochReason::LayoutChange
                },
                ..epoch()
            };
            mismatch.observe(
                binding,
                index as u64 * 4,
                if index == crate::MAX_QUALITY_EPOCHS {
                    2
                } else {
                    4
                },
                4,
                8_000,
            );
            mismatch.observe(binding, index as u64 * 4, 4, 4, 8_000);
        }
        assert_eq!(mismatch.epochs.len(), crate::MAX_QUALITY_EPOCHS);
        assert_eq!(
            mismatch.omitted_epochs, 1,
            "repeated chunks do not recount the same omitted epoch"
        );
        let quality = mismatch.provisional_quality();
        assert!(quality.spans.is_empty());
        assert_eq!(quality.overflow.omitted_epochs, 1);
        assert_eq!(quality.overflow.outstanding_omitted_spans[1], 1);
        let mut conflicting = MicTapMismatches::default();
        conflicting.observe(epoch(), 0, 4, 4, 8_000);
        conflicting.observe(
            CaptureEpoch {
                mic_channels: 2,
                ..epoch()
            },
            4,
            2,
            4,
            8_000,
        );
        let quality = conflicting.provisional_quality();
        assert!(quality.spans.is_empty());
        assert_eq!(quality.overflow.outstanding_omitted_spans[1], 1);
    }

    #[test]
    fn provisional_processing_facts_merge_only_with_independent_capture_authority() {
        let mut mismatch = MicTapMismatches::default();
        mismatch.observe(epoch(), 4, 2, 4, 8_000);
        let provisional = mismatch.provisional_quality();
        assert_eq!(provisional.spans.len(), 1);
        assert!(provisional.requires_batch_recovery());
        for observed in [false, true] {
            let mut authority = if observed {
                capture(epoch())
            } else {
                QualityRecord::default()
            };
            authority.merge_snapshot(&provisional).unwrap();
            authority
                .resolve_processing_after_batch(
                    crate::BatchRecoveryOutcome::SameBackendWholeFileSuccess,
                )
                .unwrap();
            assert_eq!(
                authority.completeness(),
                if observed {
                    crate::SourceCompleteness::Complete
                } else {
                    crate::SourceCompleteness::Unknown
                }
            );
        }
        let mut missing_epoch = MicTapMismatches::default();
        missing_epoch.observe(
            CaptureEpoch {
                id: 1,
                reason: CaptureEpochReason::LayoutChange,
                ..epoch()
            },
            4,
            2,
            4,
            8_000,
        );
        assert_eq!(
            missing_epoch
                .provisional_quality()
                .overflow
                .outstanding_omitted_spans[1],
            1
        );
    }
}
