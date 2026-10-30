//! Integration probes call the real protected cleanup pipeline, not a model of its edits.
use super::*;
use crate::segment::{self, CleanupConfig, CleanupDecisions, SpanEvidence, TextRule};
use std::cell::Cell;

fn info() -> RecordingIdentity {
    let group = base(1, "");
    RecordingIdentity {
        window: group.window,
        kind: StageKind::TrueAsr,
        identity: group.identity,
        signal: group.signal,
    }
}
fn inputs() -> CleanupInput<'static> {
    CleanupInput {
        origins: &[],
        source_links: None,
        carry: &[],
        carry_links: None,
        protected_input_indices: &[],
    }
}
fn no_structural() -> CleanupConfig {
    CleanupConfig {
        echo_drop: false,
        merge_gap_seconds: 0.0,
        drop_backchannels: false,
        strip_fillers: true,
        ..CleanupConfig::default()
    }
}
fn stage_for(ledger: &Ledger, pass: CleanupStage) -> &Stage {
    ledger
        .groups()
        .iter()
        .flat_map(|g| &g.stages)
        .find(|s| s.kind == StageKind::Cleanup { pass })
        .unwrap()
}
fn final_rows(ledger: &Ledger, stage: InvocationId) -> Vec<&Row> {
    let stage = ledger
        .groups()
        .iter()
        .flat_map(|g| &g.stages)
        .find(|s| s.id == stage)
        .unwrap();
    stage
        .output
        .iter()
        .map(|id| ledger.groups().iter().find_map(|g| g.row(*id)).unwrap())
        .collect()
}

#[test]
fn actual_filler_trace_preserves_utf8_preimages_and_stutter_positions() {
    for before in [
        "Very very important.",
        "Um, ßeta.",
        "Um, ﬃeld.",
        "Um, ǰam.",
        "I was, uh, thinking.",
        "I think, um, We should.",
        "we, we roll",
        "we can we can roll",
        "uh",
        "  Um,  we\tgo.  ",
        "Yes, um.",
    ] {
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        let original = row(1, 0, before).segment;
        let legacy = segment::cleanup_with_protected_decisions(
            vec![original.clone()],
            &no_structural(),
            &[],
            None,
            None,
            &[],
        );
        let recorded = ledger.cleanup(
            info(),
            vec![original],
            &no_structural(),
            inputs(),
            None,
            None,
        );
        assert_eq!(recorded.segments, legacy.segments);
        assert_eq!(recorded.stats, legacy.stats);
        assert_eq!(recorded.evidence.unwrap().retained, Append::Retained);
        let loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
        let pass = stage_for(&loaded, CleanupStage::Fillers);
        assert_eq!(
            loaded.reconstruct_input(pass.id).unwrap()[0].segment.text,
            before
        );
        assert_eq!(
            final_rows(&loaded, pass.id)
                .iter()
                .map(|r| &r.segment)
                .collect::<Vec<_>>(),
            recorded.segments.iter().collect::<Vec<_>>()
        );
        if before == "Very very important." {
            let edits = &pass.decisions[0].edits;
            let stutter = edits
                .iter()
                .find(|e| matches!(e.action, EditKind::Stutter))
                .unwrap();
            assert_eq!((stutter.before.start, stutter.before.end), (5, 9));
            assert_eq!(stutter.after.start, stutter.after.end);
        }
    }
}

#[test]
fn actual_structural_passes_keep_explicit_duplicate_row_identity() {
    let mut source = vec![
        row(1, 0, "Second").segment,
        row(1, 1, "First").segment,
        row(1, 2, "tail").segment,
    ];
    source[0].start = 4.0;
    source[0].end = 5.0;
    source[1].start = 1.0;
    source[1].end = 2.0;
    source[2].start = 5.0;
    source[2].end = 6.0;
    let config = CleanupConfig {
        merge_gap_seconds: 0.5,
        ..no_structural()
    };
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let actual = ledger.cleanup(info(), source.clone(), &config, inputs(), None, None);
    let result = actual.evidence.unwrap();
    assert_eq!(result.retained, Append::Retained);
    let loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    let sort = stage_for(&loaded, CleanupStage::Sort);
    assert_eq!(sort.output, vec![id(1, 1), id(1, 0), id(1, 2)]);
    assert_eq!(sort.decisions.len(), 2);
    let merge = stage_for(&loaded, CleanupStage::Merge);
    assert_eq!(merge.decisions[0].originals, vec![id(1, 0), id(1, 2)]);
    assert_eq!(actual.segments[1].text, "Second tail");
    assert_eq!(
        loaded
            .reconstruct_input(result.source)
            .unwrap()
            .iter()
            .map(|r| &r.segment)
            .collect::<Vec<_>>(),
        source.iter().collect::<Vec<_>>()
    );
}

