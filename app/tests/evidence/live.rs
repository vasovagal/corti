fn evidence_window(name: &str, diarize: bool) -> (TranscriptWindow, NoteWriter<TempFiler>) {
    let filer = TempFiler::new(name);
    let mut metadata = meta();
    metadata.audio_path = filer.dir.join("recording.wav");
    std::fs::write(&metadata.audio_path, b"synthetic-only").unwrap();
    let config = crate::config::AppConfig::default();
    let mut writer = NoteWriter::with_provenance(
        filer,
        metadata,
        None,
        crate::provenance::from_config(&config, corti_vagus::provenance::GenerationMode::Live),
    );
    writer.evidence_config = Some(Arc::new(std::sync::Mutex::new(config.clone())));
    let mut window = TranscriptWindow::new(16_000, 1, diarize, config.cleanup_config()).unwrap();
    window.arm_evidence(&writer, &config);
    (window, writer)
}

#[test]
fn actual_live_evidence_preserves_critical_raw_protection_through_diarization() {
    for reverse in [false, true] {
        let (mut window, mut writer) = evidence_window(
            if reverse {
                "evidence-protected-mic"
            } else {
                "evidence-protected-tap"
            },
            true,
        );
        let full = vec![word(0.0, 10.0, "The gateway dimensions are in mm today.")];
        let parts = vec![
            word(1.0, 2.0, "The gateway dimensions are in"),
            word(5.0, 6.0, "um"),
            word(9.0, 10.0, "today."),
        ];
        let (mic, tap) = if reverse {
            (parts, full)
        } else {
            (full, parts)
        };
        publish_mic_words(&mut window, &NoopPublisher, mic);
        publish_them_words(&mut window, &NoopPublisher, tap);
        release_held_mic_words(&mut window, &NoopPublisher);
        let diarizer = ScriptedDiarizer {
            turns: vec![SpeakerTurn {
                start: 0.0,
                end: 11.0,
                label: "Synthetic speaker".into(),
            }],
            calls: RefCell::new(Vec::new()),
        };
        flush_window(&diarizer, &mut window, &mut writer).unwrap();
        assert_eq!(diarizer.calls.borrow().len(), 1);
        let note = std::fs::read_to_string(writer.path().unwrap()).unwrap();
        assert!(note.contains(if reverse {
            "**[00:05] Me:** um"
        } else {
            "**[00:05] Synthetic speaker:** um"
        }));
        // Actual final input assembly must carry the same known protected parents, not reject them.
        window
            .evidence
            .as_mut()
            .unwrap()
            .provider_input(&writer.final_transcript().unwrap());
        window.persist_evidence(&writer, None);
        let path = crate::evidence::path_for(&writer.meta.audio_path);
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(
            document["ledger"]["truncation"]["cause"].is_null(),
            "{}",
            document["ledger"]["truncation"]
        );
        let stages: Vec<_> = document["ledger"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["stages"].as_array().unwrap())
            .collect();
        assert!(
            stages
                .iter()
                .any(|stage| stage["kind"]["kind"] == "diarization")
        );
        assert!(stages.iter().any(|stage| {
            stage["protected_output"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty())
        }));
        std::fs::remove_dir_all(&writer.filer.dir).unwrap();
    }
}

