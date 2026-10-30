// Included inside the existing actual-pipeline fixture module. ASR is scripted; queue, checkpoint,
// cleanup, quality storage, evidence publication, same-inode note rewrite, and retry dispatch are real.

fn evidence_fixture(name: &str) -> Fixture {
    let mut fixture = fixture(name, 0);
    let counted = fixture.calls.clone();
    let config = fixture.ctx.config.lock().unwrap().clone();
    fixture.ctx.backend = Backend::scripted(
        config,
        Box::new(move |_, _, quality| {
            let generation = counted.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(corti_transcribe::TranscriptionOutcome {
                transcript: corti_core::DiarizedTranscript::new(vec![
                    corti_core::TranscriptSegment {
                        speaker: corti_core::Speaker::Me,
                        start: 1.234,
                        end: 2.987,
                        text: format!(
                            "Um, Very very syntheticquartzfalcon generation {generation} important."
                        ),
                    },
                ]),
                quality: quality.clone(),
                decode_complete: true,
            })
        }),
    );
    fixture
}

fn run_evidence_batch(fixture: &mut Fixture) -> Result<()> {
    transcribe_and_file(
        &mut fixture.ctx,
        &fixture.id,
        &fixture.meta,
        &fixture.meta.audio_path,
        Some(&OwnedNote::partial(fixture.note.clone())),
        crate::transcribe::RetryOrigin::Retry(Some(crate::transcribe::FirstAsrBackend::Local)),
    )
}

#[test]
fn actual_batch_optout_cannot_complete_between_enabled_check_and_publication() {
    let mut fixture = evidence_fixture("evidence-publication-optout");
    let config = fixture.ctx.config.clone();
    let audio = fixture.meta.audio_path.clone();
    let evidence_path = crate::evidence::path_for(&audio);
    std::fs::write(&evidence_path, b"synthetic previous evidence").unwrap();
    let toggled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callback_toggled = toggled.clone();
    let callback_entered = entered.clone();
    crate::evidence_scheduling::before_persist(Box::new(move || {
        callback_entered.store(true, Ordering::SeqCst);
        let mut settings = match config.try_lock() {
            Ok(settings) => settings,
            Err(std::sync::TryLockError::WouldBlock) => return,
            Err(std::sync::TryLockError::Poisoned(_)) => panic!("settings lock poisoned"),
        };
        settings.evidence.enabled = false;
        callback_toggled.store(true, Ordering::SeqCst);
        drop(settings);
        crate::evidence::remove(&audio).unwrap();
        assert!(!crate::evidence::path_for(&audio).exists());
    }));
    run_evidence_batch(&mut fixture).unwrap();
    assert!(
        entered.load(Ordering::SeqCst),
        "actual publication seam must execute"
    );
    assert_eq!(
        fixture.ctx.queue.get(&fixture.id).unwrap().unwrap().status,
        JobStatus::Done
    );
    let recreated = toggled.load(Ordering::SeqCst) && evidence_path.exists();
    // A fenced toggle completes after publication; the actual opt-out sweep then remains final.
    fixture.ctx.config.lock().unwrap().evidence.enabled = false;
    crate::evidence_retention::optout_sweep(&fixture.ctx.queue).unwrap();
    assert!(!evidence_path.exists());
    assert!(fixture.meta.audio_path.exists());
    assert!(
        corti_core::quality_store::quality_sidecar_path(&fixture.meta.audio_path)
            .unwrap()
            .exists()
    );
    drop(fixture.ctx);
    std::fs::remove_dir_all(fixture.directory).unwrap();
    assert!(
        !recreated,
        "batch recreated evidence after opt-out completed before publication"
    );
    assert!(
        !toggled.load(Ordering::SeqCst),
        "publication must retain the shared settings guard"
    );
}

fn asr_stages(bytes: &[u8]) -> Vec<serde_json::Value> {
    let document: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    document["ledger"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|group| group["stages"].as_array().unwrap())
        .filter(|stage| {
            matches!(
                stage["kind"]["kind"].as_str(),
                Some("true_asr" | "fallback_asr")
            )
        })
        .cloned()
        .collect()
}