#[test]
fn actual_echo_queries_each_input_once_and_keeps_every_candidate_decision() {
    let mut far = row(1, 0, "we should continue the ordinary discussion").segment;
    far.speaker = Speaker::Other("Synthetic far".into());
    far.start = 0.0;
    far.end = 10.0;
    let near = row(1, 1, "we should continue the ordinary discussion").segment;
    let config = CleanupConfig {
        echo_drop: true,
        ..no_structural()
    };
    let calls = Cell::new(0);
    let audio = |_: f64, _: f64| {
        calls.set(calls.get() + 1);
        Some(SpanEvidence {
            mic_db: -30.0,
            echo_estimate_db: -30.0,
            double_talk_fraction: 0.0,
            blocks: 8,
        })
    };
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let actual = ledger.cleanup(
        info(),
        vec![far, near],
        &config,
        inputs(),
        Some(&audio),
        None,
    );
    assert_eq!(actual.evidence.unwrap().retained, Append::Retained);
    assert_eq!(
        calls.get(),
        2,
        "the existing pass queries each input exactly once"
    );
    assert_eq!(actual.stats.echo_dropped_audio, 1);
    let stage = stage_for(&ledger, CleanupStage::Echo);
    assert!(stage.decisions.iter().any(|d| d.reason
        == CleanupReason::Echo(EchoVerdict::DropAcoustic)
        && d.originals == vec![id(1, 1)]
        && d.replacements.is_empty()));
    assert_eq!(ledger.reconstruct_input(stage.id).unwrap().len(), 2);
}

#[test]
fn actual_critical_echo_protection_and_fallback_identity_survive_readback() {
    let mut far = row(1, 0, "we should proceed").segment;
    far.speaker = Speaker::Other("Synthetic far".into());
    far.start = 0.0;
    far.end = 10.0;
    let near = row(1, 1, "Um, we should not proceed").segment;
    let audio = |_: f64, _: f64| {
        Some(SpanEvidence {
            mic_db: -30.0,
            echo_estimate_db: -30.0,
            double_talk_fraction: 0.0,
            blocks: 8,
        })
    };
    let config = CleanupConfig {
        echo_drop: true,
        ..no_structural()
    };
    let mut identity = info();
    identity.kind = StageKind::FallbackAsr;
    identity.signal = SignalIdentity {
        source: SignalSource::RetainedProcessedWav,
        aec: AecOrigin::Capture,
        source_rate_hz: Some(48_000),
        lookahead_frames: None,
    };
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let recorded = ledger.cleanup(
        identity,
        vec![far, near],
        &config,
        inputs(),
        Some(&audio),
        None,
    );
    assert_eq!(recorded.evidence.unwrap().retained, Append::Retained);
    assert_eq!(recorded.segments[1].text, "Um, we should not proceed");
    assert_eq!(recorded.stats.echo_dropped_audio, 0);
    let loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    let echo = stage_for(&loaded, CleanupStage::Echo);
    assert_eq!(echo.protected_output, vec![id(1, 0), id(1, 1)]);
    assert!(
        echo.decisions
            .iter()
            .any(|decision| decision.originals == vec![id(1, 1)]
                && decision.reason == CleanupReason::Echo(EchoVerdict::KeepCriticalDifference))
    );
    for group in loaded.groups() {
        assert_eq!(
            group.identity.configuration_hash.as_deref(),
            Some("a".repeat(64).as_str())
        );
        assert_eq!(
            group.identity.model_hash.as_deref(),
            Some("b".repeat(64).as_str())
        );
        assert_eq!(
            group.identity.runtime_hash.as_deref(),
            Some("c".repeat(64).as_str())
        );
        assert_eq!(group.signal.source, SignalSource::RetainedProcessedWav);
        assert_eq!(group.signal.aec, AecOrigin::Capture);
    }
    assert_eq!(loaded.groups()[0].stages[0].kind, StageKind::FallbackAsr);
}