#[test]
fn actual_live_finalization_records_only_selected_hosted_prose() {
    struct Publisher(crate::postprocess_app::HostedHandle);
    impl TranscriptPublisher for Publisher {
        fn words(&self, _: Speaker, _: &[Word]) {}
        fn hosted(&self) -> Option<&crate::postprocess_app::HostedHandle> {
            Some(&self.0)
        }
        fn recording_id(&self) -> Option<&str> {
            Some("synthetic-hosted-boundary")
        }
    }
    for mode in [1, 2, 3, 4] {
        crate::postprocess_app::evidence_hosted_mode(mode);
        let (mut window, mut writer) =
            evidence_window(&format!("evidence-final-fence-{mode}"), false);
        if mode == 4 {
            publish_mic_words(
                &mut window,
                &NoopPublisher,
                vec![word(61.0, 62.0, "Later synthetic statement.")],
            );
            release_held_mic_words(&mut window, &NoopPublisher);
            flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        }
        publish_mic_words(
            &mut window,
            &NoopPublisher,
            vec![word(1.0, 2.0, "Um, Original synthetic statement.")],
        );
        release_held_mic_words(&mut window, &NoopPublisher);
        let outcome = finish_session(
            16_000,
            None,
            Scripted::new(vec![], vec![]),
            Scripted::new(vec![], vec![]),
            NoDiarizer,
            window,
            FinishQuality {
                dropped_chunks: 0,
                capture: Some(corti_core::QualityRecord::observed()),
            },
            &mut writer,
            &Publisher(crate::postprocess_app::HostedHandle),
        )
        .unwrap();
        assert!(matches!(outcome, LiveOutcome::Filed { .. }));
        let bytes = std::fs::read(crate::evidence::path_for(&writer.meta.audio_path)).unwrap();
        let serialized = String::from_utf8(bytes.clone()).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(document["ledger"]["truncation"]["cause"].is_null());
        let stages: Vec<_> = document["ledger"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["stages"].as_array().unwrap())
            .collect();
        assert_eq!(
            stages
                .iter()
                .filter(|stage| stage["kind"]["kind"] == "true_asr")
                .count(),
            if mode == 4 { 2 } else { 1 }
        );
        assert_eq!(
            stages
                .iter()
                .any(|stage| stage["kind"]["kind"] == "provider_input"),
            mode != 3
        );
        assert_eq!(
            serialized.contains("Selected synthetic hosted prose."),
            mode == 1
        );
        let note = std::fs::read_to_string(writer.path().unwrap()).unwrap();
        assert_eq!(note.contains("Selected synthetic hosted prose."), mode == 1);
        assert_eq!(note.contains("Original synthetic statement."), mode != 1);
        assert!(!note.contains("fingerprint_kind"));
        std::fs::remove_dir_all(&writer.filer.dir).unwrap();
        crate::postprocess_app::evidence_hosted_mode(0);
    }
}

