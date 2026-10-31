//! Owned-capacity budgets, reload limits and capacity errors.
use super::*;

#[test]
fn admitted_document_reloads_with_the_same_owned_limit() {
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
fn nested_ownership_and_settled_quality_reload_at_identical_limits() {
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
fn external_capacity_is_not_corruption_and_invalid_groups_are_not_omitted() {
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

#[test]
fn tiny_wire_record_cannot_retain_unbudgeted_string_capacity() {
    let cap = QUALITY_RESERVE + METADATA_RESERVE + 4096;
    let mut group = base(1, "unused");
    let mut text = String::with_capacity(1024 * 1024);
    text.push_str("Synthetic input.");
    group.rows[0].segment.text = text;
    let mut ledger = Ledger::new(cap).unwrap();
    assert_eq!(ledger.append(group), Ok(Append::Omitted));
    assert!(ledger.groups().is_empty());
    assert_eq!(
        ledger.truncation().cause,
        Some(TruncationCause::OwnedMemoryCap)
    );
    assert!(ledger.owned_bytes() <= ledger.owned_limit());
}

#[test]
fn every_nested_owned_capacity_is_budgeted_even_when_not_serialized() {
    let cap = QUALITY_RESERVE + METADATA_RESERVE + 4096;
    for variant in 0..13 {
        let mut group = transform(1, "rust", "REST", CleanupStage::Lexicon, vec![]);
        match variant {
            0 => group.rows.reserve_exact(8192),
            1 => group.stages.reserve_exact(8192),
            2 => group.rows[0].segment.text.reserve_exact(1024 * 1024),
            3 => {
                let mut label = String::with_capacity(1024 * 1024);
                label.push_str("Synthetic speaker");
                group.rows[0].segment.speaker = Speaker::Other(label);
                group.rows[1].segment.speaker = Speaker::Other("Synthetic speaker".into());
            }
            4 => group
                .identity
                .configuration_hash
                .as_mut()
                .unwrap()
                .reserve_exact(1024 * 1024),
            5 => group.stages[1].assembled_input.reserve_exact(65536),
            6 => group.stages[1].output.reserve_exact(65536),
            7 => group.stages[1].decisions.reserve_exact(8192),
            8 => group.stages[1].decisions[0]
                .input_indices
                .reserve_exact(131072),
            9 => group.stages[1].decisions[0].originals.reserve_exact(65536),
            10 => group.stages[1].decisions[0]
                .replacements
                .reserve_exact(65536),
            11 => group.stages[1].decisions[0].edits.reserve_exact(65536),
            _ => {
                if let EditKind::Lexicon { rule_id } =
                    &mut group.stages[1].decisions[0].edits[0].action
                {
                    rule_id.reserve_exact(1024 * 1024);
                }
            }
        }
        assert!(wire_size(&group, 4096).is_ok());
        let mut ledger = Ledger::new(cap).unwrap();
        assert_eq!(
            ledger.append(group),
            Ok(Append::Omitted),
            "capacity variant {variant}"
        );
        assert_eq!(
            ledger.truncation().cause,
            Some(TruncationCause::OwnedMemoryCap)
        );
        assert!(ledger.owned_bytes() <= ledger.owned_limit());
    }
}

#[test]
fn owned_budget_is_exact_across_growth_and_reserves_final_quality() {
    let first = base(1, "Synthetic owned budget.");
    let owned = std::mem::size_of::<Ledger>()
        + std::mem::size_of::<Group>()
        + first.heap_bytes()
        + QUALITY_OWNED_RESERVE;
    let mut ledger = Ledger::with_limits(DEFAULT_CAP, owned).unwrap();
    assert_eq!(ledger.append(first), Ok(Append::Retained));
    let retained_before = ledger.owned_bytes();
    assert_eq!(
        ledger.append(base(2, "Another group.")),
        Ok(Append::Omitted)
    );
    assert_eq!(ledger.owned_bytes(), retained_before);
    ledger.set_final_quality(full_quality()).unwrap();
    assert!(ledger.owned_bytes() <= owned);
    let mut oversized = full_quality();
    oversized.epochs.reserve_exact(8192);
    assert_eq!(
        ledger.set_final_quality(oversized),
        Err(Error::InvalidQuality)
    );
    assert_eq!(ledger.final_quality(), Some(&full_quality()));
    let encoded = bytes(&ledger);
    assert!(
        Ledger::read_json_with_limits(
            &encoded,
            DEFAULT_CAP,
            std::mem::size_of::<Ledger>() + QUALITY_OWNED_RESERVE
        )
        .is_err()
    );
    let restored = Ledger::read_json_with_limits(&encoded, DEFAULT_CAP, owned).unwrap();
    assert!(restored.owned_bytes() <= restored.owned_limit());
    assert_eq!(
        restored.truncation().cause,
        Some(TruncationCause::OwnedMemoryCap)
    );
}
