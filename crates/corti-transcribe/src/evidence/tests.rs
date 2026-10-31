use super::*;
use corti_core::{
    CaptureEpoch, CaptureEpochReason, QualityCause, QualityClass, QualityResolution, QualitySpan,
    QualityStage, Speaker,
};

#[path = "budget_tests.rs"]
mod budget_tests;
#[path = "lineage_tests.rs"]
mod lineage_tests;
#[path = "production.rs"]
mod production;
#[path = "validation_tests.rs"]
mod validation_tests;

#[test]
fn borrowed_row_index_and_budget_fallback_preserve_unordered_ids_and_missing_rows() {
    let mut group = base(1, "first");
    group.rows.push(row(1, 1, "second"));
    group.stages[0].output.push(id(1, 1));
    group.rows.reverse();
    let groups = [group];
    for budget in [0, 1, ROW_LOOKUP_BYTES] {
        let lookup = RowLookup::with_budget(&groups, budget);
        assert_eq!(lookup.rows.is_some(), budget == ROW_LOOKUP_BYTES);
        for slot in 0..3 {
            match (lookup.get(id(1, slot)), groups[0].row(id(1, slot))) {
                (Some(indexed), Some(scanned)) => assert!(std::ptr::eq(indexed, scanned)),
                (None, None) => {}
                _ => panic!("row index changed lookup availability"),
            }
        }
    }
    let failed = RowLookup::with_reservation(&groups, ROW_LOOKUP_BYTES, |_, _| {
        Vec::<u8>::new().try_reserve_exact(usize::MAX)
    });
    assert!(failed.rows.is_none());
    assert!(std::ptr::eq(
        failed.get(id(1, 1)).unwrap(),
        groups[0].row(id(1, 1)).unwrap()
    ));
    assert!(failed.get(id(900, 0)).is_none());
}

fn id(invocation: u64, slot: u32) -> RowId {
    RowId {
        invocation: InvocationId(invocation),
        slot,
    }
}

fn row(invocation: u64, slot: u32, text: &str) -> Row {
    Row {
        id: id(invocation, slot),
        track: Some(QualityTrack::Mic),
        source_region_index: None,
        segment: TranscriptSegment {
            speaker: Speaker::Me,
            start: 1.234,
            end: 2.987,
            text: text.into(),
        },
    }
}

fn base(first: u64, text: &str) -> Group {
    Group {
        window: 7,
        identity: Identity {
            configuration_hash: Some("a".repeat(64)),
            model_hash: Some("b".repeat(64)),
            runtime_hash: Some("c".repeat(64)),
        },
        signal: SignalIdentity {
            source: SignalSource::LiveStream,
            aec: AecOrigin::LiveShortLookahead,
            source_rate_hz: Some(48_000),
            lookahead_frames: Some(8192),
        },
        rows: vec![row(first, 0, text)],
        stages: vec![Stage {
            id: InvocationId(first),
            kind: StageKind::TrueAsr,
            input: None,
            assembled_input: vec![],
            context: vec![],
            derivations: vec![],
            protected_output: vec![],
            output: vec![id(first, 0)],
            decisions: vec![],
        }],
    }
}

fn transform(
    first: u64,
    before: &str,
    after: &str,
    pass: CleanupStage,
    mut edits: Vec<TextEdit>,
) -> Group {
    // Generic capacity/link fixtures author a whole-string synthetic rule. Accuracy/position fixtures
    // below supply their own precise edits from the explicitly named synthetic transformations.
    if edits.is_empty() && pass == CleanupStage::Lexicon {
        edits.push(TextEdit {
            before: ByteSpan {
                start: 0,
                end: before.len() as u32,
            },
            after: ByteSpan {
                start: 0,
                end: after.len() as u32,
            },
            action: EditKind::Lexicon {
                rule_id: "synthetic-capacity-rule".into(),
            },
        });
    }
    let mut group = base(first, before);
    group.rows.push(row(first + 1, 0, after));
    group.stages.push(Stage {
        id: InvocationId(first + 1),
        kind: StageKind::Cleanup { pass },
        input: Some(InvocationId(first)),
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output: vec![id(first + 1, 0)],
        decisions: vec![Decision {
            reason: if pass == CleanupStage::Lexicon {
                CleanupReason::TextRule
            } else {
                CleanupReason::FillersOrStutters
            },
            input_indices: vec![0],
            originals: vec![id(first, 0)],
            replacements: vec![id(first + 1, 0)],
            edits,
            evidence: None,
            source: None,
        }],
    });
    group
}