#[test]
fn actual_legacy_rule_runs_once_and_seals_missing_evidence_without_losing_output() {
    struct Legacy(Cell<usize>);
    impl TextRule for Legacy {
        fn apply(&self, _: &str) -> Option<String> {
            self.0.set(self.0.get() + 1);
            Some("Synthetic replacement".into())
        }
    }
    let rule = Legacy(Cell::new(0));
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let actual = ledger.cleanup(
        info(),
        vec![row(1, 0, "Synthetic input").segment],
        &no_structural(),
        inputs(),
        None,
        Some(&rule),
    );
    assert_eq!(rule.0.get(), 1);
    assert_eq!(actual.segments[0].text, "Synthetic replacement");
    assert_eq!(actual.evidence.unwrap().retained, Append::Omitted);
    assert_eq!(
        ledger.truncation().cause,
        Some(TruncationCause::TraceUnavailable)
    );
    let again = ledger.cleanup(
        info(),
        vec![row(1, 0, "Later input").segment],
        &no_structural(),
        inputs(),
        None,
        Some(&rule),
    );
    assert_eq!(rule.0.get(), 2);
    assert_eq!(again.segments[0].text, "Synthetic replacement");
    assert_eq!(again.evidence.unwrap().retained, Append::Omitted);
    assert_eq!(
        Ledger::read_json(&bytes(&ledger), DEFAULT_CAP)
            .unwrap()
            .truncation()
            .omitted_decisions,
        ledger.truncation().omitted_decisions
    );
}

#[test]
fn actual_early_echo_retains_withheld_inputs_and_source_ordinals() {
    let source = vec![
        row(1, 0, "withheld").segment,
        row(1, 1, "published").segment,
    ];
    let origins = [
        InputOrigin {
            track: Some(QualityTrack::Mic),
            source_region_index: Some(42),
        },
        InputOrigin {
            track: Some(QualityTrack::Mic),
            source_region_index: Some(43),
        },
    ];
    let mut decisions = CleanupDecisions::default();
    decisions.record_echo(
        CleanupStage::EarlyEcho,
        42,
        &source[0],
        EchoVerdict::KeepCriticalDifference,
        None,
    );
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let recorded = ledger
        .early_echo(info(), &source, &origins, decisions)
        .unwrap();
    assert_eq!(recorded.retained, Append::Retained);
    let pass = stage_for(&ledger, CleanupStage::EarlyEcho);
    assert_eq!(pass.output, vec![id(1, 0), id(1, 1)]);
    assert_eq!(pass.decisions[0].input_indices, vec![42]);
    let loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        loaded.reconstruct_input(recorded.source).unwrap()[0].source_region_index,
        Some(42)
    );
}

#[test]
fn actual_upstream_decision_omission_seals_evidence_but_preserves_known_input() {
    let mut decisions = CleanupDecisions::default();
    decisions.omitted = 3;
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let result = ledger
        .early_echo(
            info(),
            &[row(1, 0, "Synthetic retained input").segment],
            &[],
            decisions,
        )
        .unwrap();
    assert_eq!(result.retained, Append::Omitted);
    assert_eq!(
        ledger.truncation().cause,
        Some(TruncationCause::UpstreamDecisions)
    );
    assert_eq!(ledger.truncation().omitted_decisions, 3);
    assert_eq!(ledger.truncation().omitted_rows, 0);
    assert_eq!(
        ledger.reconstruct_input(result.source).unwrap()[0]
            .segment
            .text,
        "Synthetic retained input"
    );
    Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
}