#[test]
fn actual_live_aec_consume_and_finish_record_track_specific_short_lookahead() {
    for enabled in [false, true] {
        let (mut window, mut writer) = evidence_window(
            if enabled {
                "evidence-aec"
            } else {
                "evidence-no-aec"
            },
            false,
        );
        let rate = 16_000;
        let cfg = crate::config::AppConfig::default();
        let mut aec = enabled.then(|| new_live_aec(rate, cfg.aec_config()));
        let mut mic = Scripted::new(vec![], vec![word(0.1, 0.2, "Synthetic near speech.")]);
        let mut tap = Scripted::new(vec![], vec![word(0.3, 0.4, "Different far statement.")]);
        let epoch = fixture_epoch(rate, true, true);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(CaptureChunk {
            epoch,
            retained_start_frame: 0,
            mic: vec![0.01; 8_000],
            tap: vec![0.005; 8_000],
            unavailable: Vec::new(),
        })
        .unwrap();
        drop(tx);
        consume_chunks(
            &rx,
            rate,
            &mut aec,
            &mut mic,
            &mut tap,
            &NoDiarizer,
            &mut window,
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        let mut capture = corti_core::QualityRecord::observed();
        capture.insert_epoch(epoch).unwrap();
        let outcome = finish_session(
            rate,
            aec,
            mic,
            tap,
            NoDiarizer,
            window,
            FinishQuality {
                dropped_chunks: 0,
                capture: Some(capture),
            },
            &mut writer,
            &NoopPublisher,
        )
        .unwrap();
        assert!(matches!(outcome, LiveOutcome::Filed { .. }));
        let document: serde_json::Value = serde_json::from_slice(
            &std::fs::read(crate::evidence::path_for(&writer.meta.audio_path)).unwrap(),
        )
        .unwrap();
        assert!(document["ledger"]["truncation"]["cause"].is_null());
        let groups = document["ledger"]["groups"].as_array().unwrap();
        for track in ["mic", "tap"] {
            let group = groups
                .iter()
                .find(|group| {
                    group["stages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|stage| stage["kind"]["kind"] == "true_asr")
                        && group["rows"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|row| row["track"] == track)
                })
                .unwrap();
            assert_eq!(group["signal"]["source"], "live_stream");
            assert_eq!(
                group["signal"]["aec"],
                if enabled && track == "mic" {
                    "live_short_lookahead"
                } else {
                    "none"
                }
            );
            assert_eq!(group["signal"]["source_rate_hz"], rate);
            if enabled && track == "mic" {
                assert_eq!(
                    group["signal"]["lookahead_frames"],
                    corti_aec::lookahead_samples_for(rate, cfg.aec_config().filter_len, 0.1) as u64
                );
            } else {
                assert!(group["signal"]["lookahead_frames"].is_null());
            }
        }
        // Only mic audio passes through live AEC. Window assembly, its cleanup and the final prose
        // combine both tracks, so their lineage cannot claim it.
        let mixed: Vec<_> = groups
            .iter()
            .filter(|group| {
                group["stages"].as_array().unwrap().iter().any(|stage| {
                    matches!(stage["kind"]["kind"].as_str(), Some("final_prose" | "provider_input"))
                        || stage["kind"]["pass"] == "sort"
                })
            })
            .collect();
        assert!(!mixed.is_empty());
        for group in mixed {
            assert_eq!(group["signal"]["aec"], "none", "{group}");
            assert!(group["signal"]["lookahead_frames"].is_null(), "{group}");
        }
        std::fs::remove_dir_all(&writer.filer.dir).unwrap();
    }
}

#[test]
fn actual_live_flush_keeps_one_asr_per_input_and_persists_windows_before_note_failure() {
    let (mut window, mut writer) = evidence_window("evidence-live-window", true);
    publish_mic_words(
        &mut window,
        &NoopPublisher,
        vec![word(
            1.234,
            2.987,
            "Um, Very very syntheticquartzfalcon important.",
        )],
    );
    publish_them_words(
        &mut window,
        &NoopPublisher,
        vec![
            word(4.0, 5.0, "The gateway uses mm."),
            word(8.0, 9.0, "Later item."),
        ],
    );
    release_held_mic_words(&mut window, &NoopPublisher);
    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
    let path = crate::evidence::path_for(&writer.meta.audio_path);
    let before = std::fs::read(&path).unwrap();
    let document: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert!(
        document["ledger"]["truncation"]["cause"].is_null(),
        "{}",
        document["ledger"]["truncation"]
    );
    let groups = document["ledger"]["groups"].as_array().unwrap();
    let asr = groups
        .iter()
        .flat_map(|group| group["stages"].as_array().unwrap())
        .filter(|stage| stage["kind"]["kind"] == "true_asr")
        .count();
    assert_eq!(
        asr, 2,
        "early mic input and one far poll, without flush reminting"
    );
    let note = std::fs::read_to_string(writer.path().unwrap()).unwrap();
    assert!(!note.contains("Um, Very very"));
    assert!(!note.contains("fingerprint_kind"));
    assert!(!note.contains("\"groups\""));
    // The next durable evidence generation must exist even if the subsequent note append fails.
    let note_path = writer.path().unwrap().clone();
    std::fs::remove_file(&note_path).unwrap();
    std::fs::create_dir(&note_path).unwrap();
    publish_mic_words(
        &mut window,
        &NoopPublisher,
        vec![word(61.0, 62.0, "Second synthetic window.")],
    );
    release_held_mic_words(&mut window, &NoopPublisher);
    assert!(flush_window(&NoDiarizer, &mut window, &mut writer).is_err());
    let after = std::fs::read(&path).unwrap();
    assert_ne!(before, after);
    assert!(
        String::from_utf8(after)
            .unwrap()
            .contains("Second synthetic window.")
    );
    assert!(
        crate::checkpoint::temporary_paths(&writer.meta.audio_path)
            .unwrap()
            .is_empty()
    );
    std::fs::remove_dir_all(&writer.filer.dir).unwrap();
}

#[test]
fn actual_live_optout_releases_text_and_never_recreates_removed_evidence() {
    let (mut window, mut writer) = evidence_window("evidence-live-optout", false);
    publish_mic_words(
        &mut window,
        &NoopPublisher,
        vec![word(1.0, 2.0, "First synthetic window.")],
    );
    release_held_mic_words(&mut window, &NoopPublisher);
    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
    writer
        .evidence_config
        .as_ref()
        .unwrap()
        .lock()
        .unwrap()
        .evidence
        .enabled = false;
    crate::evidence::remove(&writer.meta.audio_path).unwrap();
    publish_mic_words(
        &mut window,
        &NoopPublisher,
        vec![word(61.0, 62.0, "Second synthetic window.")],
    );
    release_held_mic_words(&mut window, &NoopPublisher);
    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
    assert!(window.evidence.is_none());
    assert!(!crate::evidence::path_for(&writer.meta.audio_path).exists());
    assert!(writer.meta.audio_path.exists());
    assert!(
        std::fs::read_to_string(writer.path().unwrap())
            .unwrap()
            .contains("Second synthetic window.")
    );
    std::fs::remove_dir_all(&writer.filer.dir).unwrap();
}

#[test]
fn actual_live_evidence_cap_never_drops_words_or_stops_later_windows() {
    let (mut window, mut writer) = evidence_window("evidence-live-cap", false);
    let cap = 256 * 1024 + 4096 + 2048;
    window.evidence.as_mut().unwrap().set_test_wire_cap(cap);
    for index in 0..20 {
        let text = format!("Synthetic window number {index} remains filed.");
        publish_mic_words(
            &mut window,
            &NoopPublisher,
            vec![word(index as f64 * 60.0, index as f64 * 60.0 + 1.0, &text)],
        );
        release_held_mic_words(&mut window, &NoopPublisher);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
    }
    let bytes = std::fs::read(crate::evidence::path_for(&writer.meta.audio_path)).unwrap();
    assert!(bytes.len() <= cap + 4096);
    let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(!document["ledger"]["truncation"]["cause"].is_null());
    assert!(
        document["ledger"]["truncation"]["omitted_stages"]
            .as_u64()
            .unwrap()
            > 0
    );
    let note = std::fs::read_to_string(writer.path().unwrap()).unwrap();
    for index in 0..20 {
        assert!(note.contains(&format!("Synthetic window number {index} remains filed.")));
    }
    assert!(
        crate::checkpoint::temporary_paths(&writer.meta.audio_path)
            .unwrap()
            .is_empty()
    );
    std::fs::remove_dir_all(&writer.filer.dir).unwrap();
}

#[test]
fn actual_live_withheld_early_echo_retains_original_ordinal_and_explicit_context() {
    let (mut window, mut writer) = evidence_window("evidence-live-withheld", false);
    window.frames = 5 * 16_000;
    window.push_aec_blocks(vec![block(3.5, 1.0, false)]);
    publish_mic_words(
        &mut window,
        &NoopPublisher,
        vec![word(3.5, 4.0, "Gateway harness.")],
    );
    publish_them_words(
        &mut window,
        &NoopPublisher,
        vec![
            word(0.0, 1.0, "An introductory statement."),
            word(3.0, 5.0, "The unrelated weather report."),
        ],
    );
    assert_eq!(window.early_drop.stats.released_dropped, 1);
    release_held_mic_words(&mut window, &NoopPublisher);
    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
    let bytes = std::fs::read(crate::evidence::path_for(&writer.meta.audio_path)).unwrap();
    let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(document["ledger"]["truncation"]["cause"].is_null());
    let groups = document["ledger"]["groups"].as_array().unwrap();
    let mic = groups
        .iter()
        .filter(|group| {
            group["stages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|stage| stage["kind"]["kind"] == "true_asr")
        })
        .flat_map(|group| group["rows"].as_array().unwrap())
        .find(|row| row["track"] == "mic")
        .unwrap();
    assert_eq!(mic["segment"]["text"], "Gateway harness.");
    assert_eq!(mic["source_region_index"], 0);
    let early = groups
        .iter()
        .flat_map(|group| group["stages"].as_array().unwrap())
        .find(|stage| stage["kind"]["pass"] == "early_echo")
        .unwrap();
    assert!(early["output"].as_array().unwrap().is_empty());
    assert!(!early["context"].as_array().unwrap().is_empty());
    assert!(
        !std::fs::read_to_string(writer.path().unwrap())
            .unwrap()
            .contains("Gateway harness.")
    );
    std::fs::remove_dir_all(&writer.filer.dir).unwrap();
}

fn persisted_early_stage(writer: &NoteWriter<TempFiler>) -> (serde_json::Value, serde_json::Value) {
    let document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(crate::evidence::path_for(&writer.meta.audio_path)).unwrap(),
    )
    .unwrap();
    let early = document["ledger"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["stages"].as_array().unwrap())
        .find(|stage| stage["kind"]["pass"] == "early_echo")
        .unwrap()
        .clone();
    (document, early)
}

#[test]
fn actual_live_early_echo_context_matches_pruned_classifier_inventory() {
    let (mut window, writer) = evidence_window("evidence-early-pruned-context", false);
    window.frames = 16_000;
    publish_them_words(
        &mut window,
        &NoopPublisher,
        vec![word(0.0, 1.0, "Expired comparison source.")],
    );
    window.frames = 20 * 16_000;
    publish_them_words(
        &mut window,
        &NoopPublisher,
        vec![word(19.0, 20.0, "Recent comparison source.")],
    );
    assert_eq!(window.early_drop.sources.segments.len(), 1);
    assert_eq!(window.carry.segments.len(), 2);
    let recent = window.carry.segments[1].evidence_parent.unwrap();
    publish_mic_words(
        &mut window,
        &NoopPublisher,
        vec![word(19.0, 20.0, "Recent comparison source.")],
    );
    let mic = window.mic_polls[0].evidence_parent.unwrap();
    release_held_mic_words(&mut window, &NoopPublisher);
    window.persist_evidence(&writer, None);
    let (document, early) = persisted_early_stage(&writer);
    assert!(document["ledger"]["truncation"]["cause"].is_null());
    assert_eq!(early["context"], serde_json::json!([recent, mic]));
    assert_eq!(window.early_drop.stats.released_published, 1);
    assert_eq!(window.early_drop.stats.released_dropped, 0);
    std::fs::remove_dir_all(&writer.filer.dir).unwrap();
}

#[test]
fn actual_live_early_echo_context_survives_post_assessment_pruning() {
    let (mut window, writer) = evidence_window("evidence-early-release-context", false);
    window.frames = 16_000;
    publish_them_words(
        &mut window,
        &NoopPublisher,
        vec![word(0.0, 1.0, "Earlier comparison source.")],
    );
    publish_mic_words(
        &mut window,
        &NoopPublisher,
        vec![word(0.2, 0.3, "Retained original microphone words.")],
    );
    assert_eq!(window.early_drop.held.len(), 1);
    let earlier = window.carry.segments[0].evidence_parent.unwrap();
    let mic = window.mic_polls[0].evidence_parent.unwrap();
    // offer_them assesses the held region against both sources, then prunes the old one before
    // emit_mic_words records anything. The handoff must keep the inventory from the assessment.
    window.frames = 20 * 16_000;
    publish_them_words(
        &mut window,
        &NoopPublisher,
        vec![word(19.0, 20.0, "Recent comparison source.")],
    );
    assert!(window.early_drop.held.is_empty());
    assert_eq!(window.early_drop.sources.segments.len(), 1);
    assert_eq!(window.carry.segments.len(), 2);
    let recent = window.carry.segments[1].evidence_parent.unwrap();
    window.persist_evidence(&writer, None);
    let (document, early) = persisted_early_stage(&writer);
    assert!(document["ledger"]["truncation"]["cause"].is_null());
    assert_eq!(early["context"], serde_json::json!([earlier, recent, mic]));
    assert_eq!(window.early_drop.stats.released_published, 1);
    std::fs::remove_dir_all(&writer.filer.dir).unwrap();
}

#[test]
fn actual_live_early_echo_immediate_publication_has_no_comparison_context() {
    let (mut window, mut writer) = evidence_window("evidence-early-bypass-context", false);
    publish_them_words(
        &mut window,
        &NoopPublisher,
        vec![word(0.0, 1.0, "Unconsulted comparison source.")],
    );
    let text = "This complete microphone sentence publishes immediately with its original words.";
    publish_mic_words(&mut window, &NoopPublisher, vec![word(2.0, 10.0, text)]);
    assert_eq!(window.early_drop.stats.held, 0);
    assert_eq!(window.mic_regions.len(), 1);
    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
    let (document, early) = persisted_early_stage(&writer);
    assert!(document["ledger"]["truncation"]["cause"].is_null());
    assert!(
        early
            .get("context")
            .is_none_or(|context| context.as_array().is_some_and(Vec::is_empty))
    );
    assert!(
        std::fs::read_to_string(writer.path().unwrap())
            .unwrap()
            .contains(text)
    );
    std::fs::remove_dir_all(&writer.filer.dir).unwrap();
}

#[test]
fn actual_live_early_echo_missing_lineage_and_cap_preserve_output() {
    assert_eq!(
        std::mem::size_of::<Option<Vec<corti_transcribe::evidence::ParentRow>>>(),
        std::mem::size_of::<Vec<corti_transcribe::evidence::ParentRow>>()
    );
    let cap = corti_transcribe::evidence::QUALITY_RESERVE
        + corti_transcribe::evidence::METADATA_RESERVE
        + 2048;
    for capped in [false, true] {
        let (mut window, mut writer) = evidence_window(
            if capped {
                "evidence-early-capped-context"
            } else {
                "evidence-early-missing-context"
            },
            false,
        );
        if capped {
            window.evidence.as_mut().unwrap().set_test_wire_cap(cap);
        }
        let source = if capped {
            "Synthetic comparison words. ".repeat(1024)
        } else {
            "Synthetic comparison words.".into()
        };
        if capped {
            // Quality and metadata reserves are mandatory. Exceed only the remaining text budget.
            assert!(
                source.len()
                    > cap
                        - corti_transcribe::evidence::QUALITY_RESERVE
                        - corti_transcribe::evidence::METADATA_RESERVE
            );
        }
        publish_them_words(&mut window, &NoopPublisher, vec![word(0.0, 1.0, &source)]);
        if !capped {
            // A missing parent must seal evidence even though another ring retains a matching row.
            window.early_drop.sources.segments[0].evidence_parent = None;
        }
        let text = "Retain the original correction of 43 centimeters.";
        let publisher = RecordingPublisher::default();
        publish_mic_words(&mut window, &publisher, vec![word(2.0, 3.0, text)]);
        release_held_mic_words(&mut window, &publisher);
        assert_eq!(publisher.0.borrow().len(), 1);
        assert_eq!(publisher.0.borrow()[0].1, vec![word(2.0, 3.0, text)]);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        let bytes = std::fs::read(crate::evidence::path_for(&writer.meta.audio_path)).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(!document["ledger"]["truncation"]["cause"].is_null());
        if capped {
            assert!(bytes.len() <= cap + 4096);
        }
        assert!(
            std::fs::read_to_string(writer.path().unwrap())
                .unwrap()
                .contains(text)
        );
        assert_eq!(window.early_drop.stats.released_dropped, 0);
        std::fs::remove_dir_all(&writer.filer.dir).unwrap();
    }
}

#[test]
fn actual_live_early_echo_split_poll_bounds_context_capacity_and_preserves_every_word() {
    let (mut window, mut writer) = evidence_window("evidence-early-context-budget", false);
    for index in 0..MAX_LIVE_ECHO_SOURCES {
        publish_them_words(
            &mut window,
            &NoopPublisher,
            vec![word(0.0, 1.0, &format!("Reference source number {index}."))],
        );
    }
    assert_eq!(
        window.early_drop.sources.segments.len(),
        MAX_LIVE_ECHO_SOURCES
    );
    let short_regions = 3 * MAX_HELD_MIC_REGIONS;
    let mut words: Vec<_> = (0..short_regions)
        .map(|index| {
            let start = 10.0 + index as f64 * (SEGMENT_GAP + 1.0);
            word(
                start,
                start + 0.2,
                &format!("Microphone original number {index}."),
            )
        })
        .collect();
    let start = 10.0 + short_regions as f64 * (SEGMENT_GAP + 1.0);
    words.push(word(
        start,
        start + 10.0,
        "This final complete sentence drains all prior holds before immediate publication.",
    ));
    let poll = window.remember_mic_poll(&words);
    assert!(!poll.omitted());
    let released = window
        .early_drop
        .offer_mic_with_poll(words.clone(), 0.0, None, poll);
    assert!(window.early_drop.is_idle());
    assert_eq!(released.len(), words.len());
    assert!(released.len() > MAX_HELD_MIC_REGIONS);
    let retained: Vec<_> = released
        .iter()
        .filter_map(|region| region.evidence_parents.as_ref())
        .collect();
    assert!(retained.len() > MAX_HELD_MIC_REGIONS);
    assert!(
        released
            .iter()
            .any(|region| region.evidence_parents.is_none())
    );
    assert!(
        retained
            .iter()
            .all(|parents| parents.len() <= MAX_LIVE_ECHO_SOURCES + 1)
    );
    assert!(
        retained
            .iter()
            .map(|parents| {
                parents.capacity() * std::mem::size_of::<corti_transcribe::evidence::ParentRow>()
            })
            .sum::<usize>()
            <= MAX_BUFFERED_TRANSCRIPT_BYTES
    );
    assert!(
        released
            .last()
            .unwrap()
            .evidence_parents
            .as_ref()
            .unwrap()
            .is_empty()
    );
    let publisher = RecordingPublisher::default();
    emit_mic_words(&mut window, &publisher, released);
    assert_eq!(
        publisher
            .0
            .borrow()
            .iter()
            .flat_map(|(_, published)| published.iter().cloned())
            .collect::<Vec<_>>(),
        words
    );
    assert_eq!(window.early_drop.stats.released_dropped, 0);
    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
    let document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(crate::evidence::path_for(&writer.meta.audio_path)).unwrap(),
    )
    .unwrap();
    assert!(!document["ledger"]["truncation"]["cause"].is_null());
    let note = std::fs::read_to_string(writer.path().unwrap()).unwrap();
    for original in words {
        assert!(note.contains(&original.text));
    }
    std::fs::remove_dir_all(&writer.filer.dir).unwrap();
}

#[test]
fn actual_live_discard_and_post_panic_reaper_remove_optional_text_and_temps() {
    for reaper in [false, true] {
        let (mut window, mut writer) = evidence_window(
            if reaper {
                "evidence-discard-reaper"
            } else {
                "evidence-discard-writer"
            },
            false,
        );
        publish_mic_words(
            &mut window,
            &NoopPublisher,
            vec![word(1.0, 2.0, "Synthetic discarded words.")],
        );
        release_held_mic_words(&mut window, &NoopPublisher);
        flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
        let evidence = crate::evidence::path_for(&writer.meta.audio_path);
        let temporary = PathBuf::from(format!("{}.tmp-1-2", evidence.display()));
        std::fs::write(&temporary, b"synthetic interrupted evidence").unwrap();
        if reaper {
            let (pipe_tx, _) = std::sync::mpsc::channel();
            let handle =
                std::thread::spawn(|| -> LiveOutcome { panic!("synthetic worker failure") });
            reap_discard_work(DiscardWork {
                id: corti_queue::job_id(&writer.meta),
                handle,
                reporter: Some(DiscardReporter {
                    meta: writer.meta.clone(),
                    pipe_tx,
                }),
            });
        } else {
            writer.discard().unwrap();
        }
        assert!(!evidence.exists());
        assert!(!temporary.exists());
        assert!(
            writer.meta.audio_path.exists(),
            "capture retains audio deletion ownership"
        );
        std::fs::remove_dir_all(&writer.filer.dir).unwrap();
    }
}

#[test]
fn actual_live_selected_and_fallback_prose_precede_note_rewrite_crash() {
    struct Publisher(crate::postprocess_app::HostedHandle);
    impl TranscriptPublisher for Publisher {
        fn words(&self, _: Speaker, _: &[Word]) {}
        fn hosted(&self) -> Option<&crate::postprocess_app::HostedHandle> {
            Some(&self.0)
        }
        fn recording_id(&self) -> Option<&str> {
            Some("synthetic-hosted-crash")
        }
    }
    for mode in [1, 5] {
        crate::postprocess_app::evidence_hosted_mode(mode);
        let (mut window, mut writer) =
            evidence_window(&format!("evidence-final-crash-{mode}"), false);
        publish_mic_words(
            &mut window,
            &NoopPublisher,
            vec![word(1.0, 2.0, "Um, Original synthetic statement.")],
        );
        release_held_mic_words(&mut window, &NoopPublisher);
        crate::evidence_scheduling::crash_after_rewrite();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            finish_session(
                16_000,
                None,
                Scripted::new(vec![], vec![]),
                Scripted::new(vec![], vec![]),
                NoDiarizer,
                window,
                FinishQuality {
                    dropped_chunks: 0,
                    capture: Some(corti_core::QualityRecord::observed()),
                },
                &mut writer,
                &Publisher(crate::postprocess_app::HostedHandle),
            )
        }));
        assert!(
            result.is_err(),
            "crash after the actual durable rewrite must fire"
        );
        let note = std::fs::read_to_string(writer.path().unwrap()).unwrap();
        let bytes = std::fs::read(crate::evidence::path_for(&writer.meta.audio_path)).unwrap();
        let document: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let stages: Vec<_> = document["ledger"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|group| group["stages"].as_array().unwrap())
            .collect();
        let final_stage = stages.last().unwrap();
        assert_eq!(final_stage["kind"]["kind"], "final_prose");
        let provider = stages
            .iter()
            .find(|stage| stage["id"] == final_stage["input"])
            .unwrap();
        assert_eq!(
            provider["kind"]["kind"], "provider_input",
            "selected fallback must follow this actual attempted handoff"
        );
        let selected = if mode == 1 {
            "Selected synthetic hosted prose."
        } else {
            "Original synthetic statement."
        };
        assert!(note.contains(selected));
        // Only rows the provider actually returned are provider text; the handoff input and a
        // declined (local) final prose keep their live lineage.
        let source = |stage: &serde_json::Value| {
            document["ledger"]["groups"]
                .as_array()
                .unwrap()
                .iter()
                .find(|group| group["stages"].as_array().unwrap().contains(stage))
                .unwrap()["signal"]["source"]
                .clone()
        };
        assert_eq!(source(provider), "live_stream");
        assert_eq!(
            source(final_stage),
            if mode == 1 { "provider_text" } else { "live_stream" }
        );
        assert!(String::from_utf8(bytes).unwrap().contains(selected));
        assert!(document["ledger"]["truncation"]["cause"].is_null());
        std::fs::remove_dir_all(&writer.filer.dir).unwrap();
        crate::postprocess_app::evidence_hosted_mode(0);
    }
}