fn bytes(ledger: &Ledger) -> Vec<u8> {
    let mut out = Vec::new();
    ledger.write_json(&mut out).unwrap();
    assert_eq!(out.len(), ledger.serialized_len().unwrap());
    out
}

fn full_quality() -> QualityRecord {
    let mut quality = QualityRecord::observed();
    for epoch in 0..corti_core::MAX_QUALITY_EPOCHS as u32 {
        quality
            .insert_epoch(CaptureEpoch {
                id: epoch,
                source_start_ns: None,
                source_rate_hz: 48_000,
                source_frame_base: 0,
                retained_start_frame: u64::from(epoch) * 10_000,
                retained_rate_hz: 48_000,
                mic_channels: 1,
                tap_channels: 1,
                reason: if epoch == 0 {
                    CaptureEpochReason::Start
                } else {
                    CaptureEpochReason::RateChange
                },
            })
            .unwrap();
        for offset in [0, 2, 4, 6] {
            quality
                .insert_span(QualitySpan {
                    epoch,
                    start_frame: offset,
                    end_frame: offset + 1,
                    track: QualityTrack::Both,
                    cause: QualityCause::RingOverflow,
                    class: QualityClass::SourceLoss,
                    stage: QualityStage::Capture,
                    resolution: QualityResolution::Outstanding,
                })
                .unwrap();
        }
    }
    quality.overflow.omitted_spans = [u64::MAX; 3];
    quality.overflow.outstanding_omitted_spans = [u64::MAX; 3];
    quality.overflow.omitted_epochs = u64::MAX;
    quality.overflow.counter_saturated = true;
    quality.validate().unwrap();
    quality
}

#[test]
fn exact_subseconds_speaker_and_track_round_trip_without_markdown() {
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let mut group = base(1, "Synthetic timing sentence.");
    group.rows[0].segment.speaker = Speaker::Other("Synthetic speaker".into());
    group.rows[0].track = Some(QualityTrack::Tap);
    assert_eq!(ledger.append(group), Ok(Append::Retained));
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    let rows = restored.reconstruct_input(InvocationId(1)).unwrap();
    assert_eq!(rows[0].segment.start.to_bits(), 1.234_f64.to_bits());
    assert_eq!(rows[0].segment.end.to_bits(), 2.987_f64.to_bits());
    assert_eq!(rows[0].track, Some(QualityTrack::Tap));
    assert_eq!(rows[0].segment.speaker.display(), "Synthetic speaker");
}

#[test]
fn source_frame_derived_timing_corpus_round_trips_every_f64_bit() {
    for rate in [16_000_u32, 44_100, 48_000] {
        let mut group = base(1, "Synthetic sample-timing corpus.");
        group.rows.clear();
        group.stages[0].output.clear();
        let mut expected = Vec::new();
        for slot in 0..512 {
            let frame = (u64::from(slot) * 1_103_515_245 + 12_345) % (u64::from(rate) * 28_800);
            let mut item = row(1, slot, "Synthetic sample.");
            item.segment.start = frame as f64 / f64::from(rate);
            item.segment.end = (frame + 57_813) as f64 / f64::from(rate);
            expected.push((item.segment.start.to_bits(), item.segment.end.to_bits()));
            group.stages[0].output.push(item.id);
            group.rows.push(item);
        }
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        assert_eq!(ledger.append(group), Ok(Append::Retained));
        let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
        for (row, expected) in restored.groups()[0].rows.iter().zip(expected) {
            assert_eq!(
                (row.segment.start.to_bits(), row.segment.end.to_bits()),
                expected
            );
        }
    }
}

