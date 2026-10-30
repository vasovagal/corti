use super::*;

#[test]
fn rereview_admitted_document_reloads_with_the_same_owned_limit() {
    let mut measured = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(
        measured.append(base(1, "Synthetic boundary.")),
        Ok(Append::Retained)
    );
    let limit = measured.owned_bytes() + QUALITY_OWNED_RESERVE;
    let mut ledger = Ledger::with_limits(DEFAULT_CAP, limit).unwrap();
    assert_eq!(
        ledger.append(base(1, "Synthetic boundary.")),
        Ok(Append::Retained)
    );
    assert!(ledger.owned_bytes() <= limit);
    let encoded = bytes(&ledger);
    let restored = Ledger::read_json_with_limits(&encoded, DEFAULT_CAP, limit);
    let result = match restored {
        Ok(_) => "ok".to_owned(),
        Err(e) => format!("{e:?}"),
    };
    println!(
        "wire_bytes={} retained_owned={} same_owned_limit={} read_result={result}",
        encoded.len(),
        ledger.owned_bytes(),
        limit
    );
    assert_eq!(
        result, "ok",
        "a retained serialized record should reload under its unchanged limits"
    );
}

#[test]
fn rereview_valid_unicode_case_expansion_from_existing_cleanup_is_recordable() {
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

fn nested_groups() -> Vec<Group> {
    let mut first = transform(1, "rust", "REST", CleanupStage::Lexicon, vec![]);
    for row in &mut first.rows {
        row.segment.speaker = Speaker::Other("Synthetic speaker".into());
    }
    let second = base(3, "Another synthetic window.");
    let mut provider = base(4, "unused");
    provider.rows.clear();
    provider.stages[0].kind = StageKind::ProviderInput;
    provider.stages[0].assembled_input = vec![
        ParentRow {
            parent: InvocationId(3),
            row: id(3, 0),
        },
        ParentRow {
            parent: InvocationId(2),
            row: id(2, 0),
        },
    ];
    provider.stages[0].output = vec![id(3, 0), id(2, 0)];
    vec![first, second, provider]
}

fn assert_length_sized_allocations(ledger: &Ledger) {
    fn vector<T>(v: &Vec<T>) {
        assert_eq!(v.capacity(), v.len());
    }
    fn string(s: &String) {
        assert_eq!(s.capacity(), s.len());
    }
    vector(&ledger.document.groups);
    for group in ledger.groups() {
        vector(&group.rows);
        vector(&group.stages);
        for hash in [
            &group.identity.configuration_hash,
            &group.identity.model_hash,
            &group.identity.runtime_hash,
        ]
        .into_iter()
        .flatten()
        {
            string(hash);
        }
        for row in &group.rows {
            string(&row.segment.text);
            if let Speaker::Other(label) = &row.segment.speaker {
                string(label);
            }
        }
        for stage in &group.stages {
            vector(&stage.assembled_input);
            vector(&stage.output);
            vector(&stage.decisions);
            for decision in &stage.decisions {
                vector(&decision.input_indices);
                vector(&decision.originals);
                vector(&decision.replacements);
                vector(&decision.edits);
                for edit in &decision.edits {
                    if let EditKind::Lexicon { rule_id } = &edit.action {
                        string(rule_id);
                    }
                }
            }
        }
    }
    if let Some(quality) = ledger.final_quality() {
        vector(&quality.epochs);
        vector(&quality.spans);
    }
}

#[test]
fn rereview_nested_ownership_and_settled_quality_reload_at_identical_limits() {
    for count in 0..=3 {
        let mut measured = Ledger::new(DEFAULT_CAP).unwrap();
        for group in nested_groups().into_iter().take(count) {
            assert_eq!(measured.append(group), Ok(Append::Retained));
        }
        let limit = measured.owned_bytes() + QUALITY_OWNED_RESERVE;
        for quality_case in 0..3 {
            let mut ledger = Ledger::with_limits(DEFAULT_CAP, limit).unwrap();
            for group in nested_groups().into_iter().take(count) {
                assert_eq!(ledger.append(group), Ok(Append::Retained));
            }
            if quality_case != 0 {
                let mut quality = full_quality();
                if quality_case == 1 {
                    quality.epochs.truncate(1);
                    quality.spans.truncate(1);
                    quality.epochs = quality.epochs.into_boxed_slice().into_vec();
                    quality.spans = quality.spans.into_boxed_slice().into_vec();
                }
                ledger.set_final_quality(quality).unwrap();
            }
            let encoded = bytes(&ledger);
            let restored = Ledger::read_json_with_limits(&encoded, DEFAULT_CAP, limit).unwrap();
            assert_eq!(bytes(&restored), encoded);
            assert!(restored.owned_bytes() <= ledger.owned_bytes());
            assert!(restored.owned_bytes() <= limit);
            assert_eq!(restored.final_quality(), ledger.final_quality());
            assert_length_sized_allocations(&restored);
        }
    }
}

#[test]
fn rereview_external_capacity_is_not_corruption_and_invalid_groups_are_not_omitted() {
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    for group in nested_groups() {
        ledger.append(group).unwrap();
    }
    let encoded = bytes(&ledger);
    assert!(matches!(
        Ledger::read_json_with_limits(
            &encoded,
            DEFAULT_CAP,
            std::mem::size_of::<Ledger>() + QUALITY_OWNED_RESERVE
        ),
        Err(Error::Capacity)
    ));
    assert!(matches!(
        Ledger::read_json_with_limits(
            &encoded,
            QUALITY_RESERVE + METADATA_RESERVE,
            DEFAULT_OWNED_CAP
        ),
        Err(Error::Capacity)
    ));
    let mut invalid: Document = serde_json::from_slice(&encoded).unwrap();
    invalid.groups[2].stages[0].assembled_input[0].parent = InvocationId(1);
    assert!(matches!(
        Ledger::read_json(&serde_json::to_vec(&invalid).unwrap(), DEFAULT_CAP),
        Err(Error::InvalidLink)
    ));
    let mut invalid: Document = serde_json::from_slice(&encoded).unwrap();
    invalid.groups[0].stages[1].decisions[0].edits[0].action = EditKind::Filler;
    assert!(matches!(
        Ledger::read_json(&serde_json::to_vec(&invalid).unwrap(), DEFAULT_CAP),
        Err(Error::InvalidEdit)
    ));
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
fn rereview_unicode_case_mapping_keeps_positions_and_rejects_non_case_rewrites() {
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