pub(crate) fn sentinel_live_flush_and_invalid_readback() {
    let (mut window, mut writer) = evidence_window("evidence-live-sentinel", false);
    writer.meta.owning_app.name = "invented-private-participant".into();
    std::fs::write(&writer.meta.audio_path, b"invented-audio-content-violet").unwrap();
    publish_mic_words(
        &mut window,
        &NoopPublisher,
        vec![word(
            1.234,
            2.987,
            "Um, Very very syntheticquartzfalcon important.",
        )],
    );
    release_held_mic_words(&mut window, &NoopPublisher);
    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
    let path = crate::evidence::path_for(&writer.meta.audio_path);
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let receipt = serde_json::from_value(document["receipt"].clone()).unwrap();
    document["identity"]["version"] =
        serde_json::json!({"syntheticquartzfalcon":"invalid private identity"});
    crate::private_file::atomic_write_private(
        &path,
        &serde_json::to_vec(&document).unwrap(),
        "synthetic invalid evidence",
    )
    .unwrap();
    let result = crate::evidence::Session::load(
        &corti_queue::job_id(&writer.meta),
        &writer.meta,
        &writer.meta.audio_path,
        crate::evidence::Status::Available { receipt },
    );
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("invalid identity unexpectedly accepted"),
    };
    crate::evidence_tracing::assert_private(&format!("{error:?}"));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    publish_mic_words(
        &mut window,
        &NoopPublisher,
        vec![word(61.0, 62.0, "syntheticquartzfalcon second window.")],
    );
    release_held_mic_words(&mut window, &NoopPublisher);
    flush_window(&NoDiarizer, &mut window, &mut writer).unwrap();
    std::fs::remove_dir_all(&writer.filer.dir).unwrap();
}