#[test]
fn actual_protection_survives_sort_merge_and_skipped_filler_pass() {
    let mut first = row(1, 0, "Um, preserve this.").segment;
    first.start = 4.0;
    first.end = 5.0;
    let mut last = row(1, 1, "Very very important.").segment;
    last.start = 5.0;
    last.end = 6.0;
    let mut early = row(1, 2, "unprotected").segment;
    early.start = 0.0;
    early.end = 1.0;
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let config = CleanupConfig {
        merge_gap_seconds: 0.5,
        ..no_structural()
    };
    let recorded = ledger.cleanup(
        info(),
        vec![first, last, early],
        &config,
        CleanupInput {
            protected_input_indices: &[0],
            ..inputs()
        },
        None,
        None,
    );
    assert_eq!(recorded.evidence.unwrap().retained, Append::Retained);
    assert_eq!(
        recorded.segments[1].text,
        "Um, preserve this. Very very important."
    );
    assert_eq!(
        ledger.groups()[0].stages[0].protected_output,
        vec![id(1, 0)]
    );
    let filler = stage_for(&ledger, CleanupStage::Fillers);
    assert!(filler.decisions.is_empty());
    assert_eq!(filler.protected_output.len(), 1);
    let loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        stage_for(&loaded, CleanupStage::Fillers).protected_output,
        filler.protected_output
    );
    let mut corrupt: serde_json::Value = serde_json::from_slice(&bytes(&ledger)).unwrap();
    corrupt["groups"][1]["stages"][0]["protected_output"] = serde_json::json!([]);
    assert!(matches!(
        Ledger::read_json(&serde_json::to_vec(&corrupt).unwrap(), DEFAULT_CAP),
        Err(Error::InvalidLink)
    ));
}

#[test]
fn actual_carry_uses_caller_links_or_explicit_unattributed_snapshot() {
    let mut carry = row(1, 0, "we should continue the ordinary discussion").segment;
    carry.speaker = Speaker::Other("Synthetic far".into());
    carry.start = 0.0;
    carry.end = 10.0;
    let near = row(1, 0, "we should continue the ordinary discussion").segment;
    let config = CleanupConfig {
        echo_drop: true,
        ..no_structural()
    };
    for linked in [false, true] {
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        let links = if linked {
            let prior = ledger
                .cleanup(
                    info(),
                    vec![carry.clone()],
                    &no_structural(),
                    inputs(),
                    None,
                    None,
                )
                .evidence
                .unwrap();
            vec![ParentRow {
                parent: prior.final_stage.unwrap(),
                row: id(1, 0),
            }]
        } else {
            vec![]
        };
        let result = ledger.cleanup(
            info(),
            vec![near.clone()],
            &config,
            CleanupInput {
                carry: std::slice::from_ref(&carry),
                carry_links: linked.then_some(links.as_slice()),
                ..inputs()
            },
            None,
            None,
        );
        assert_eq!(result.evidence.unwrap().retained, Append::Retained);
        let echo = stage_for(&ledger, CleanupStage::Echo);
        assert_eq!(echo.context.len(), 1);
        if linked {
            assert_eq!(echo.context, links);
        } else {
            let group = ledger
                .groups()
                .iter()
                .find(|g| g.stages[0].kind == StageKind::ContextSnapshot)
                .unwrap();
            assert_eq!(group.signal.source, SignalSource::UnattributedContext);
            assert!(group.identity.model_hash.is_none());
        }
        Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    }
}

#[test]
fn production_preflight_rejects_small_wire_large_owner_expansion_before_decode() {
    // The serde default Vec path would allocate thousands of large Group owners from tiny objects.
    let bytes = format!(
        "{{\"groups\":[{}]}}",
        std::iter::repeat_n("{}", 2048)
            .collect::<Vec<_>>()
            .join(",")
    );
    assert!(bytes.len() < 8192);
    assert_eq!(
        preflight::check(bytes.as_bytes(), QUALITY_OWNED_RESERVE + 1024),
        Err(Error::Capacity)
    );
    assert!(matches!(
        Ledger::read_json_with_limits(bytes.as_bytes(), DEFAULT_CAP, QUALITY_OWNED_RESERVE + 1024),
        Err(Error::Capacity)
    ));
    // A tiny-input nesting attack never reaches typed ownership and has no content-bearing error.
    let nested = format!("{}null{}", "{\"unknown\":".repeat(130), "}".repeat(130));
    assert_eq!(
        preflight::check(nested.as_bytes(), DEFAULT_OWNED_CAP),
        Err(Error::InvalidDocument)
    );
}

#[test]
fn actual_backchannel_deletion_keeps_its_preimage_and_caller_protection() {
    let mut far = row(1, 0, "we discuss the ordinary things").segment;
    far.speaker = Speaker::Other("Synthetic far".into());
    far.start = 0.0;
    far.end = 10.0;
    let near = row(1, 1, "Yeah.").segment;
    let config = CleanupConfig {
        drop_backchannels: true,
        ..no_structural()
    };
    for protected in [false, true] {
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        let actual = ledger.cleanup(
            info(),
            vec![far.clone(), near.clone()],
            &config,
            CleanupInput {
                protected_input_indices: if protected { &[1] } else { &[] },
                ..inputs()
            },
            None,
            None,
        );
        assert_eq!(actual.evidence.unwrap().retained, Append::Retained);
        assert_eq!(actual.stats.backchannels_dropped, usize::from(!protected));
        let stage = stage_for(&ledger, CleanupStage::Backchannel);
        assert_eq!(stage.decisions.len(), usize::from(!protected));
        assert_eq!(
            ledger.reconstruct_input(stage.id).unwrap()[1].segment.text,
            "Yeah."
        );
        Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    }
}

