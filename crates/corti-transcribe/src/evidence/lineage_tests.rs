use super::*;
use crate::segment::{self, CleanupDecisions, SpanEvidence, SpeakerTurn, Word};

#[test]
fn actual_early_echo_context_keeps_exact_raw_and_carry_parents_and_seals_bad_or_excess_links() {
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let raw = far("identical synthetic context");
    let first = ledger
        .capture(
            info(StageKind::TrueAsr),
            std::slice::from_ref(&raw),
            &[],
            &[],
        )
        .unwrap()
        .source;
    let second = ledger
        .capture(
            info(StageKind::TrueAsr),
            std::slice::from_ref(&raw),
            &[],
            &[],
        )
        .unwrap()
        .source;
    let context = [output(&ledger, second)[0], output(&ledger, first)[0]];
    let mic = segment(Speaker::Me, 1.0, 2.0, "synthetic microphone row");
    let origin = [InputOrigin {
        track: Some(QualityTrack::Mic),
        source_region_index: Some(42),
    }];
    let decisions = || {
        let mut decisions = CleanupDecisions::default();
        decisions.record_echo(
            CleanupStage::EarlyEcho,
            42,
            &mic,
            EchoVerdict::SuspectedDuplicate,
            None,
        );
        decisions
    };
    let actual = ledger
        .early_echo_with_context(
            info(StageKind::TrueAsr),
            std::slice::from_ref(&mic),
            &origin,
            decisions(),
            &context,
        )
        .unwrap();
    let mut loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        loaded.stage(actual.final_stage.unwrap()).unwrap().context,
        context
    );
    assert_eq!(
        loaded.stage(actual.final_stage.unwrap()).unwrap().decisions[0].input_indices,
        [42]
    );
    assert_ne!(context[0].row, context[1].row);
    let missing = [ParentRow {
        parent: InvocationId(900),
        row: id(900, 0),
    }];
    assert!(
        loaded
            .early_echo_with_context(
                info(StageKind::TrueAsr),
                std::slice::from_ref(&mic),
                &origin,
                decisions(),
                &missing
            )
            .is_err()
    );
    let loaded = Ledger::read_json(&bytes(&loaded), DEFAULT_CAP).unwrap();
    assert_eq!(
        loaded.truncation().cause,
        Some(TruncationCause::TraceUnavailable)
    );
    assert_eq!(loaded.truncation().omitted_stages, 1);
    assert_eq!(loaded.truncation().omitted_decisions, 1);
    let mut capped = Ledger::new(DEFAULT_CAP).unwrap();
    let excess = vec![context[0]; MAX_CAPTURE_ROWS + 1];
    assert_eq!(
        capped
            .early_echo_with_context(
                info(StageKind::TrueAsr),
                std::slice::from_ref(&mic),
                &origin,
                decisions(),
                &excess
            )
            .unwrap()
            .retained,
        Append::Omitted
    );
    assert_eq!(
        capped.truncation().cause,
        Some(TruncationCause::TransientCap)
    );
    assert_eq!(mic.text, "synthetic microphone row");
}

fn info(kind: StageKind) -> RecordingIdentity {
    let group = base(1, "");
    RecordingIdentity {
        window: group.window,
        kind,
        identity: group.identity,
        signal: group.signal,
    }
}
fn segment(speaker: Speaker, start: f64, end: f64, text: &str) -> TranscriptSegment {
    TranscriptSegment {
        speaker,
        start,
        end,
        text: text.into(),
    }
}
fn output(ledger: &Ledger, parent: InvocationId) -> Vec<ParentRow> {
    ledger
        .stage(parent)
        .unwrap()
        .output
        .iter()
        .map(|row| ParentRow { parent, row: *row })
        .collect()
}
fn far(text: &str) -> TranscriptSegment {
    segment(Speaker::Other("Them".into()), 0.0, 5.0, text)
}
fn split_ledger() -> Ledger {
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let source = ledger
        .capture(
            info(StageKind::TrueAsr),
            &[far("écho now later.")],
            &[],
            &[0],
        )
        .unwrap()
        .source;
    let original = ledger.stage(source).unwrap().output[0];
    let descendants = [
        DerivedSegment {
            original,
            source: ByteSpan { start: 0, end: 9 },
            segment: segment(Speaker::Other("A".into()), 0.0, 1.0, "écho now"),
        },
        DerivedSegment {
            original,
            source: ByteSpan { start: 10, end: 16 },
            segment: segment(Speaker::Other("B".into()), 4.0, 5.0, "later."),
        },
    ];
    assert_eq!(
        ledger
            .derive_segments(info(StageKind::Diarization), source, &descendants)
            .unwrap()
            .retained,
        Append::Retained
    );
    ledger
}

