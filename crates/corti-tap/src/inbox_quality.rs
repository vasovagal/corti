//! Quality-aware rendering for the force-tap CLI's existing inbox filing path.

use corti_core::{
    DiarizedTranscript, QualityClass, QualityRecord, RecordingMeta, SourceCompleteness,
};
use corti_vagus::provenance::TranscriptProvenance;

pub(super) fn prepare(
    meta: &RecordingMeta,
    transcript: &DiarizedTranscript,
    quality: &QualityRecord,
    provenance: &mut TranscriptProvenance,
) -> anyhow::Result<String> {
    quality.validate()?;
    // The same per-surface budget as the app: retain every epoch, replacing only display spans
    // with class/resolution-aware omission counts. The mandatory sidecar retains the full record.
    let mut projection = quality.clone();
    while serde_json::to_vec(&projection)?.len() > 48 * 1024 {
        let span = projection
            .spans
            .pop()
            .ok_or_else(|| anyhow::anyhow!("quality_epoch_projection_exceeds_prefix_budget"))?;
        projection.record_omitted_spans(span.class, span.resolution, 1)?;
    }
    provenance
        .configuration
        .insert("quality".into(), serde_json::to_value(&projection)?);
    provenance.configuration.insert(
        "quality_projection_truncated".into(),
        (projection != *quality).into(),
    );
    provenance
        .configuration
        .insert("quality_full_span_count".into(), quality.spans.len().into());
    provenance.configuration.insert(
        "source_completeness".into(),
        serde_json::to_value(quality.source_completeness())?,
    );
    provenance.configuration.insert(
        "text_completeness".into(),
        serde_json::to_value(quality.completeness())?,
    );

    let mut display = transcript.clone();
    let mut uncertain_turns = 0usize;
    for turn in &mut display.segments {
        match (
            source_endpoint(quality, turn.start, false),
            source_endpoint(quality, turn.end, true),
        ) {
            (Some((start, first)), Some((end, last))) if end >= start => {
                turn.start = start;
                turn.end = end;
                uncertain_turns += usize::from(first != last);
            }
            _ if !quality.epochs.is_empty() => uncertain_turns += 1,
            _ => {}
        }
    }
    let mut body = corti_vagus::recording_body(meta, &display);
    if quality.completeness() != SourceCompleteness::Complete {
        body.push_str("\n\n> Transcript coverage is partial or unknown. Retained audio can be transcribed again with `corti --input <audio> --local`.\n");
    }
    for span in &quality.spans {
        if span.class == QualityClass::DegradedOnly {
            continue;
        }
        let times = quality
            .epochs
            .iter()
            .find(|epoch| epoch.id == span.epoch)
            .and_then(|epoch| {
                Some((
                    epoch.source_time_ns(span.start_frame).ok()??,
                    epoch.source_time_ns(span.end_frame).ok()??,
                ))
            });
        let location = match times {
            Some((start, end)) => format!("{:.3}–{:.3} s", start as f64 / 1e9, end as f64 / 1e9),
            None => format!(
                "epoch {} frames {}–{} (source offset unknown)",
                span.epoch, span.start_frame, span.end_frame
            ),
        };
        body.push_str(&format!(
            "\n> Audio/text availability: {} at {location}; {}, {}, {}.\n",
            label(serde_json::to_value(span.cause)?)?,
            label(serde_json::to_value(span.track)?)?,
            label(serde_json::to_value(span.class)?)?,
            label(serde_json::to_value(span.resolution)?)?
        ));
    }
    if quality.overflow.omitted_spans != [0; 3]
        || quality.overflow.omitted_epochs != 0
        || quality.overflow.counter_saturated
    {
        body.push_str(&format!("\n> Quality accounting overflow: source/processing/degraded spans {:?}; outstanding {:?}; omitted epochs {}; counter saturated {}.\n", quality.overflow.omitted_spans, quality.overflow.outstanding_omitted_spans, quality.overflow.omitted_epochs, quality.overflow.counter_saturated));
    }
    if uncertain_turns != 0 {
        body.push_str(&format!("\n> {uncertain_turns} transcript turn(s) cross an epoch boundary or have an unknown source offset; within-turn source timing is uncertain.\n"));
    }
    Ok(body)
}