#[test]
fn actual_batch_pending_note_reuses_byte_identical_asr_without_backend_or_audio() {
    let mut fixture = evidence_fixture("evidence-after-checkpoint");
    let control = rusqlite::Connection::open(fixture.directory.join("queue.db")).unwrap();
    control.execute_batch("CREATE TRIGGER reject_done BEFORE UPDATE ON recordings WHEN NEW.status='done' BEGIN SELECT RAISE(ABORT,'synthetic completion failure'); END;").unwrap();
    assert!(run_evidence_batch(&mut fixture).is_err());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let path = crate::evidence::path_for(&fixture.meta.audio_path);
    let before = std::fs::read(&path).unwrap();
    assert_eq!(asr_stages(&before).len(), 1);
    let checkpoint = FilingCheckpoint::load(&fixture.meta.audio_path).unwrap();
    assert!(matches!(
        checkpoint.evidence,
        Some(crate::evidence::Status::Available { .. })
    ));
    assert_eq!(
        fixture.ctx.queue.get(&fixture.id).unwrap().unwrap().status,
        JobStatus::PendingNote
    );
    let note = std::fs::read_to_string(&fixture.note).unwrap();
    assert!(
        !note.contains("Um, Very very"),
        "canonical note must not embed evidence preimages"
    );
    assert!(!note.contains("\"groups\""));
    assert!(!note.contains("fingerprint_kind"));
    std::fs::remove_file(&fixture.meta.audio_path).unwrap();
    control.execute_batch("DROP TRIGGER reject_done;").unwrap();
    run_evidence_batch(&mut fixture).unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(!checkpoint_path(&fixture.meta.audio_path).exists());
    assert_eq!(
        fixture.ctx.queue.get(&fixture.id).unwrap().unwrap().status,
        JobStatus::Done
    );
    drop(fixture.ctx);
    std::fs::remove_dir_all(fixture.directory).unwrap();
}

#[test]
fn actual_batch_precheckpoint_crash_retries_and_atomically_replaces_one_asr_generation() {
    let mut fixture = evidence_fixture("evidence-before-checkpoint");
    let checkpoint = checkpoint_path(&fixture.meta.audio_path);
    std::fs::create_dir(&checkpoint).unwrap();
    assert!(run_evidence_batch(&mut fixture).is_err());
    let path = crate::evidence::path_for(&fixture.meta.audio_path);
    let first = std::fs::read(&path).unwrap();
    assert_eq!(asr_stages(&first).len(), 1);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    assert!(FilingCheckpoint::load(&fixture.meta.audio_path).is_err());
    std::fs::remove_dir(&checkpoint).unwrap();
    run_evidence_batch(&mut fixture).unwrap();
    let second = std::fs::read(&path).unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    assert_eq!(asr_stages(&second).len(), 1);
    let first: serde_json::Value = serde_json::from_slice(&first).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&second).unwrap();
    assert_ne!(
        first["receipt"]["generation"],
        second["receipt"]["generation"]
    );
    let current = serde_json::to_string(&second["ledger"]).unwrap();
    assert!(current.contains("generation 2"));
    assert!(!current.contains("generation 1"));
    assert!(
        crate::checkpoint::temporary_paths(&fixture.meta.audio_path)
            .unwrap()
            .is_empty()
    );
    drop(fixture.ctx);
    std::fs::remove_dir_all(fixture.directory).unwrap();
}

#[test]
fn actual_batch_optout_and_evidence_write_failure_never_block_filing() {
    for disabled in [true, false] {
        let mut fixture = evidence_fixture(if disabled {
            "evidence-disabled"
        } else {
            "evidence-write-failure"
        });
        fixture.ctx.config.lock().unwrap().evidence.enabled = !disabled;
        let path = crate::evidence::path_for(&fixture.meta.audio_path);
        if !disabled {
            std::fs::create_dir(&path).unwrap();
        }
        run_evidence_batch(&mut fixture).unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.ctx.queue.get(&fixture.id).unwrap().unwrap().status,
            JobStatus::Done
        );
        assert!(!path.is_file());
        assert!(fixture.note.is_file());
        assert!(fixture.meta.audio_path.is_file());
        assert!(
            corti_core::quality_store::quality_sidecar_path(&fixture.meta.audio_path)
                .unwrap()
                .is_file()
        );
        assert!(
            crate::checkpoint::temporary_paths(&fixture.meta.audio_path)
                .unwrap()
                .is_empty()
        );
        drop(fixture.ctx);
        std::fs::remove_dir_all(fixture.directory).unwrap();
    }
}

