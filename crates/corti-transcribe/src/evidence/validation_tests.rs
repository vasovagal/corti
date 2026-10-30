//! Decision and lineage validation on append and readback.
use super::*;

#[test]
fn valid_unicode_case_expansion_from_existing_cleanup_is_recordable() {
    let input = "Um, ßeta.";
    let (output, fillers, stutters) = crate::segment::strip_fillers_text(input);
    assert_eq!((output.as_str(), fillers, stutters), ("SSeta.", 1, 0));
    let edits = vec![
        TextEdit {
            before: ByteSpan { start: 0, end: 4 },
            after: ByteSpan { start: 0, end: 0 },
            action: EditKind::Filler,
        },
        TextEdit {
            before: ByteSpan { start: 4, end: 6 },
            after: ByteSpan { start: 0, end: 2 },
            action: EditKind::Case,
        },
    ];
    let result = Ledger::new(DEFAULT_CAP).unwrap().append(transform(
        1,
        input,
        &output,
        CleanupStage::Fillers,
        edits,
    ));
    println!("actual_cleanup_unicode_case_trace={result:?}");
    assert_eq!(result, Ok(Append::Retained));
}

fn first_case_trace(before: &str, after: &str) -> Group {
    let original = format!("Um, {before}eta.");
    let output = format!("{after}eta.");
    transform(
        1,
        &original,
        &output,
        CleanupStage::Fillers,
        vec![
            TextEdit {
                before: ByteSpan { start: 0, end: 4 },
                after: ByteSpan { start: 0, end: 0 },
                action: EditKind::Filler,
            },
            TextEdit {
                before: ByteSpan {
                    start: 4,
                    end: 4 + before.len() as u32,
                },
                after: ByteSpan {
                    start: 0,
                    end: after.len() as u32,
                },
                action: EditKind::Case,
            },
        ],
    )
}

#[test]
fn unicode_case_mapping_keeps_positions_and_rejects_non_case_rewrites() {
    // The first three expansions are outputs of the actual existing cleanup, not a new policy.
    for (before, after) in [("ß", "SS"), ("ﬃ", "FFI"), ("ǰ", "J\u{30c}"), ("a", "A")] {
        let original = format!("Um, {before}eta.");
        let (actual, fillers, stutters) = crate::segment::strip_fillers_text(&original);
        assert_eq!(
            (actual.as_str(), fillers, stutters),
            (format!("{after}eta.").as_str(), 1, 0)
        );
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        assert_eq!(
            ledger.append(first_case_trace(before, after)),
            Ok(Append::Retained)
        );
        let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
        assert_eq!(
            restored.reconstruct_input(InvocationId(2)).unwrap()[0]
                .segment
                .text,
            original
        );
    }
    // Same helper as production lower_first; its multi-scalar Unicode output is representable.
    assert!(is_first_scalar_case_mapping("İeta.", "i\u{307}eta."));
    assert!(is_first_scalar_case_mapping("ßeta.", "SSeta."));
    for (before, after) in [("ß", "SX"), ("ß", "ss"), ("SS", "ß"), ("a", "b"), ("", "A")] {
        assert!(matches!(
            Ledger::new(DEFAULT_CAP)
                .unwrap()
                .append(first_case_trace(before, after)),
            Err(Error::InvalidEdit)
        ));
    }
    for mutation in 0..4 {
        let mut bad = first_case_trace("ß", "SS");
        match mutation {
            0 => bad.stages[1].decisions[0].edits[1].before.end = 5,
            1 => bad.stages[1].decisions[0].edits[1].after.end = 1,
            2 => bad.rows[1].segment.text.push('!'),
            _ => bad.stages[1].decisions[0].reason = CleanupReason::TextRule,
        }
        assert!(Ledger::new(DEFAULT_CAP).unwrap().append(bad).is_err());
    }
    assert!(!is_first_scalar_case_mapping("ßeta.", "SSETA."));
}

fn rejects_read(group: Group) -> bool {
    let last_invocation = group.stages.last().unwrap().id;
    let document = Document {
        schema_version: VERSION,
        groups: vec![group],
        last_invocation,
        truncation: Truncation::default(),
        final_quality: None,
    };
    Ledger::read_json(&serde_json::to_vec(&document).unwrap(), DEFAULT_CAP).is_err()
}