fn label(value: serde_json::Value) -> anyhow::Result<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("invalid_quality_label"))
}

fn source_endpoint(quality: &QualityRecord, seconds: f64, end: bool) -> Option<(f64, u32)> {
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    let (index, epoch) = quality.epochs.iter().enumerate().rev().find(|(_, epoch)| {
        let boundary = epoch.retained_start_frame as f64 / f64::from(epoch.retained_rate_hz);
        seconds > boundary || (!end && seconds == boundary) || (seconds == 0.0 && boundary == 0.0)
    })?;
    if index + 1 == quality.epochs.len() && quality.overflow.omitted_epochs != 0 {
        return None;
    }
    let source = epoch.source_start_ns? as f64 / 1e9;
    let retained = epoch.retained_start_frame as f64 / f64::from(epoch.retained_rate_hz);
    Some((source + seconds - retained, epoch.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{
        CaptureEpoch, CaptureEpochReason, OwningApp, QualityCause, QualityClass, QualityResolution,
        QualitySpan, QualityStage, QualityTrack, Speaker, TranscriptSegment,
    };
    use corti_vagus::provenance::GenerationMode;

    fn fixture() -> (
        RecordingMeta,
        DiarizedTranscript,
        QualityRecord,
        TranscriptProvenance,
    ) {
        let meta = RecordingMeta {
            started_at: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Local),
            ended_at: None,
            owning_app: OwningApp {
                bundle_id: None,
                name: "Synthetic probe".into(),
            },
            audio_path: "/tmp/synthetic-probe.wav".into(),
        };
        let transcript = DiarizedTranscript::new(vec![
            TranscriptSegment {
                speaker: Speaker::Me,
                start: 0.25,
                end: 0.75,
                text: "First synthetic phrase.".into(),
            },
            TranscriptSegment {
                speaker: Speaker::Me,
                start: 2.25,
                end: 2.75,
                text: "Third synthetic phrase.".into(),
            },
        ]);
        let mut quality = QualityRecord::observed();
        quality
            .insert_epoch(CaptureEpoch {
                id: 0,
                source_start_ns: Some(0),
                source_rate_hz: 16_000,
                source_frame_base: 0,
                retained_start_frame: 0,
                retained_rate_hz: 16_000,
                mic_channels: 1,
                tap_channels: 1,
                reason: CaptureEpochReason::Start,
            })
            .unwrap();
        (
            meta,
            transcript,
            quality,
            TranscriptProvenance::legacy_unknown(GenerationMode::Batch),
        )
    }

    #[test]
    fn inbox_partial_decode_keeps_other_words_and_exact_loss_metadata() {
        let (meta, transcript, mut quality, mut provenance) = fixture();
        quality
            .insert_span(QualitySpan {
                epoch: 0,
                start_frame: 16_000,
                end_frame: 32_000,
                track: QualityTrack::Mic,
                cause: QualityCause::DecodeFailure,
                class: QualityClass::ProcessingLoss,
                stage: QualityStage::Batch,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        let body = prepare(&meta, &transcript, &quality, &mut provenance).unwrap();
        assert!(body.contains("First synthetic phrase."));
        assert!(body.contains("Third synthetic phrase."));
        assert!(body.contains("decode_failure at 1.000–2.000 s"), "{body}");
        assert_eq!(provenance.configuration["text_completeness"], "incomplete");
        assert_eq!(
            provenance.configuration["quality"]["spans"][0]["start_frame"],
            16_000
        );
        assert_eq!(
            provenance.configuration["quality"]["spans"][0]["end_frame"],
            32_000
        );
    }

    #[test]
    fn inbox_missing_interval_mapping_remains_visible_as_incomplete() {
        let (meta, transcript, mut quality, mut provenance) = fixture();
        quality
            .record_omitted_spans(
                QualityClass::ProcessingLoss,
                QualityResolution::Outstanding,
                1,
            )
            .unwrap();
        let body = prepare(&meta, &transcript, &quality, &mut provenance).unwrap();
        assert!(body.contains("Quality accounting overflow"), "{body}");
        assert_eq!(provenance.configuration["text_completeness"], "incomplete");
    }

    #[test]
    fn inbox_complete_quality_keeps_existing_body_text() {
        let (meta, transcript, quality, mut provenance) = fixture();
        let body = prepare(&meta, &transcript, &quality, &mut provenance).unwrap();
        assert_eq!(body, corti_vagus::recording_body(&meta, &transcript));
    }

    #[test]
    fn inbox_epoch_jump_keeps_source_time_and_unknown_boundaries_explicit() {
        let (meta, transcript, mut quality, mut provenance) = fixture();
        quality
            .insert_epoch(CaptureEpoch {
                id: 1,
                source_start_ns: Some(20_000_000_000),
                source_frame_base: 320_000,
                retained_start_frame: 16_000,
                reason: CaptureEpochReason::HostDiscontinuity,
                ..quality.epochs[0]
            })
            .unwrap();
        quality
            .insert_span(QualitySpan {
                epoch: 0,
                start_frame: 16_000,
                end_frame: 320_000,
                track: QualityTrack::Both,
                cause: QualityCause::HostDiscontinuity,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            })
            .unwrap();
        let body = prepare(&meta, &transcript, &quality, &mut provenance).unwrap();
        assert!(body.contains("[00:21] Me:"), "{body}");
        assert!(
            body.contains("host_discontinuity at 1.000–20.000 s"),
            "{body}"
        );
        assert_eq!(source_endpoint(&quality, 1.0, false), Some((20.0, 1)));
        assert_eq!(source_endpoint(&quality, 1.0, true), Some((1.0, 0)));
        assert_eq!(
            provenance.configuration["source_completeness"],
            "incomplete"
        );
        quality.record_omitted_epochs(1);
        assert_eq!(source_endpoint(&quality, 2.25, false), None);
        assert!(
            prepare(&meta, &transcript, &quality, &mut provenance)
                .unwrap()
                .contains("within-turn source timing is uncertain")
        );
    }

    #[test]
    fn inbox_projection_bounds_metadata_but_body_keeps_every_retained_loss() {
        let (meta, transcript, _, mut provenance) = fixture();
        let mut quality = QualityRecord::observed();
        for epoch in 0..corti_core::MAX_QUALITY_EPOCHS as u32 {
            quality
                .insert_epoch(CaptureEpoch {
                    id: epoch,
                    source_start_ns: Some(
                        u64::MAX - 64_000_000_000 + u64::from(epoch) * 1_000_000_000,
                    ),
                    source_rate_hz: u32::MAX,
                    source_frame_base: u64::MAX - 1_000,
                    retained_start_frame: u64::MAX - 1_000 + u64::from(epoch) * 10,
                    retained_rate_hz: u32::MAX,
                    mic_channels: 1,
                    tap_channels: 1,
                    reason: if epoch == 0 {
                        CaptureEpochReason::Start
                    } else {
                        CaptureEpochReason::HostDiscontinuity
                    },
                })
                .unwrap();
        }
        for epoch in 0..corti_core::MAX_QUALITY_EPOCHS as u32 {
            for index in 0..4 {
                quality
                    .insert_span(QualitySpan {
                        epoch,
                        start_frame: u64::MAX - 1_000 + index * 2,
                        end_frame: u64::MAX - 999 + index * 2,
                        track: QualityTrack::Mic,
                        cause: QualityCause::DecodeFailure,
                        class: QualityClass::ProcessingLoss,
                        stage: QualityStage::Batch,
                        resolution: QualityResolution::Outstanding,
                    })
                    .unwrap();
            }
        }
        assert!(serde_json::to_vec(&quality).unwrap().len() > 48 * 1024);
        let body = prepare(&meta, &transcript, &quality, &mut provenance).unwrap();
        let projected: QualityRecord =
            serde_json::from_value(provenance.configuration["quality"].clone()).unwrap();
        assert!(serde_json::to_vec(&projected).unwrap().len() <= 48 * 1024);
        assert_eq!(projected.epochs, quality.epochs);
        assert_eq!(
            projected.spans.len() as u64 + projected.overflow.omitted_spans[1],
            256
        );
        assert_eq!(body.matches("decode_failure at").count(), 256);
        assert_eq!(
            provenance.configuration["quality_projection_truncated"],
            true
        );
        assert_eq!(provenance.configuration["quality_full_span_count"], 256);
    }
}
