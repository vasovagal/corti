//! Checked projection of processing losses and forced-boundary locations into capture epochs.

use crate::{
    CaptureEpoch, QualityCause, QualityClass, QualityError, QualityRecord, QualityResolution,
    QualitySpan, QualityStage, QualityTrack,
};

/// Project a forced-boundary location envelope, not an assertion of lost audio or speech extent.
/// The live tracker supplies the final retained input frame before its nonempty cut. This helper
/// applies the same epoch splitting and conservative rounding as processing-loss projection, but
/// unavailable/capped locations count only as degraded omissions and never as processing loss.
pub fn insert_retained_forced_boundary(
    record: &mut QualityRecord,
    retained_start: u64,
    retained_end: u64,
    track: QualityTrack,
) -> Result<(), QualityError> {
    record.validate()?;
    insert_retained_fact(
        record,
        retained_start,
        retained_end,
        track,
        QualityCause::ForcedBoundary,
        QualityStage::Live,
        QualityClass::DegradedOnly,
    )
}

/// Append a processing loss in fixed-rate retained-WAV coordinates to the existing quality record.
/// Split at actual retained epoch boundaries; compressed-away source time is never included. Starts
/// round down and ends round up, clipped to a known source boundary after rounding. Unavailable or
/// overflowing mappings are counted as omitted processing losses rather than assigned an epoch.
/// The caller supplies a retained interval within the WAV; this record does not store its length.
pub fn insert_retained_processing_loss(
    record: &mut QualityRecord,
    retained_start: u64,
    retained_end: u64,
    track: QualityTrack,
    cause: QualityCause,
    stage: QualityStage,
) -> Result<(), QualityError> {
    record.validate()?;
    if !matches!(
        cause,
        QualityCause::TeeDrop
            | QualityCause::MicTapMismatch
            | QualityCause::DecodeFailure
            | QualityCause::NoResult
            | QualityCause::WindowNotFiled
    ) && !(cause == QualityCause::ResamplerFailure && stage != QualityStage::Capture)
    {
        return Err(QualityError::InvalidClassification);
    }
    insert_retained_fact(
        record,
        retained_start,
        retained_end,
        track,
        cause,
        stage,
        QualityClass::ProcessingLoss,
    )
}

fn insert_retained_fact(
    record: &mut QualityRecord,
    retained_start: u64,
    retained_end: u64,
    track: QualityTrack,
    cause: QualityCause,
    stage: QualityStage,
    class: QualityClass,
) -> Result<(), QualityError> {
    if retained_start >= retained_end {
        return omit(record, class);
    }
    let mut cursor = retained_start;
    // At most MAX_QUALITY_EPOCHS entries. Clone the bindings so insertion may coalesce spans.
    let epochs = record.epochs.clone();
    for (index, epoch) in epochs.iter().enumerate() {
        let next = epochs.get(index + 1);
        let end = next.map_or(retained_end, |next| {
            next.retained_start_frame.min(retained_end)
        });
        let start = cursor.max(epoch.retained_start_frame);
        if start >= end {
            continue;
        }
        if cursor < start {
            omit(record, class)?;
        }
        // An omitted boundary could have changed the mapping anywhere after this known epoch.
        let unknown_boundary = next.map_or(record.overflow.omitted_epochs != 0, |next| {
            epoch.id.checked_add(1) != Some(next.id)
        });
        let span = (!unknown_boundary)
            .then(|| source_interval(*epoch, next.copied(), start, end))
            .flatten()
            .map(|(start_frame, end_frame)| QualitySpan {
                epoch: epoch.id,
                start_frame,
                end_frame,
                track,
                cause,
                class,
                stage,
                resolution: QualityResolution::Outstanding,
            });
        if span.is_none_or(|span| record.insert_span(span).is_err()) {
            omit(record, class)?;
        }
        cursor = end;
        if cursor == retained_end {
            break;
        }
    }
    if cursor < retained_end {
        omit(record, class)?;
    }
    Ok(())
}

fn omit(record: &mut QualityRecord, class: QualityClass) -> Result<(), QualityError> {
    record.record_omitted_spans(class, QualityResolution::Outstanding, 1)
}