#[test]
fn multiple_retained_windows_preserve_ordered_provider_ancestry() {
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let mut first = base(1, "Same.");
    first.window = 1;
    let mut second = base(2, "Same.");
    second.window = 2;
    ledger.append(first).unwrap();
    ledger.append(second).unwrap();
    let asr_before = ledger
        .groups()
        .iter()
        .map(|g| serde_json::to_vec(g).unwrap())
        .collect::<Vec<_>>();
    let mut provider = base(3, "unused");
    provider.rows.clear();
    provider.stages[0].kind = StageKind::ProviderInput;
    provider.stages[0].input = None;
    provider.stages[0].assembled_input = vec![
        ParentRow {
            parent: InvocationId(2),
            row: id(2, 0),
        },
        ParentRow {
            parent: InvocationId(1),
            row: id(1, 0),
        },
    ];
    provider.stages[0].output = vec![id(2, 0), id(1, 0)];
    provider.rows.push(row(4, 0, "Synthetic final prose."));
    provider.stages.push(Stage {
        id: InvocationId(4),
        kind: StageKind::FinalProse,
        input: Some(InvocationId(3)),
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output: vec![id(4, 0)],
        decisions: vec![],
    });
    assert_eq!(ledger.append(provider), Ok(Append::Retained));
    assert_eq!(
        ledger
            .reconstruct_input(InvocationId(3))
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [id(2, 0), id(1, 0)]
    );
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        restored
            .reconstruct_input(InvocationId(4))
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [id(2, 0), id(1, 0)]
    );
    assert_eq!(
        restored
            .groups()
            .iter()
            .map(|g| g.rows.len())
            .sum::<usize>(),
        3
    );
    assert_eq!(
        restored.groups()[..2]
            .iter()
            .map(|g| serde_json::to_vec(g).unwrap())
            .collect::<Vec<_>>(),
        asr_before
    );
}

fn lexicon_with_filler_action() -> Group {
    let mut group = transform(
        1,
        "Synthetic input.",
        "Synthetic replacement.",
        CleanupStage::Lexicon,
        vec![],
    );
    group.stages[1].decisions[0].edits[0].action = EditKind::Filler;
    group
}

#[test]
fn lexicon_rejects_no_rule_id_on_append_and_read() {
    let append_rejected = Ledger::new(DEFAULT_CAP)
        .unwrap()
        .append(lexicon_with_filler_action())
        .is_err();
    let read_rejected = rejects_read(lexicon_with_filler_action());
    assert_eq!((append_rejected, read_rejected), (true, true));
}

fn echo_keep_rewrite() -> Group {
    let mut group = transform(
        1,
        "Synthetic input.",
        "Synthetic replacement.",
        CleanupStage::Echo,
        vec![],
    );
    group.stages[1].decisions[0].reason = CleanupReason::Echo(EchoVerdict::Keep);
    group
}

#[test]
fn echo_keep_rejects_untraced_rewrite_on_append_and_read() {
    let append_rejected = Ledger::new(DEFAULT_CAP)
        .unwrap()
        .append(echo_keep_rewrite())
        .is_err();
    let read_rejected = rejects_read(echo_keep_rewrite());
    assert_eq!((append_rejected, read_rejected), (true, true));
}

fn unrelated_sort_decision() -> Group {
    let mut group = base(1, "Synthetic input.");
    group.rows.extend([
        row(1, 1, "Synthetic second."),
        row(1, 2, "Synthetic third."),
    ]);
    group.stages[0].output.extend([id(1, 1), id(1, 2)]);
    group.stages.push(Stage {
        id: InvocationId(2),
        kind: StageKind::Cleanup {
            pass: CleanupStage::Sort,
        },
        input: Some(InvocationId(1)),
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output: vec![id(1, 0), id(1, 2), id(1, 1)],
        decisions: vec![Decision {
            reason: CleanupReason::TimeOrder,
            input_indices: vec![0],
            originals: vec![id(1, 0)],
            replacements: vec![id(1, 0)],
            edits: vec![],
            evidence: None,
            source: None,
        }],
    });
    group
}

#[test]
fn sort_requires_every_moved_row_decision_on_append_and_read() {
    let append_rejected = Ledger::new(DEFAULT_CAP)
        .unwrap()
        .append(unrelated_sort_decision())
        .is_err();
    let read_rejected = rejects_read(unrelated_sort_decision());
    assert_eq!((append_rejected, read_rejected), (true, true));
}

#[test]
fn text_pass_reason_must_match_the_recorded_pass() {
    let mut group = transform(1, "rust", "REST", CleanupStage::Lexicon, vec![]);
    group.stages[1].decisions[0].reason = CleanupReason::FillersOrStutters;
    assert!(Ledger::new(DEFAULT_CAP).unwrap().append(group).is_err());
}