#[test]
fn actual_poll_echo_split_child_echo_diarization_and_cleanup_keep_one_asr_lineage() {
    let words = vec![
        Word {
            start: 0.0,
            end: 0.4,
            text: "écho".into(),
        },
        Word {
            start: 0.5,
            end: 1.0,
            text: "now".into(),
        },
        Word {
            start: 4.0,
            end: 5.0,
            text: "later.".into(),
        },
    ];
    let raw =
        segment::words_to_segments(&words, Speaker::Other("Them".into()), f64::INFINITY).remove(0);
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let source = ledger
        .capture(
            info(StageKind::TrueAsr),
            std::slice::from_ref(&raw),
            &[],
            &[],
        )
        .unwrap()
        .source;
    let mut decisions = CleanupDecisions::default();
    decisions.record_echo(
        CleanupStage::Echo,
        0,
        &raw,
        EchoVerdict::KeepCriticalDifference,
        None,
    );
    let echo = ledger
        .record_echo(
            info(StageKind::Cleanup {
                pass: CleanupStage::Echo,
            }),
            source,
            decisions,
            &[],
        )
        .unwrap()
        .final_stage
        .unwrap();
    let original = ledger.stage(echo).unwrap().output[0];
    let children = segment::split_regions(&words, segment::SEGMENT_GAP);
    let grouped = segment::words_to_segments_with_ranges(
        &words,
        Speaker::Other("Them".into()),
        segment::SEGMENT_GAP,
    );
    let descendants: Vec<_> = grouped
        .segments
        .into_iter()
        .zip(grouped.source_ranges.unwrap())
        .map(|(segment, source)| DerivedSegment {
            original,
            source,
            segment,
        })
        .collect();
    let split = ledger
        .derive_segments(info(StageKind::Segmentation), echo, &descendants)
        .unwrap()
        .final_stage
        .unwrap();
    let child_links = output(&ledger, split);
    let turns = [
        SpeakerTurn {
            start: 0.0,
            end: 0.45,
            label: "A".into(),
        },
        SpeakerTurn {
            start: 0.5,
            end: 5.0,
            label: "B".into(),
        },
    ];
    let mut final_links = Vec::new();
    let mut rendered = Vec::new();
    for ((child, supplied), link) in children.iter().zip(&descendants).zip(child_links) {
        let assembled = ledger
            .assemble(info(StageKind::CleanupAssembly), &[link], &[])
            .unwrap()
            .source;
        let mut decisions = CleanupDecisions::default();
        decisions.record_echo(
            CleanupStage::Echo,
            0,
            &supplied.segment,
            EchoVerdict::SuspectedDuplicate,
            None,
        );
        let echo = ledger
            .record_echo(
                info(StageKind::Cleanup {
                    pass: CleanupStage::Echo,
                }),
                assembled,
                decisions,
                &[],
            )
            .unwrap()
            .final_stage
            .unwrap();
        let actual =
            segment::diarize_words_with_ranges(child, &turns, segment::SEGMENT_GAP, "Them");
        let original = ledger.stage(echo).unwrap().output[0];
        let descendants: Vec<_> = actual
            .segments
            .iter()
            .zip(actual.source_ranges.unwrap())
            .map(|(segment, source)| DerivedSegment {
                original,
                source,
                segment: segment.clone(),
            })
            .collect();
        let diarized = ledger
            .derive_segments(info(StageKind::Diarization), echo, &descendants)
            .unwrap()
            .final_stage
            .unwrap();
        final_links.extend(output(&ledger, diarized));
        rendered.extend(actual.segments);
    }
    let protected: Vec<_> = (0..rendered.len()).collect();
    let result = ledger.cleanup(
        info(StageKind::CleanupAssembly),
        rendered.clone(),
        &segment::CleanupConfig {
            echo_drop: false,
            ..segment::CleanupConfig::default()
        },
        CleanupInput {
            origins: &[],
            source_links: Some(&final_links),
            carry: &[],
            carry_links: None,
            protected_input_indices: &protected,
        },
        None,
        None,
    );
    assert_eq!(result.segments, rendered);
    assert_eq!(result.evidence.unwrap().retained, Append::Retained);
    assert_eq!(
        ledger
            .groups()
            .iter()
            .flat_map(|g| &g.stages)
            .filter(|s| s.kind == StageKind::TrueAsr)
            .count(),
        1
    );
    let loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(loaded.reconstruct_input(echo).unwrap()[0].segment, raw);
    assert_eq!(loaded.stage(split).unwrap().protected_output.len(), 2);
    assert_eq!(
        loaded.row(original).unwrap().segment.text,
        "écho now later."
    );
}