#[test]
fn finite_nonnegative_timing_bits_survive_json_without_a_custom_time_representation() {
    let mut group = base(1, "Synthetic numeric timing stress.");
    group.rows.clear();
    group.stages[0].output.clear();
    let mut bits = vec![
        0,
        (-0.0_f64).to_bits(),
        1,
        f64::MIN_POSITIVE.to_bits(),
        f64::MAX.to_bits(),
        f64::EPSILON.to_bits(),
        1e-6_f64.to_bits(),
        1e20_f64.to_bits(),
    ];
    let mut seed = 164_u64;
    for _ in 0..512 {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        let candidate = seed & 0x7fff_ffff_ffff_ffff;
        if f64::from_bits(candidate).is_finite() {
            bits.push(candidate);
        }
    }
    for (slot, bit_pattern) in bits.iter().enumerate() {
        let mut item = row(1, slot as u32, "Synthetic numeric value.");
        item.segment.start = f64::from_bits(*bit_pattern);
        item.segment.end = item.segment.start;
        group.stages[0].output.push(item.id);
        group.rows.push(item);
    }
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(ledger.append(group), Ok(Append::Retained));
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    for (row, expected) in restored.groups()[0].rows.iter().zip(bits) {
        assert_eq!(
            (row.segment.start.to_bits(), row.segment.end.to_bits()),
            (expected, expected)
        );
    }
}

#[test]
fn very_very_stutter_keeps_the_deleted_position_and_preimage() {
    let input = "Very very important.";
    let (output, fillers, stutters) = crate::segment::strip_fillers_text(input);
    assert_eq!(
        (output.as_str(), fillers, stutters),
        ("Very important.", 0, 1)
    );
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let edits = vec![TextEdit {
        before: ByteSpan { start: 5, end: 10 },
        after: ByteSpan { start: 5, end: 5 },
        action: EditKind::Stutter,
    }];
    assert_eq!(
        ledger.append(transform(1, input, &output, CleanupStage::Fillers, edits)),
        Ok(Append::Retained)
    );
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    let d = &restored.groups()[0].stages[1].decisions[0];
    assert_eq!(d.edits[0].before, ByteSpan { start: 5, end: 10 });
    assert_eq!(
        restored.reconstruct_input(InvocationId(2)).unwrap()[0]
            .segment
            .text,
        input
    );
}

#[test]
fn filler_position_and_separate_case_repair_cover_the_actual_output() {
    let input = "Um, the gateway is down.";
    let (output, fillers, stutters) = crate::segment::strip_fillers_text(input);
    assert_eq!(
        (output.as_str(), fillers, stutters),
        ("The gateway is down.", 1, 0)
    );
    let edits = vec![
        TextEdit {
            before: ByteSpan { start: 0, end: 4 },
            after: ByteSpan { start: 0, end: 0 },
            action: EditKind::Filler,
        },
        TextEdit {
            before: ByteSpan { start: 4, end: 5 },
            after: ByteSpan { start: 0, end: 1 },
            action: EditKind::Case,
        },
    ];
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(
        ledger.append(transform(1, input, &output, CleanupStage::Fillers, edits)),
        Ok(Append::Retained)
    );
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        restored.reconstruct_input(InvocationId(2)).unwrap()[0]
            .segment
            .text,
        input
    );
    let d = &restored.groups()[0].stages[1].decisions[0];
    assert!(matches!(d.edits[0].action, EditKind::Filler));
    assert_eq!(d.edits[0].before, ByteSpan { start: 0, end: 4 });
}

#[test]
fn fully_deleted_filler_row_remains_reconstructable_without_an_empty_output_clone() {
    let (output, fillers, stutters) = crate::segment::strip_fillers_text("Hmm.");
    assert_eq!((output.as_str(), fillers, stutters), ("", 1, 0));
    let mut group = transform(
        1,
        "Hmm.",
        "",
        CleanupStage::Fillers,
        vec![TextEdit {
            before: ByteSpan { start: 0, end: 4 },
            after: ByteSpan { start: 0, end: 0 },
            action: EditKind::Filler,
        }],
    );
    group.rows.pop();
    group.stages[1].output.clear();
    group.stages[1].decisions[0].replacements.clear();
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(ledger.append(group), Ok(Append::Retained));
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(restored.groups()[0].rows.len(), 1);
    assert_eq!(
        restored.reconstruct_input(InvocationId(2)).unwrap()[0]
            .segment
            .text,
        "Hmm."
    );
}