#[test]
fn actual_trace_and_capture_caps_preserve_cleanup_output_and_mark_missing_evidence() {
    let before = "Um, we we go. ".repeat(10_000);
    let (expected, fillers, stutters) = segment::strip_fillers_text(&before);
    let (output, actual_fillers, actual_stutters, trace) =
        segment::strip_fillers_text_traced(&before);
    assert_eq!(output, expected);
    assert_eq!((actual_fillers, actual_stutters), (fillers, stutters));
    assert_eq!(trace.gap(), Some(segment::TraceGap::Capacity));
    assert!(trace.edits().is_empty());
    assert_eq!(trace.owned_bytes(), 0);
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let result = ledger.cleanup(
        info(),
        vec![row(1, 0, &before).segment],
        &no_structural(),
        inputs(),
        None,
        None,
    );
    assert_eq!(result.segments[0].text, expected);
    assert_eq!(result.evidence.unwrap().retained, Append::Omitted);
    assert_eq!(
        ledger.truncation().cause,
        Some(TruncationCause::TraceUnavailable)
    );
    assert_eq!(ledger.truncation().omitted_decisions, 1);
    assert_eq!(ledger.truncation().omitted_rows, 0);
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let rows = (0..=MAX_CAPTURE_ROWS)
        .map(|_| row(1, 0, "unchanged").segment)
        .collect();
    let result = ledger.cleanup(
        info(),
        rows,
        &CleanupConfig {
            strip_fillers: false,
            ..no_structural()
        },
        inputs(),
        None,
        None,
    );
    assert_eq!(result.segments.len(), MAX_CAPTURE_ROWS + 1);
    assert_eq!(result.evidence.unwrap().retained, Append::Omitted);
    assert_eq!(
        ledger.truncation().cause,
        Some(TruncationCause::TransientCap)
    );
    assert_eq!(
        ledger.truncation().omitted_rows,
        (MAX_CAPTURE_ROWS + 1) as u64
    );
    Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
}

#[test]
fn actual_early_echo_rejects_ambiguous_ordinals_instead_of_matching_identical_text() {
    let source = vec![
        row(1, 0, "identical").segment,
        row(1, 1, "identical").segment,
    ];
    let origins = [InputOrigin {
        source_region_index: Some(42),
        ..InputOrigin::default()
    }; 2];
    let mut decisions = CleanupDecisions::default();
    decisions.record_echo(
        CleanupStage::EarlyEcho,
        42,
        &source[1],
        EchoVerdict::Keep,
        None,
    );
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert!(matches!(
        ledger.early_echo(info(), &source, &origins, decisions),
        Err(Error::InvalidLink)
    ));
}

#[test]
fn actual_filler_position_corpus_covers_punctuation_unicode_and_repeated_bigrams() {
    let tokens = [
        "Um,", "we,", "we", "go", "uh.", "Very", "very", "ßeta", "ﬃeld", "ǰam", "Um", "May", "may",
    ];
    for first in tokens {
        for second in tokens {
            for third in tokens {
                let before = format!("{first}  {second}\t{third} {second} {third}");
                let (expected, fillers, stutters) = segment::strip_fillers_text(&before);
                let (after, traced_fillers, traced_stutters, trace) =
                    segment::strip_fillers_text_traced(&before);
                assert_eq!(after, expected);
                assert_eq!((traced_fillers, traced_stutters), (fillers, stutters));
                assert_eq!(trace.gap(), None);
                assert_eq!(validate_edits(&before, &after, trace.edits()), Ok(()));
            }
        }
    }
}