#[test]
fn lineage_shared_append_and_readback_reject_lost_duplicated_fabricated_ranges_or_protection() {
    let original: serde_json::Value = serde_json::from_slice(&bytes(&split_ledger())).unwrap();
    for mutation in 0..11 {
        let mut corrupt = original.clone();
        let stage = &mut corrupt["groups"][1]["stages"][0];
        match mutation {
            0 => stage["derivations"][0]["source"]["end"] = serde_json::json!(8),
            1 => stage["derivations"][1]["source"]["start"] = serde_json::json!(8),
            2 => stage["derivations"][0]["source"]["start"] = serde_json::json!(1),
            3 => stage["derivations"][0]["original"]["invocation"] = serde_json::json!(900),
            4 => stage["derivations"].as_array_mut().unwrap().swap(0, 1),
            5 => stage["protected_output"] = serde_json::json!([]),
            6 => stage["input"] = serde_json::Value::Null,
            7 => stage["derivations"]
                .as_array_mut()
                .unwrap()
                .pop()
                .map(|_| ())
                .unwrap(),
            8 => corrupt["groups"][1]["rows"][0]["segment"]["start"] = serde_json::json!(-1.0),
            9 => corrupt["groups"][1]["rows"][0]["segment"]["end"] = serde_json::json!(6.0),
            10 => {
                corrupt["groups"][1]["rows"][0]["segment"]["text"] = serde_json::json!("Invented")
            }
            _ => unreachable!(),
        }
        assert!(
            Ledger::read_json(&serde_json::to_vec(&corrupt).unwrap(), DEFAULT_CAP).is_err(),
            "read mutation {mutation}"
        );
        let group: Group =
            serde_json::from_value(corrupt["groups"].as_array_mut().unwrap().pop().unwrap())
                .unwrap();
        corrupt["last_invocation"] = serde_json::json!(1);
        let mut prefix =
            Ledger::read_json(&serde_json::to_vec(&corrupt).unwrap(), DEFAULT_CAP).unwrap();
        assert!(prefix.append(group).is_err(), "append mutation {mutation}");
    }
}

#[test]
fn lineage_full_unchanged_duplicate_rows_keep_distinct_owners_and_empty_derived_stages_work() {
    let raw = [far("same"), far("same")];
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let source = ledger
        .capture(info(StageKind::TrueAsr), &raw, &[], &[])
        .unwrap()
        .source;
    let descendants: Vec<_> = raw
        .iter()
        .enumerate()
        .map(|(slot, segment)| DerivedSegment {
            original: id(source.0, slot as u32),
            source: ByteSpan { start: 0, end: 4 },
            segment: segment.clone(),
        })
        .collect();
    let final_stage = ledger
        .derive_segments(info(StageKind::Segmentation), source, &descendants)
        .unwrap()
        .final_stage
        .unwrap();
    assert_eq!(
        ledger.stage(final_stage).unwrap().output,
        [id(1, 0), id(1, 1)]
    );
    assert!(ledger.groups()[1].rows.is_empty());
    let empty = ledger
        .assemble(info(StageKind::CleanupAssembly), &[], &[])
        .unwrap()
        .source;
    ledger
        .derive_segments(info(StageKind::Diarization), empty, &[])
        .unwrap();
    assert!(Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).is_ok());
}

#[test]
fn lineage_bad_caller_map_seals_evidence_and_keeps_borrowed_outputs_and_quality() {
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    ledger.set_final_quality(full_quality()).unwrap();
    let source = ledger
        .capture(info(StageKind::TrueAsr), &[far("original")], &[], &[])
        .unwrap()
        .source;
    let descendants = [DerivedSegment {
        original: id(1, 0),
        source: ByteSpan { start: 0, end: 8 },
        segment: far("replacement"),
    }];
    assert!(
        ledger
            .derive_segments(info(StageKind::Segmentation), source, &descendants)
            .is_err()
    );
    assert_eq!(descendants[0].segment.text, "replacement");
    let mut loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        loaded.truncation().cause,
        Some(TruncationCause::TraceUnavailable)
    );
    assert_eq!(loaded.final_quality(), Some(&full_quality()));
    assert_eq!(
        loaded
            .capture(info(StageKind::TrueAsr), &[far("later")], &[], &[])
            .unwrap()
            .retained,
        Append::Omitted
    );
}