#[test]
fn early_echo_region_ordinal_is_not_a_stage_local_index_or_a_u32() {
    let ordinal = u64::from(u32::MAX) + 17;
    let mut group = base(1, "Synthetic echo candidate.");
    group.rows[0].source_region_index = Some(ordinal);
    group.stages.push(Stage {
        id: InvocationId(2),
        kind: StageKind::Cleanup {
            pass: CleanupStage::EarlyEcho,
        },
        input: Some(InvocationId(1)),
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output: vec![id(1, 0)],
        decisions: vec![Decision {
            reason: CleanupReason::Echo(EchoVerdict::SuspectedDuplicate),
            input_indices: vec![ordinal],
            originals: vec![id(1, 0)],
            replacements: vec![id(1, 0)],
            edits: vec![],
            evidence: Some(EchoEvidence {
                mic_db_bits: f32::NEG_INFINITY.to_bits(),
                echo_estimate_db_bits: (-45.5_f32).to_bits(),
                double_talk_fraction_bits: 0.0_f32.to_bits(),
                blocks: 3,
            }),
            source: Some(EchoSourceDescription {
                me: false,
                start: 1.234,
                end: 2.987,
            }),
        }],
    });
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(ledger.append(group), Ok(Append::Retained));
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    let d = &restored.groups()[0].stages[1].decisions[0];
    assert_eq!(d.input_indices, [ordinal]);
    assert_eq!(
        d.evidence.as_ref().unwrap().mic_db_bits,
        f32::NEG_INFINITY.to_bits()
    );
    assert_eq!(
        restored.reconstruct_input(InvocationId(2)).unwrap()[0].source_region_index,
        Some(ordinal)
    );
    let mut malformed: Document = serde_json::from_slice(&bytes(&ledger)).unwrap();
    malformed.groups[0].stages[1].decisions[0].input_indices[0] = 0;
    assert!(matches!(
        Ledger::read_json(&serde_json::to_vec(&malformed).unwrap(), DEFAULT_CAP),
        Err(Error::InvalidLink)
    ));
}

#[test]
fn changed_cleanup_cannot_silently_omit_its_decision_or_position_trace() {
    let mut missing_decision = transform(1, "rust", "REST", CleanupStage::Lexicon, vec![]);
    missing_decision.stages[1].decisions.clear();
    assert_eq!(
        Ledger::new(DEFAULT_CAP).unwrap().append(missing_decision),
        Err(Error::InvalidLink)
    );
    let mut missing_edit = transform(1, "rust", "REST", CleanupStage::Lexicon, vec![]);
    missing_edit.stages[1].decisions[0].edits.clear();
    assert_eq!(
        Ledger::new(DEFAULT_CAP).unwrap().append(missing_edit),
        Err(Error::InvalidEdit)
    );
}

#[test]
fn identical_text_and_time_in_different_windows_never_join_invocations() {
    let mut first = transform(
        1,
        "Very very important.",
        "Very important.",
        CleanupStage::Fillers,
        vec![TextEdit {
            before: ByteSpan { start: 5, end: 10 },
            after: ByteSpan { start: 5, end: 5 },
            action: EditKind::Stutter,
        }],
    );
    let mut second = base(3, "Very very important.");
    first.window = 1;
    second.window = 2;
    second.stages[0].kind = StageKind::FallbackAsr;
    second.stages.push(Stage {
        id: InvocationId(4),
        kind: StageKind::ProviderInput,
        input: Some(InvocationId(3)),
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output: vec![id(3, 0)],
        decisions: vec![],
    });
    second.stages.push(Stage {
        id: InvocationId(5),
        kind: StageKind::FinalProse,
        input: Some(InvocationId(4)),
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output: vec![id(3, 0)],
        decisions: vec![],
    });
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(ledger.append(first), Ok(Append::Retained));
    assert_eq!(ledger.append(second), Ok(Append::Retained));
    assert_eq!(
        ledger.reconstruct_input(InvocationId(2)).unwrap()[0].id,
        id(1, 0)
    );
    assert_eq!(
        ledger.reconstruct_input(InvocationId(4)).unwrap()[0].id,
        id(3, 0)
    );
    assert_eq!(
        ledger.reconstruct_input(InvocationId(5)).unwrap()[0].id,
        id(3, 0)
    );
    assert_eq!(
        ledger.groups()[1].rows.len(),
        1,
        "unchanged stages borrow one owned row"
    );
    assert_eq!(ledger.groups()[1].stages[0].kind, StageKind::FallbackAsr);
}