#[test]
fn actual_early_rows_assemble_for_cleanup_without_reminting_asr_or_copying_text() {
    let source = vec![
        row(1, 0, "Um, identical.").segment,
        row(1, 1, "Um, identical.").segment,
    ];
    let origins = [
        InputOrigin {
            source_region_index: Some(41),
            ..InputOrigin::default()
        },
        InputOrigin {
            source_region_index: Some(42),
            ..InputOrigin::default()
        },
    ];
    let mut decisions = CleanupDecisions::default();
    decisions.record_echo(
        CleanupStage::EarlyEcho,
        42,
        &source[1],
        EchoVerdict::KeepCriticalDifference,
        None,
    );
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let early = ledger
        .early_echo(info(), &source, &origins, decisions)
        .unwrap();
    let parent = early.final_stage.unwrap();
    // Released in an explicitly different order; equal strings/times must not collapse identities.
    let links = [
        ParentRow {
            parent,
            row: id(1, 1),
        },
        ParentRow {
            parent,
            row: id(1, 0),
        },
    ];
    let mut identity = info();
    identity.kind = StageKind::CleanupAssembly;
    let recorded = ledger.cleanup(
        identity,
        vec![source[1].clone(), source[0].clone()],
        &no_structural(),
        CleanupInput {
            source_links: Some(&links),
            protected_input_indices: &[0],
            ..inputs()
        },
        None,
        None,
    );
    let result = recorded.evidence.unwrap();
    assert_eq!(result.retained, Append::Retained);
    assert_eq!(recorded.segments[0].text, "Um, identical.");
    assert_eq!(recorded.segments[1].text, "Identical.");
    let assembly = ledger
        .groups()
        .iter()
        .find(|g| g.stages[0].id == result.source)
        .unwrap();
    assert!(assembly.rows.is_empty());
    assert_eq!(assembly.stages[0].assembled_input, links);
    assert_eq!(assembly.stages[0].output, vec![id(1, 1), id(1, 0)]);
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
    assert_eq!(
        loaded
            .reconstruct_input(result.source)
            .unwrap()
            .iter()
            .map(|r| r.id)
            .collect::<Vec<_>>(),
        vec![id(1, 1), id(1, 0)]
    );
}

#[test]
fn actual_assembly_missing_or_omitted_parents_never_change_cleanup_output() {
    let source = row(1, 0, "Um, genuine output.").segment;
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let missing = [ParentRow {
        parent: InvocationId(900),
        row: id(900, 0),
    }];
    let mut identity = info();
    identity.kind = StageKind::CleanupAssembly;
    let recorded = ledger.cleanup(
        identity,
        vec![source.clone()],
        &no_structural(),
        CleanupInput {
            source_links: Some(&missing),
            ..inputs()
        },
        None,
        None,
    );
    assert!(matches!(recorded.evidence, Err(Error::Unavailable)));
    assert_eq!(recorded.segments[0].text, "Genuine output.");
    assert!(ledger.groups().is_empty());
    // Once the evidence cap seals the suffix, later assembly cannot resurrect omitted owners.
    let mut ledger = Ledger::new(QUALITY_RESERVE + METADATA_RESERVE).unwrap();
    let early = ledger
        .early_echo(
            info(),
            std::slice::from_ref(&source),
            &[],
            CleanupDecisions::default(),
        )
        .unwrap();
    assert_eq!(early.retained, Append::Omitted);
    let links = [ParentRow {
        parent: early.source,
        row: RowId {
            invocation: early.source,
            slot: 0,
        },
    }];
    let mut identity = info();
    identity.kind = StageKind::CleanupAssembly;
    let recorded = ledger.cleanup(
        identity,
        vec![source],
        &no_structural(),
        CleanupInput {
            source_links: Some(&links),
            ..inputs()
        },
        None,
        None,
    );
    assert_eq!(recorded.evidence.unwrap().retained, Append::Omitted);
    assert_eq!(recorded.segments[0].text, "Genuine output.");
    assert!(ledger.groups().is_empty());
    assert_eq!(ledger.truncation().omitted_rows, 1);
    Ledger::read_json(&bytes(&ledger), QUALITY_RESERVE + METADATA_RESERVE).unwrap();
}