fn structural_stage(pass: CleanupStage, output: Vec<RowId>, decisions: Vec<Decision>) -> Stage {
    Stage {
        id: InvocationId(2),
        kind: StageKind::Cleanup { pass },
        input: Some(InvocationId(1)),
        assembled_input: vec![],
        context: vec![],
        derivations: vec![],
        protected_output: vec![],
        output,
        decisions,
    }
}

fn decision(reason: CleanupReason, indices: &[u64], replacements: Vec<RowId>) -> Decision {
    Decision {
        reason,
        input_indices: indices.to_vec(),
        originals: indices.iter().map(|i| id(1, *i as u32)).collect(),
        replacements,
        edits: vec![],
        evidence: None,
        source: None,
    }
}

#[test]
fn valid_sort_covers_moved_rows_and_preserves_unchanged_rows() {
    let mut group = unrelated_sort_decision();
    group.rows[1].segment.start = 3.0;
    group.rows[1].segment.end = 4.0;
    group.rows[2].segment.start = 2.0;
    group.rows[2].segment.end = 3.0;
    group.stages[1].decisions = vec![
        decision(CleanupReason::TimeOrder, &[1], vec![id(1, 1)]),
        decision(CleanupReason::TimeOrder, &[2], vec![id(1, 2)]),
    ];
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(ledger.append(group), Ok(Append::Retained));
    let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        restored.groups()[0].stages[1].output,
        [id(1, 0), id(1, 2), id(1, 1)]
    );
    assert_eq!(
        restored
            .reconstruct_input(InvocationId(2))
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        [id(1, 0), id(1, 1), id(1, 2)]
    );
}

#[test]
fn time_valid_sort_still_requires_each_moved_row_decision() {
    for missing in [1, 2] {
        let make = || {
            let mut group = unrelated_sort_decision();
            group.rows[1].segment.start = 3.0;
            group.rows[1].segment.end = 4.0;
            group.rows[2].segment.start = 2.0;
            group.rows[2].segment.end = 3.0;
            let present = 3 - missing;
            group.stages[1].decisions.push(decision(
                CleanupReason::TimeOrder,
                &[present],
                vec![id(1, present as u32)],
            ));
            group
        };
        assert!(Ledger::new(DEFAULT_CAP).unwrap().append(make()).is_err());
        assert!(rejects_read(make()));
    }
}

#[test]
fn merge_has_exact_structural_preimages_and_keeps_unmerged_row() {
    for middle in [" Beta  ", "   "] {
        let mut group = base(1, " Alpha  ");
        group.rows[0].segment.end = 2.0;
        for (slot, text) in [(1, middle), (2, " Gamma  "), (3, "Unchanged.")] {
            let mut item = row(1, slot, text);
            item.segment.start = f64::from(slot * 2 + 1);
            item.segment.end = item.segment.start + 1.0;
            group.rows.push(item);
            group.stages[0].output.push(id(1, slot));
        }
        let intermediate = format!("{} {}", " Alpha  ".trim_end(), middle.trim_start());
        let expected = format!("{} {}", intermediate.trim_end(), " Gamma  ".trim_start());
        let mut merged = row(2, 0, &expected);
        merged.segment.end = 6.0;
        group.rows.push(merged);
        group.stages.push(structural_stage(
            CleanupStage::Merge,
            vec![id(2, 0), id(1, 3)],
            vec![decision(
                CleanupReason::SameSpeakerContinuation,
                &[0, 1, 2],
                vec![id(2, 0)],
            )],
        ));
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        assert_eq!(ledger.append(group), Ok(Append::Retained));
        let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
        assert_eq!(restored.groups()[0].rows[4].segment.text, expected);
        assert_eq!(
            restored.reconstruct_input(InvocationId(2)).unwrap().len(),
            4
        );
    }
}

#[test]
fn protected_echo_and_acoustic_drop_keep_exact_row_identity() {
    for verdict in [
        EchoVerdict::Keep,
        EchoVerdict::KeepCriticalDifference,
        EchoVerdict::SuspectedDuplicate,
        EchoVerdict::DropAcoustic,
    ] {
        let mut group = base(1, "Synthetic echo subject.");
        group.rows.push(row(1, 1, "Unaffected synthetic row."));
        group.stages[0].output.push(id(1, 1));
        let replacements = if verdict == EchoVerdict::DropAcoustic {
            vec![]
        } else {
            vec![id(1, 0)]
        };
        let mut d = decision(CleanupReason::Echo(verdict), &[0], replacements);
        if verdict == EchoVerdict::DropAcoustic {
            d.evidence = Some(EchoEvidence {
                mic_db_bits: (-50.0_f32).to_bits(),
                echo_estimate_db_bits: (-49.0_f32).to_bits(),
                double_talk_fraction_bits: 0.0_f32.to_bits(),
                blocks: 2,
            });
        }
        let mut output = d.replacements.clone();
        output.push(id(1, 1));
        group
            .stages
            .push(structural_stage(CleanupStage::Echo, output, vec![d]));
        if verdict == EchoVerdict::KeepCriticalDifference {
            group.stages[1].protected_output.push(id(1, 0));
        }
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        assert_eq!(ledger.append(group), Ok(Append::Retained));
        let restored = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
        assert_eq!(restored.groups()[0].rows.len(), 2);
        assert_eq!(
            restored.reconstruct_input(InvocationId(2)).unwrap().len(),
            2
        );
    }
}