#[test]
fn later_provider_and_prose_invocations_link_retained_rows_without_copying_asr() {
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(
        ledger.append(base(1, "Synthetic retained ASR.")),
        Ok(Append::Retained)
    );
    let asr_before = serde_json::to_vec(&ledger.groups()[0]).unwrap();
    let mut provider = base(2, "unused");
    provider.rows.clear();
    provider.signal = SignalIdentity {
        source: SignalSource::ProviderText,
        aec: AecOrigin::None,
        source_rate_hz: None,
        lookahead_frames: None,
    };
    provider.stages[0] = Stage {
        id: InvocationId(2),
        kind: StageKind::ProviderInput,
        input: Some(InvocationId(1)),
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output: vec![id(1, 0)],
        decisions: vec![],
    };
    provider.rows.push(row(3, 0, "Synthetic final prose."));
    provider.stages.push(Stage {
        id: InvocationId(3),
        kind: StageKind::FinalProse,
        input: Some(InvocationId(2)),
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output: vec![id(3, 0)],
        decisions: vec![],
    });
    assert_eq!(ledger.append(provider), Ok(Append::Retained));
    assert_eq!(
        ledger.reconstruct_input(InvocationId(3)).unwrap()[0].id,
        id(1, 0)
    );
    assert_eq!(
        ledger.groups().iter().map(|g| g.rows.len()).sum::<usize>(),
        2
    );
    ledger.set_final_quality(full_quality()).unwrap();
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        serde_json::to_vec(&restored.groups()[0]).unwrap(),
        asr_before
    );
    assert_eq!(
        restored.reconstruct_input(InvocationId(2)).unwrap()[0]
            .segment
            .text,
        "Synthetic retained ASR."
    );
    assert_eq!(
        restored.reconstruct_input(InvocationId(3)).unwrap()[0]
            .segment
            .text,
        "Synthetic retained ASR."
    );
}

#[test]
fn local_input_indices_select_the_right_identical_row_and_bad_links_are_rejected() {
    let mut group = transform(1, "Same.", "Changed.", CleanupStage::Lexicon, vec![]);
    group.rows.insert(1, row(1, 1, "Same."));
    group.stages[0].output.push(id(1, 1));
    group.stages[1].output.insert(0, id(1, 0));
    group.stages[1].decisions[0].input_indices[0] = 1;
    group.stages[1].decisions[0].originals[0] = id(1, 1);
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(ledger.append(group), Ok(Append::Retained));
    let rows = ledger.reconstruct_input(InvocationId(2)).unwrap();
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [id(1, 0), id(1, 1)]
    );
    let mut bad = transform(3, "Same.", "Changed.", CleanupStage::Lexicon, vec![]);
    bad.stages[1].decisions[0].originals[0] = id(1, 0);
    assert_eq!(ledger.append(bad), Err(Error::InvalidLink));
    assert_eq!(ledger.next_invocation(), Ok(InvocationId(3)));
}