pub(crate) fn source_interval(
    epoch: CaptureEpoch,
    next: Option<CaptureEpoch>,
    retained_start: u64,
    retained_end: u64,
) -> Option<(u64, u64)> {
    let source = |retained: u64, ceil: bool| {
        let numerator = u128::from(retained.checked_sub(epoch.retained_start_frame)?)
            * u128::from(epoch.source_rate_hz);
        let divisor = u128::from(epoch.retained_rate_hz);
        let delta = if ceil {
            numerator.div_ceil(divisor)
        } else {
            numerator / divisor
        };
        epoch
            .source_frame_base
            .checked_add(u64::try_from(delta).ok()?)
    };
    let start = source(retained_start, false)?;
    let mut end = source(retained_end, true)?;
    if let Some((base, limit)) = epoch
        .source_start_ns
        .zip(next.and_then(|next| next.source_start_ns))
    {
        // Timestamps are floor-quantized nanoseconds. Find the last frame whose floored timestamp
        // does not exceed the next epoch's timestamp, without comparing unrelated frame bases.
        let delta_ns = u128::from(limit.checked_sub(base)?);
        let max_delta = ((delta_ns + 1) * u128::from(epoch.source_rate_hz) - 1) / 1_000_000_000;
        let boundary = epoch
            .source_frame_base
            .checked_add(u64::try_from(max_delta).ok()?)?;
        end = end.min(boundary);
    }
    (start < end).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CaptureEpochReason, MAX_QUALITY_SPANS};

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

    fn capture() -> QualityRecord {
        let mut record = QualityRecord::observed();
        record.insert_epoch(epoch()).unwrap();
        record
    }

    fn loss(record: &mut QualityRecord, start: u64, end: u64) {
        insert_retained_processing_loss(
            record,
            start,
            end,
            QualityTrack::Mic,
            QualityCause::DecodeFailure,
            QualityStage::Batch,
        )
        .unwrap();
    }

    #[test]
    fn failure_across_compressed_source_jump_splits_without_covering_the_jump() {
        let mut record = capture();
        record
            .insert_epoch(CaptureEpoch {
                id: 1,
                source_start_ns: Some(100_000_000_000),
                source_frame_base: 4_800_000,
                retained_start_frame: 48_000,
                reason: CaptureEpochReason::HostDiscontinuity,
                ..epoch()
            })
            .unwrap();
        loss(&mut record, 47_990, 48_010);
        assert_eq!(
            record
                .spans
                .iter()
                .map(|span| (span.epoch, span.start_frame, span.end_frame))
                .collect::<Vec<_>>(),
            [(0, 47_990, 48_000), (1, 4_800_000, 4_800_010)]
        );
        assert_eq!(record.overflow.outstanding_omitted_spans[1], 0);
    }

    #[test]
    fn floor_start_ceil_end_and_fractional_epoch_boundary_are_checked() {
        let mut record = QualityRecord::observed();
        let first = CaptureEpoch {
            source_rate_hz: 44_100,
            ..epoch()
        };
        record.insert_epoch(first).unwrap();
        record
            .insert_epoch(CaptureEpoch {
                id: 1,
                source_start_ns: Some(22_675),
                source_frame_base: 1,
                retained_start_frame: 1,
                reason: CaptureEpochReason::RateChange,
                ..first
            })
            .unwrap();
        loss(&mut record, 0, 3);
        assert_eq!(
            record
                .spans
                .iter()
                .map(|span| (span.start_frame, span.end_frame))
                .collect::<Vec<_>>(),
            [(0, 1), (1, 3)]
        );
        record.validate().unwrap();

        let mut rounded = QualityRecord::observed();
        rounded
            .insert_epoch(CaptureEpoch {
                source_rate_hz: 11_025,
                retained_rate_hz: 8_000,
                source_frame_base: 100,
                retained_start_frame: 8,
                ..epoch()
            })
            .unwrap();
        loss(&mut rounded, 11, 13);
        assert_eq!(
            (rounded.spans[0].start_frame, rounded.spans[0].end_frame),
            (104, 107)
        );
    }

    #[test]
    fn missing_and_omitted_epochs_and_arithmetic_overflow_remain_counted() {
        let mut missing = QualityRecord::default();
        loss(&mut missing, 4, 8);
        assert!(missing.spans.is_empty());
        assert_eq!(missing.overflow.outstanding_omitted_spans[1], 1);
        let mut omitted = capture();
        omitted.record_omitted_epochs(1);
        loss(&mut omitted, 4, 8);
        assert!(omitted.spans.is_empty());
        assert_eq!(omitted.overflow.outstanding_omitted_spans[1], 1);
        let mut overflow = QualityRecord::observed();
        overflow
            .insert_epoch(CaptureEpoch {
                source_frame_base: u64::MAX - 1,
                ..epoch()
            })
            .unwrap();
        loss(&mut overflow, 1, 4);
        assert_eq!(overflow.overflow.outstanding_omitted_spans[1], 1);
    }

    #[test]
    fn cap_preserves_the_failure_count_and_source_loss_is_unchanged() {
        let mut record = capture();
        record
            .insert_span(QualitySpan {
                epoch: 0,
                start_frame: 0,
                end_frame: 1,
                track: QualityTrack::Both,
                cause: QualityCause::RingOverflow,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        let source = record.spans[0];
        for index in 0..MAX_QUALITY_SPANS as u64 {
            loss(&mut record, index * 4 + 2, index * 4 + 3);
        }
        assert_eq!(record.spans.len(), MAX_QUALITY_SPANS);
        assert!(record.spans.contains(&source));
        assert_eq!(record.overflow.outstanding_omitted_spans[1], 1);
    }

    #[test]
    fn forced_boundary_projection_splits_epochs_without_labelling_compressed_gap_as_loss() {
        let mut record = capture();
        record
            .insert_epoch(CaptureEpoch {
                id: 1,
                source_start_ns: Some(100_000_000_000),
                source_frame_base: 4_800_000,
                retained_start_frame: 48_000,
                reason: CaptureEpochReason::HostDiscontinuity,
                ..epoch()
            })
            .unwrap();
        insert_retained_forced_boundary(&mut record, 47_990, 48_010, QualityTrack::Tap).unwrap();
        assert_eq!(
            record
                .spans
                .iter()
                .map(|s| (s.epoch, s.start_frame, s.end_frame))
                .collect::<Vec<_>>(),
            [(0, 47_990, 48_000), (1, 4_800_000, 4_800_010)]
        );
        assert!(
            record
                .spans
                .iter()
                .all(|s| s.cause == QualityCause::ForcedBoundary
                    && s.class == QualityClass::DegradedOnly
                    && s.stage == QualityStage::Live
                    && s.resolution == QualityResolution::Outstanding)
        );
        assert!(!record.requires_batch_recovery());
        assert_eq!(record.completeness(), crate::SourceCompleteness::Complete);
    }

    #[test]
    fn forced_boundary_unknown_mapping_and_span_cap_remain_degraded_only() {
        let mut unknown = QualityRecord::default();
        insert_retained_forced_boundary(&mut unknown, 2, 3, QualityTrack::Mic).unwrap();
        assert_eq!(unknown.overflow.outstanding_omitted_spans, [0, 0, 1]);
        assert!(!unknown.requires_batch_recovery());
        let mut capped = capture();
        for index in 0..MAX_QUALITY_SPANS as u64 {
            loss(&mut capped, 4 * index, 4 * index + 1);
        }
        let existing = capped.spans.clone();
        insert_retained_forced_boundary(&mut capped, 7, 8, QualityTrack::Tap).unwrap();
        assert_eq!(capped.spans, existing);
        assert_eq!(capped.overflow.outstanding_omitted_spans, [0, 0, 1]);
        assert!(
            capped.requires_batch_recovery(),
            "preexisting failures are preserved"
        );
    }

    #[test]
    fn processing_projector_still_rejects_forced_classification() {
        let mut record = capture();
        let original = record.clone();
        assert_eq!(
            insert_retained_processing_loss(
                &mut record,
                2,
                3,
                QualityTrack::Mic,
                QualityCause::ForcedBoundary,
                QualityStage::Live
            ),
            Err(QualityError::InvalidClassification)
        );
        assert_eq!(record, original);
    }

    #[test]
    fn forced_boundary_rounding_and_unavailable_epoch_use_the_existing_projection() {
        let mut rounded = QualityRecord::observed();
        rounded
            .insert_epoch(CaptureEpoch {
                source_rate_hz: 11_025,
                retained_rate_hz: 8_000,
                source_frame_base: 100,
                retained_start_frame: 8,
                ..epoch()
            })
            .unwrap();
        insert_retained_forced_boundary(&mut rounded, 11, 12, QualityTrack::Mic).unwrap();
        assert_eq!(
            (rounded.spans[0].start_frame, rounded.spans[0].end_frame),
            (104, 106)
        );
        let mut omitted = capture();
        omitted.record_omitted_epochs(1);
        insert_retained_forced_boundary(&mut omitted, 1, 2, QualityTrack::Mic).unwrap();
        assert!(omitted.spans.is_empty());
        assert_eq!(omitted.overflow.outstanding_omitted_spans, [0, 0, 1]);
        assert!(!omitted.requires_batch_recovery());
        let mut overflow = capture();
        overflow.epochs[0].source_frame_base = u64::MAX - 1;
        insert_retained_forced_boundary(&mut overflow, 1, 3, QualityTrack::Mic).unwrap();
        assert_eq!(overflow.overflow.outstanding_omitted_spans, [0, 0, 1]);
    }
}