#[test]
fn actual_sweep_removes_opted_out_evidence_before_aws_job_enqueue_failure() {
    let mut fixture = evidence_fixture("evidence-optout-aws-enqueue");
    let audio = &fixture.meta.audio_path;
    let evidence = crate::evidence::path_for(audio);
    let temp = PathBuf::from(format!("{}.tmp-3-4", evidence.display()));
    for path in [&evidence, &temp] {
        std::fs::write(path, b"synthetic evidence").unwrap();
    }
    crate::checkpoint::AwsStaging {
        bucket: "synthetic".into(),
        key_prefix: "corti/".into(),
        job_name: fixture.id.clone(),
        region: None,
    }
    .store(audio)
    .unwrap();
    fixture
        .ctx
        .queue
        .set_status(&fixture.id, JobStatus::Failed)
        .unwrap();
    fixture.ctx.config.lock().unwrap().evidence.enabled = false;
    let control = rusqlite::Connection::open(fixture.directory.join("queue.db")).unwrap();
    control.execute_batch("CREATE TRIGGER reject_aws_enqueue BEFORE INSERT ON jobs WHEN NEW.kind='cleanup_aws_staging' BEGIN SELECT RAISE(ABORT,'synthetic AWS enqueue failure'); END;").unwrap();
    let result = crate::evidence_retention::run_sweep(&mut fixture.ctx);
    assert!(result.is_err(), "the real AWS enqueue fault must fire");
    assert!(
        !evidence.exists(),
        "opt-out must run before unrelated AWS enqueue"
    );
    assert!(!temp.exists());
    assert!(audio.exists());
    assert!(crate::checkpoint::aws_staging_path_for(audio).exists());
    assert!(
        corti_core::quality_store::quality_sidecar_path(audio)
            .unwrap()
            .exists()
    );
    assert!(fixture.ctx.queue.get(&fixture.id).unwrap().is_some());
    drop(fixture.ctx);
    std::fs::remove_dir_all(fixture.directory).unwrap();
}

#[test]
fn actual_batch_live_and_persistence_diagnostics_omit_content_under_failures() {
    let (dispatch, captured) = crate::evidence_tracing::subscriber();
    let output = tracing::dispatcher::with_default(&dispatch, || {
        std::panic::catch_unwind(|| {
            let mut fixture = evidence_fixture("evidence-tracing-sentinel");
            fixture.meta.owning_app.name = "invented-private-participant".into();
            let mut wav = hound::WavWriter::create(
                &fixture.meta.audio_path,
                hound::WavSpec {
                    channels: 2,
                    sample_rate: 16_000,
                    bits_per_sample: 16,
                    sample_format: hound::SampleFormat::Int,
                },
            )
            .unwrap();
            let mut payload = [0_u8; 64];
            let marker = b"invented-audio-content-violet";
            payload[..marker.len()].copy_from_slice(marker);
            for pair in payload.as_chunks::<2>().0 {
                wav.write_sample(i16::from_le_bytes(*pair)).unwrap();
            }
            wav.finalize().unwrap();
            let path = crate::evidence::path_for(&fixture.meta.audio_path);
            std::fs::create_dir(&path).unwrap();
            // Actual backend, cleanup and optional publication failure must still finish the note.
            run_evidence_batch(&mut fixture).unwrap();
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
            drop(fixture.ctx);
            std::fs::remove_dir_all(fixture.directory).unwrap();
        })
    });
    assert!(
        output.is_ok(),
        "actual batch privacy path unexpectedly panicked"
    );
    // Live has a real dedicated worker. Propagate the capture subscriber across this thread boundary.
    let live = std::thread::spawn(move || {
        tracing::dispatcher::with_default(&dispatch, || {
            std::panic::catch_unwind(crate::live::tests::sentinel_live_flush_and_invalid_readback)
        })
    })
    .join()
    .unwrap();
    assert!(
        live.is_ok(),
        "actual live privacy path unexpectedly panicked"
    );
    let logs = captured.lock().unwrap();
    assert!(logs.contains("segment cleanup applied"));
    assert!(logs.contains("durable live transcript chunk synced"));
    assert!(logs.contains("local evidence unavailable"));
    crate::evidence_tracing::assert_private(&logs);
}