#[test]
fn exact_escaped_byte_boundary_keeps_or_omits_the_whole_linked_group() {
    let input = "\"\\\n\u{1}🦀".repeat(200);
    let group = transform(
        1,
        &input,
        "Synthetic replacement",
        CleanupStage::Lexicon,
        vec![],
    );
    let size = wire_size(&group, usize::MAX).unwrap();
    assert!(size > 2 * input.len());
    let cap = QUALITY_RESERVE + METADATA_RESERVE + size;
    let mut exact = Ledger::new(cap).unwrap();
    assert_eq!(exact.append(group), Ok(Append::Retained));
    assert!(bytes(&exact).len() <= cap);
    let mut short = Ledger::new(cap - 1).unwrap();
    assert_eq!(
        short.append(transform(
            1,
            &input,
            "Synthetic replacement",
            CleanupStage::Lexicon,
            vec![]
        )),
        Ok(Append::Omitted)
    );
    assert!(
        short.groups().is_empty(),
        "no dangling source or decision fragment retained"
    );
    assert_eq!(short.truncation().omitted_groups, 1);
    assert_eq!(short.truncation().omitted_stages, 2);
    assert_eq!(short.truncation().omitted_rows, 2);
    assert!(matches!(
        short.reconstruct_input(InvocationId(1)),
        Err(Error::Unavailable)
    ));
    let restored = Ledger::read_json(&bytes(&short), cap - 1).unwrap();
    assert_eq!(restored.truncation(), short.truncation());
}

#[test]
fn group_cannot_retain_only_its_source_when_its_linked_decision_does_not_fit() {
    let source_size = wire_size(&base(1, "Before."), usize::MAX).unwrap();
    let cap = QUALITY_RESERVE + METADATA_RESERVE + source_size;
    let mut ledger = Ledger::new(cap).unwrap();
    assert_eq!(
        ledger.append(transform(
            1,
            "Before.",
            "After.",
            CleanupStage::Lexicon,
            vec![]
        )),
        Ok(Append::Omitted)
    );
    assert!(ledger.groups().is_empty());
    assert_eq!(
        ledger.append(base(3, "Later.")),
        Ok(Append::Omitted),
        "truncated tail stays explicit"
    );
    assert_eq!(
        ledger.truncation().last_omitted_invocation,
        Some(InvocationId(3))
    );
    assert_eq!(ledger.truncation().omitted_groups, 2);
}

#[test]
fn final_shared_quality_fits_reserved_room_after_content_is_full_and_truncated() {
    let group = base(1, "Synthetic cap boundary.");
    let size = wire_size(&group, usize::MAX).unwrap();
    let cap = QUALITY_RESERVE + METADATA_RESERVE + size;
    let mut ledger = Ledger::new(cap).unwrap();
    assert_eq!(ledger.append(group), Ok(Append::Retained));
    assert_eq!(
        ledger.append(base(2, "Omitted synthetic window.")),
        Ok(Append::Omitted)
    );
    let quality = full_quality();
    assert_eq!(quality.spans.len(), corti_core::MAX_QUALITY_SPANS);
    assert_eq!(quality.epochs.len(), corti_core::MAX_QUALITY_EPOCHS);
    assert!(wire_size(&quality, usize::MAX).unwrap() < QUALITY_RESERVE);
    ledger.set_final_quality(quality).unwrap();
    let encoded = bytes(&ledger);
    assert!(encoded.len() <= cap);
    let restored = Ledger::read_json(&encoded, cap).unwrap();
    assert_eq!(restored.final_quality(), Some(&full_quality()));
    assert_eq!(restored.truncation().omitted_groups, 1);
    assert_eq!(
        restored.groups()[0].rows[0].segment.text,
        "Synthetic cap boundary."
    );
}

#[test]
fn invalid_utf8_range_unreported_edit_and_nonfinite_time_are_rejected() {
    for edits in [
        vec![TextEdit {
            before: ByteSpan { start: 1, end: 4 },
            after: ByteSpan { start: 0, end: 1 },
            action: EditKind::Case,
        }],
        vec![TextEdit {
            before: ByteSpan { start: 5, end: 9 },
            after: ByteSpan { start: 5, end: 9 },
            action: EditKind::Lexicon {
                rule_id: "synthetic-rule".into(),
            },
        }],
    ] {
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        assert_eq!(
            ledger.append(transform(
                1,
                "🦀 rust",
                "🐚 REST",
                CleanupStage::Lexicon,
                edits
            )),
            Err(Error::InvalidEdit)
        );
    }
    let mut group = base(1, "Synthetic timing.");
    group.rows[0].segment.start = f64::NAN;
    assert_eq!(
        Ledger::new(DEFAULT_CAP).unwrap().append(group),
        Err(Error::InvalidTiming)
    );
}