#[test]
fn lineage_raw_echo_consumes_exact_decisions_context_and_cannot_restore_a_withheld_row() {
    let raw = [
        segment(Speaker::Me, 0.0, 1.0, "synthetic dropped"),
        segment(Speaker::Me, 2.0, 3.0, "synthetic kept"),
    ];
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let source = ledger
        .capture(info(StageKind::TrueAsr), &raw, &[], &[])
        .unwrap()
        .source;
    let context = output(&ledger, source);
    let mut decisions = CleanupDecisions::default();
    decisions.record_echo(
        CleanupStage::Echo,
        0,
        &raw[0],
        EchoVerdict::DropAcoustic,
        Some(SpanEvidence {
            mic_db: -30.0,
            echo_estimate_db: -20.0,
            double_talk_fraction: 0.0,
            blocks: 1,
        }),
    );
    let echo = ledger
        .record_echo(
            info(StageKind::Cleanup {
                pass: CleanupStage::Echo,
            }),
            source,
            decisions,
            &context[1..],
        )
        .unwrap()
        .final_stage
        .unwrap();
    assert_eq!(ledger.stage(echo).unwrap().output, [id(1, 1)]);
    assert_eq!(ledger.stage(echo).unwrap().context, context[1..]);
    assert_eq!(ledger.reconstruct_input(echo).unwrap().len(), 2);
    assert!(
        ledger
            .assemble(
                info(StageKind::CleanupAssembly),
                &[ParentRow {
                    parent: echo,
                    row: id(1, 0)
                }],
                &[]
            )
            .is_err()
    );
    assert!(
        Ledger::read_json(&bytes(&ledger), DEFAULT_CAP)
            .unwrap()
            .truncation()
            .cause
            .is_some()
    );
}

#[test]
fn lineage_new_owner_and_range_vectors_obey_identical_limits_on_readback() {
    let ledger = split_ledger();
    let minimum = ledger.owned_bytes() + QUALITY_OWNED_RESERVE;
    let loaded = Ledger::read_json_with_limits(&bytes(&ledger), DEFAULT_CAP, minimum).unwrap();
    assert!(loaded.owned_bytes() <= ledger.owned_bytes());
    let mut tight = Ledger::with_limits(
        DEFAULT_CAP,
        std::mem::size_of::<Ledger>() + QUALITY_OWNED_RESERVE + 2048,
    )
    .unwrap();
    let source = tight
        .capture(
            info(StageKind::TrueAsr),
            &[far("écho now later.")],
            &[],
            &[0],
        )
        .unwrap()
        .source;
    let descendants: Vec<_> = (0..MAX_CAPTURE_ROWS + 1)
        .map(|_| DerivedSegment {
            original: id(source.0, 0),
            source: ByteSpan { start: 0, end: 2 },
            segment: far("é"),
        })
        .collect();
    assert_eq!(
        tight
            .derive_segments(info(StageKind::Diarization), source, &descendants)
            .unwrap()
            .retained,
        Append::Omitted
    );
    assert_eq!(descendants.len(), MAX_CAPTURE_ROWS + 1);
    assert_eq!(tight.groups().len(), 1);
    assert!(
        Ledger::read_json(&bytes(&tight), DEFAULT_CAP)
            .unwrap()
            .truncation()
            .omitted_decisions
            > 0
    );
}

#[test]
fn actual_word_range_emitters_match_legacy_rows_and_cover_unicode_empty_words_and_trace_cap() {
    let vocabulary = ["é", "", "two", "ßeta", "終", "end."];
    let turns = [
        SpeakerTurn {
            start: 0.0,
            end: 4.0,
            label: "A".into(),
        },
        SpeakerTurn {
            start: 4.0,
            end: 40.0,
            label: "B".into(),
        },
    ];
    for seed in 0..256 {
        let words: Vec<_> = (0..16)
            .map(|index| Word {
                start: index as f64 * 2.0,
                end: index as f64 * 2.0 + 0.8,
                text: vocabulary[(index + seed) % vocabulary.len()].into(),
            })
            .collect();
        let source = words
            .iter()
            .filter(|word| !word.text.is_empty())
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        for diarize in [false, true] {
            let (actual, legacy) = if diarize {
                (
                    segment::diarize_words_with_ranges(&words, &turns, 1.5, "Them"),
                    segment::diarize_words(&words, &turns, 1.5, "Them"),
                )
            } else {
                (
                    segment::words_to_segments_with_ranges(&words, Speaker::Me, 1.5),
                    segment::words_to_segments(&words, Speaker::Me, 1.5),
                )
            };
            assert_eq!(actual.segments, legacy);
            for (row, span) in actual.segments.iter().zip(actual.source_ranges.unwrap()) {
                assert_eq!(
                    source.get(span.start as usize..span.end as usize),
                    Some(row.text.as_str())
                );
            }
        }
    }
    let words: Vec<_> = (0..32769)
        .map(|index| Word {
            start: index as f64 * 3.0,
            end: index as f64 * 3.0 + 1.0,
            text: "x".into(),
        })
        .collect();
    let actual = segment::words_to_segments_with_ranges(&words, Speaker::Me, 1.5);
    assert!(actual.source_ranges.is_none());
    assert_eq!(
        actual.segments,
        segment::words_to_segments(&words, Speaker::Me, 1.5)
    );
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    ledger.mark_unavailable(actual.segments.len(), 0).unwrap();
    assert_eq!(
        Ledger::read_json(&bytes(&ledger), DEFAULT_CAP)
            .unwrap()
            .truncation()
            .omitted_rows,
        32769
    );
}