#[test]
fn actual_pending_note_survives_optional_corruption_and_optout_without_another_asr() {
    for disabled in [false, true] {
        let mut fixture = evidence_fixture(if disabled {
            "evidence-resume-optout"
        } else {
            "evidence-resume-corrupt"
        });
        let control = rusqlite::Connection::open(fixture.directory.join("queue.db")).unwrap();
        control.execute_batch("CREATE TRIGGER reject_done BEFORE UPDATE ON recordings WHEN NEW.status='done' BEGIN SELECT RAISE(ABORT,'synthetic completion failure'); END;").unwrap();
        assert!(run_evidence_batch(&mut fixture).is_err());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        let path = crate::evidence::path_for(&fixture.meta.audio_path);
        if disabled {
            fixture.ctx.config.lock().unwrap().evidence.enabled = false;
            crate::evidence::remove(&fixture.meta.audio_path).unwrap();
        } else {
            std::fs::write(&path, b"syntheticquartzfalcon invalid evidence").unwrap();
        }
        control.execute_batch("DROP TRIGGER reject_done;").unwrap();
        std::fs::remove_file(&fixture.meta.audio_path).unwrap();
        run_evidence_batch(&mut fixture).unwrap();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.ctx.queue.get(&fixture.id).unwrap().unwrap().status,
            JobStatus::Done
        );
        assert_eq!(path.exists(), !disabled);
        assert!(
            !std::fs::read_to_string(&fixture.note)
                .unwrap()
                .contains("invalid evidence")
        );
        drop(fixture.ctx);
        std::fs::remove_dir_all(fixture.directory).unwrap();
    }
}

#[test]
fn actual_readonly_transcription_and_export_exclude_evidence_and_clean_redo_records_processed_input()
 {
    let fixture = evidence_fixture("evidence-readonly-export");
    let mut quality = fixture.capture.clone();
    let (decoded, _, _, _) = crate::transcribe::transcribe_recording(
        &fixture.ctx.backend,
        None,
        crate::transcribe::TranscriptionAttempt::fresh(&fixture.id).with_quality(&mut quality),
        &fixture.meta,
        &fixture.meta.audio_path,
    )
    .unwrap();
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    let path = crate::evidence::path_for(&fixture.meta.audio_path);
    assert!(!path.exists());
    let cfg = fixture.ctx.config.lock().unwrap().clone();
    let provenance =
        crate::provenance::from_config(&cfg, corti_vagus::provenance::GenerationMode::Batch);
    let body =
        crate::provenance::quality_body(&fixture.meta, &decoded.transcript, Some(&quality), None);
    let export = crate::evidence_export::render(&body, &provenance);
    assert!(!export.contains("Um, Very very"));
    assert!(!export.contains("fingerprint_kind"));
    assert!(!export.contains("\"groups\""));
    let mut run = None;
    let request = crate::evidence::integration::BatchRequest {
        output: &mut run,
        capture: None,
        already_clean: true,
        kind: corti_transcribe::evidence::StageKind::FallbackAsr,
    };
    let (decoded, _, _, _) = crate::transcribe::transcribe_recording_with_evidence(
        &fixture.ctx.backend,
        None,
        crate::transcribe::TranscriptionAttempt::fresh(&fixture.id).with_quality(&mut quality),
        &fixture.meta,
        &fixture.meta.audio_path,
        Some(request),
    )
    .unwrap();
    let mut run = run.unwrap();
    run.final_prose(&decoded.transcript, false);
    assert!(matches!(
        run.persist(&fixture.meta.audio_path),
        crate::evidence::Status::Available { .. }
    ));
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(
        document["identity"]["configuration"]["aec"]["mode"],
        "preprocessed_unknown"
    );
    assert_eq!(
        document["ledger"]["groups"][0]["signal"]["source"],
        "retained_processed_wav"
    );
    assert_eq!(
        document["ledger"]["groups"][0]["stages"][0]["kind"]["kind"],
        "fallback_asr"
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    drop(fixture.ctx);
    std::fs::remove_dir_all(fixture.directory).unwrap();
}