#[test]
fn processed_wav_cannot_claim_to_be_the_exact_live_aec_input() {
    let mut group = base(1, "Synthetic signal.");
    group.signal.source = SignalSource::RetainedProcessedWav;
    assert_eq!(
        Ledger::new(DEFAULT_CAP).unwrap().append(group),
        Err(Error::InvalidIdentity)
    );
}

#[test]
fn unknown_identity_is_explicit_and_never_fabricated() {
    let mut group = base(1, "Synthetic unknown identity.");
    group.identity.model_hash = None;
    // The existing provenance fingerprint uses 43 base64url bytes, not 64 hex characters.
    group.identity.configuration_hash = Some("_".repeat(43));
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(ledger.append(group), Ok(Append::Retained));
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert!(restored.groups()[0].identity.model_hash.is_none());
    assert_eq!(
        restored.groups()[0].identity.configuration_hash.as_deref(),
        Some("_".repeat(43).as_str())
    );
}

#[test]
fn malformed_truncation_counts_and_future_versions_are_rejected() {
    let cap = QUALITY_RESERVE + METADATA_RESERVE;
    let mut ledger = Ledger::new(cap).unwrap();
    assert_eq!(
        ledger.append(base(1, "Synthetic omitted group.")),
        Ok(Append::Omitted)
    );
    let mut document: Document = serde_json::from_slice(&bytes(&ledger)).unwrap();
    document.truncation.omitted_stages = 2;
    assert!(matches!(
        Ledger::read_json(&serde_json::to_vec(&document).unwrap(), cap),
        Err(Error::InvalidDocument)
    ));
    document.truncation.omitted_stages = 1;
    document.schema_version = VERSION + 1;
    assert!(matches!(
        Ledger::read_json(&serde_json::to_vec(&document).unwrap(), cap),
        Err(Error::InvalidVersion)
    ));
}

#[test]
fn maximum_width_truncation_metadata_fits_its_reserved_room() {
    let document = Document {
        schema_version: VERSION,
        groups: vec![],
        last_invocation: InvocationId(u64::MAX),
        truncation: Truncation {
            omitted_groups: u64::MAX,
            omitted_stages: u64::MAX,
            omitted_rows: u64::MAX,
            first_omitted_invocation: Some(InvocationId(u64::MAX)),
            last_omitted_invocation: Some(InvocationId(u64::MAX)),
            counter_saturated: true,
            omitted_decisions: 0,
            cause: Some(TruncationCause::SerializedCap),
        },
        final_quality: None,
    };
    assert!(wire_size(&document, METADATA_RESERVE).unwrap() < METADATA_RESERVE);
}

#[test]
fn codec_errors_are_category_only_and_do_not_expose_authored_sentinel() {
    const SENTINEL: &str = "SYNTHETIC_PRIVATE_SENTINEL_164";
    let malformed = format!("{{\"schema_version\":\"{SENTINEL}\"}}");
    let error = match Ledger::read_json(malformed.as_bytes(), DEFAULT_CAP) {
        Err(error) => error,
        Ok(_) => panic!("malformed fixture unexpectedly accepted"),
    };
    assert_eq!(format!("{error:?}"), "InvalidDocument");
    assert!(!format!("{error:?}").contains(SENTINEL));
    let mut bad = base(1, SENTINEL);
    bad.identity.model_hash = Some(SENTINEL.into());
    let error = Ledger::new(DEFAULT_CAP).unwrap().append(bad).unwrap_err();
    assert_eq!(format!("{error:?}"), "InvalidIdentity");
    assert!(!format!("{error:?}").contains(SENTINEL));
    struct FailingWriter;
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other(SENTINEL))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    ledger.append(base(1, SENTINEL)).unwrap();
    let error = ledger.write_json(FailingWriter).unwrap_err();
    assert_eq!(format!("{error:?}"), "Io");
    assert!(!format!("{error:?}").contains(SENTINEL));
}