#[test]
fn backchannel_drop_cannot_resurrect_via_a_later_parent_reference() {
    let mut group = base(1, "Synthetic dropped reply.");
    group.stages.push(structural_stage(
        CleanupStage::Backchannel,
        vec![],
        vec![decision(
            CleanupReason::OverlappingBackchannel,
            &[0],
            vec![],
        )],
    ));
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    ledger.append(group).unwrap();
    let mut provider = base(3, "unused");
    provider.rows.clear();
    provider.stages[0].kind = StageKind::ProviderInput;
    provider.stages[0].output = vec![id(1, 0)];
    provider.stages[0].assembled_input = vec![ParentRow {
        parent: InvocationId(2),
        row: id(1, 0),
    }];
    assert_eq!(ledger.append(provider), Err(Error::InvalidLink));
    assert!(Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).is_ok());
    assert_eq!(
        ledger.reconstruct_input(InvocationId(2)).unwrap()[0].id,
        id(1, 0)
    );
}

#[test]
fn omitted_parent_and_assembled_group_are_unavailable_not_reminted() {
    let first = base(1, "Retained source.");
    let cap = QUALITY_RESERVE + METADATA_RESERVE + wire_size(&first, usize::MAX).unwrap();
    let mut ledger = Ledger::new(cap).unwrap();
    ledger.append(first).unwrap();
    assert_eq!(
        ledger.append(base(2, "Omitted source.")),
        Ok(Append::Omitted)
    );
    let mut provider = base(3, "unused");
    provider.rows.clear();
    provider.stages[0].kind = StageKind::ProviderInput;
    provider.stages[0].output = vec![id(1, 0), id(2, 0)];
    provider.stages[0].assembled_input = vec![
        ParentRow {
            parent: InvocationId(1),
            row: id(1, 0),
        },
        ParentRow {
            parent: InvocationId(2),
            row: id(2, 0),
        },
    ];
    assert_eq!(ledger.append(provider), Ok(Append::Omitted));
    let restored = Ledger::read_json(&bytes(&ledger), cap).unwrap();
    assert_eq!(restored.groups().len(), 1);
    assert!(matches!(
        restored.reconstruct_input(InvocationId(3)),
        Err(Error::Unavailable)
    ));
    assert_eq!(restored.truncation().omitted_groups, 2);
}

#[test]
fn fillers_allow_positioned_case_and_whitespace_without_metadata_changes() {
    let input = "Um,  the gateway.";
    let (output, _, _) = crate::segment::strip_fillers_text(input);
    let edits = vec![
        TextEdit {
            before: ByteSpan { start: 0, end: 3 },
            after: ByteSpan { start: 0, end: 0 },
            action: EditKind::Filler,
        },
        TextEdit {
            before: ByteSpan { start: 3, end: 5 },
            after: ByteSpan { start: 0, end: 0 },
            action: EditKind::Whitespace,
        },
        TextEdit {
            before: ByteSpan { start: 5, end: 6 },
            after: ByteSpan { start: 0, end: 1 },
            action: EditKind::Case,
        },
    ];
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(
        ledger.append(transform(1, input, &output, CleanupStage::Fillers, edits)),
        Ok(Append::Retained)
    );
    assert!(Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).is_ok());
    for mutation in 0..4 {
        let mut bad = transform(1, "rust", "REST", CleanupStage::Lexicon, vec![]);
        match mutation {
            0 => bad.rows[1].segment.start += 0.001,
            1 => bad.rows[1].track = Some(QualityTrack::Tap),
            2 => bad.rows[1].segment.speaker = Speaker::Other("Synthetic other".into()),
            _ => bad.rows[1].source_region_index = Some(99),
        }
        assert!(Ledger::new(DEFAULT_CAP).unwrap().append(bad).is_err());
    }
}