#[test]
fn production_dense_source_round_trips_at_the_same_minimum_owned_limit() {
    let dense = || {
        let mut group = base(1, "x");
        group.rows.clear();
        group.stages[0].output.clear();
        for slot in 0..8192 {
            group.rows.push(row(1, slot, "x"));
            group.stages[0].output.push(id(1, slot));
        }
        group
    };
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(ledger.append(dense()), Ok(Append::Retained));
    let minimum = ledger.owned_bytes() + QUALITY_OWNED_RESERVE;
    let mut tight = Ledger::with_limits(DEFAULT_CAP, minimum).unwrap();
    assert_eq!(tight.append(dense()), Ok(Append::Retained));
    let loaded = Ledger::read_json_with_limits(&bytes(&tight), DEFAULT_CAP, minimum).unwrap();
    assert_eq!(loaded.groups()[0].rows.len(), 8192);
    assert!(loaded.owned_bytes() <= tight.owned_bytes());
}

#[test]
fn review_invalid_carry_seals_the_missing_suffix_across_readback_and_later_windows() {
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    ledger.set_final_quality(QualityRecord::observed()).unwrap();
    let carry = [row(1, 0, "synthetic carry").segment];
    let missing = [ParentRow {
        parent: InvocationId(900),
        row: id(900, 0),
    }];
    let actual = ledger.cleanup(
        info(),
        vec![row(1, 0, "Um, first window.").segment],
        &no_structural(),
        CleanupInput {
            carry: &carry,
            carry_links: Some(&missing),
            ..inputs()
        },
        None,
        None,
    );
    assert_eq!(actual.segments[0].text, "First window.");
    assert!(matches!(actual.evidence, Err(Error::Unavailable)));
    assert_eq!(ledger.groups().len(), 1);
    let mut loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    let later = loaded.cleanup(
        info(),
        vec![row(1, 0, "Um, later window.").segment],
        &no_structural(),
        inputs(),
        None,
        None,
    );
    assert_eq!(later.segments[0].text, "Later window.");
    assert_eq!(later.evidence.unwrap().retained, Append::Omitted);
    assert_eq!(loaded.groups().len(), 1);
    assert_eq!(
        loaded.truncation().cause,
        Some(TruncationCause::TraceUnavailable)
    );
    assert_eq!(loaded.final_quality(), Some(&QualityRecord::observed()));
}

#[test]
fn review_source_and_assembly_errors_before_admission_remain_durable_omissions() {
    for assembly in [false, true] {
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        let origins = [InputOrigin::default(); 2];
        let missing = [ParentRow {
            parent: InvocationId(900),
            row: id(900, 0),
        }];
        let mut identity = info();
        if assembly {
            identity.kind = StageKind::CleanupAssembly;
        }
        let actual = ledger.cleanup(
            identity,
            vec![row(1, 0, "Um, ordinary output.").segment],
            &no_structural(),
            if assembly {
                CleanupInput {
                    source_links: Some(&missing),
                    ..inputs()
                }
            } else {
                CleanupInput {
                    origins: &origins,
                    ..inputs()
                }
            },
            None,
            None,
        );
        assert!(actual.evidence.is_err());
        assert_eq!(actual.segments[0].text, "Ordinary output.");
        assert!(ledger.groups().is_empty());
        let loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
        assert_eq!(
            loaded.truncation().cause,
            Some(TruncationCause::TraceUnavailable)
        );
        assert_eq!(loaded.truncation().omitted_stages, 3);
        assert_eq!(loaded.truncation().omitted_rows, u64::from(!assembly));
        assert_eq!(loaded.truncation().omitted_decisions, 1);
    }
}

#[test]
fn review_late_text_trace_error_keeps_prior_stages_and_seals_only_missing_suffix() {
    struct BadTrace;
    impl TextRule for BadTrace {
        fn apply(&self, _: &str) -> Option<String> {
            Some("Synthetic replacement.".into())
        }
        fn apply_traced(&self, text: &str) -> segment::TracedText {
            segment::TracedText {
                replacement: self.apply(text),
                edits: segment::PositionedEdits::default(),
            }
        }
    }
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    let actual = ledger.cleanup(
        info(),
        vec![row(1, 0, "Original.").segment],
        &no_structural(),
        inputs(),
        None,
        Some(&BadTrace),
    );
    assert_eq!(actual.segments[0].text, "Synthetic replacement.");
    assert!(matches!(actual.evidence, Err(Error::InvalidEdit)));
    assert_eq!(ledger.groups().len(), 3);
    let loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        loaded.truncation().cause,
        Some(TruncationCause::TraceUnavailable)
    );
    assert_eq!(loaded.truncation().omitted_stages, 1);
    assert_eq!(loaded.truncation().omitted_decisions, 1);
    assert_eq!(
        loaded.reconstruct_input(InvocationId(1)).unwrap()[0]
            .segment
            .text,
        "Original."
    );
}

#[test]
fn review_early_echo_errors_seal_source_and_decision_omissions() {
    let input = row(1, 0, "Synthetic early row").segment;
    let mut decisions = CleanupDecisions::default();
    decisions.record_echo(CleanupStage::EarlyEcho, 42, &input, EchoVerdict::Keep, None);
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert!(matches!(
        ledger.early_echo(info(), &[input], &[], decisions),
        Err(Error::InvalidLink)
    ));
    assert_eq!(ledger.groups().len(), 1);
    let loaded = Ledger::read_json(&bytes(&ledger), DEFAULT_CAP).unwrap();
    assert_eq!(
        loaded.truncation().cause,
        Some(TruncationCause::TraceUnavailable)
    );
    assert_eq!(loaded.truncation().omitted_stages, 1);
    assert_eq!(loaded.truncation().omitted_decisions, 1);
}

#[test]
fn review_cleanup_assembly_inherits_protection_on_append_and_read_in_both_link_forms() {
    for assembled in [false, true] {
        let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
        let first = ledger
            .cleanup(
                info(),
                vec![row(1, 0, "Um, protected.").segment],
                &no_structural(),
                CleanupInput {
                    protected_input_indices: &[0],
                    ..inputs()
                },
                None,
                None,
            )
            .evidence
            .unwrap();
        let parent = first.final_stage.unwrap();
        let next = ledger.next_invocation().unwrap();
        let group = |protected| {
            let identity = info();
            Group {
                window: 8,
                identity: identity.identity,
                signal: identity.signal,
                rows: vec![],
                stages: vec![Stage {
                    id: next,
                    kind: StageKind::CleanupAssembly,
                    input: (!assembled).then_some(parent),
                    assembled_input: if assembled {
                        vec![ParentRow {
                            parent,
                            row: id(1, 0),
                        }]
                    } else {
                        vec![]
                    },
                    context: vec![],
                    derivations: vec![],
                    protected_output: if protected { vec![id(1, 0)] } else { vec![] },
                    output: vec![id(1, 0)],
                    decisions: vec![],
                }],
            }
        };
        assert_eq!(ledger.append(group(false)), Err(Error::InvalidLink));
        assert_eq!(ledger.append(group(true)), Ok(Append::Retained));
        let mut corrupt: serde_json::Value = serde_json::from_slice(&bytes(&ledger)).unwrap();
        corrupt["groups"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["stages"][0]["protected_output"] = serde_json::json!([]);
        assert!(matches!(
            Ledger::read_json(&serde_json::to_vec(&corrupt).unwrap(), DEFAULT_CAP),
            Err(Error::InvalidLink)
        ));
    }
}

#[test]
fn review_cleanup_assembly_rejects_minted_rows_but_retains_genuinely_empty_assembly() {
    let group = |owns_row| {
        let identity = info();
        Group {
            window: 8,
            identity: identity.identity,
            signal: identity.signal,
            rows: if owns_row {
                vec![row(1, 0, "Synthetic minted text")]
            } else {
                vec![]
            },
            stages: vec![Stage {
                id: InvocationId(1),
                kind: StageKind::CleanupAssembly,
                input: None,
                assembled_input: vec![],
                context: vec![],
                derivations: vec![],
                protected_output: vec![],
                output: if owns_row { vec![id(1, 0)] } else { vec![] },
                decisions: vec![],
            }],
        }
    };
    let mut ledger = Ledger::new(DEFAULT_CAP).unwrap();
    assert_eq!(ledger.append(group(true)), Err(Error::InvalidLink));
    let mut corrupt: serde_json::Value = serde_json::from_slice(&bytes(&ledger)).unwrap();
    corrupt["groups"] = serde_json::json!([group(true)]);
    corrupt["last_invocation"] = serde_json::json!(1);
    assert!(matches!(
        Ledger::read_json(&serde_json::to_vec(&corrupt).unwrap(), DEFAULT_CAP),
        Err(Error::InvalidLink)
    ));
    assert_eq!(ledger.append(group(false)), Ok(Append::Retained));
    assert!(
        Ledger::read_json(&bytes(&ledger), DEFAULT_CAP)
            .unwrap()
            .groups()[0]
            .rows
            .is_empty()
    );
}
