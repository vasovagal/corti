#[cfg(test)]
mod retention_tests {
    use super::*;
    use crate::config::{AppConfig, BackendChoice};
    use crate::live::LiveOutcome;
    use corti_core::{QualityClass, QualityRecord, QualityResolution};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct Fixture {
        directory: PathBuf,
        id: String,
        meta: RecordingMeta,
        note: PathBuf,
        capture: QualityRecord,
        calls: Arc<AtomicUsize>,
        ctx: Ctx,
    }

    fn fixture(label: &str, capture_omissions: u64) -> Fixture {
        let directory =
            std::env::temp_dir().join(format!("decode-retention-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let audio = directory.join("recording.wav");
        let note = directory.join("live.md");
        let meta = RecordingMeta {
            audio_path: audio.clone(),
            started_at: chrono::Local::now(),
            ended_at: None,
            owning_app: corti_core::OwningApp::unknown(),
        };
        std::fs::write(
            &note,
            format!(
                "---\nsource: synthetic\n---\n\n# Synthetic\n\n{}",
                corti_vagus::live_initial_body(&meta)
            ),
        )
        .unwrap();
        let queue = Queue::open_at(directory.join("queue.db")).unwrap();
        let id = queue.enqueue(&meta).unwrap();
        let mut capture = QualityRecord::observed();
        capture
            .insert_epoch(corti_core::CaptureEpoch {
                id: 0,
                source_start_ns: Some(0),
                source_rate_hz: 16_000,
                source_frame_base: 0,
                retained_start_frame: 0,
                retained_rate_hz: 16_000,
                mic_channels: 1,
                tap_channels: 1,
                reason: corti_core::CaptureEpochReason::Start,
            })
            .unwrap();
        capture
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                capture_omissions,
            )
            .unwrap();
        let mut store =
            corti_core::quality_store::QualityStore::begin(&audio, &id, &capture).unwrap();
        let mut wav = hound::WavWriter::create(
            &audio,
            hound::WavSpec {
                channels: 2,
                sample_rate: 16_000,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..32 {
            wav.write_sample(0_i16).unwrap();
        }
        store.prepare_finalization(&capture, 16).unwrap();
        wav.finalize().unwrap();
        store.complete_after_audio_sync(16).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let cfg = AppConfig {
            transcribe_backend: BackendChoice::Local,
            aec_enabled: false,
            ..Default::default()
        };
        let backend = Backend::scripted(
            cfg.clone(),
            Box::new(move |_, _, quality| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(corti_transcribe::TranscriptionOutcome {
                    transcript: corti_core::DiarizedTranscript::default(),
                    quality: quality.clone(),
                    decode_complete: true,
                })
            }),
        );
        let ctx = Ctx {
            queue,
            vagus: Err("unused synthetic filer".into()),
            backend,
            app: (),
            stats: crate::stats::StatsBuffer,
            backend_label: "synthetic",
            config: Arc::new(Mutex::new(cfg)),
            live: Arc::new(crate::live::LiveManager),
            hosted: crate::postprocess_app::HostedHandle,
        };
        Fixture {
            directory,
            id,
            meta,
            note,
            capture,
            calls,
            ctx,
        }
    }

    fn with_metadata(capture: &QualityRecord) -> QualityRecord {
        let mut quality = capture.clone();
        quality
            .record_omitted_spans(
                QualityClass::DegradedOnly,
                QualityResolution::Outstanding,
                1,
            )
            .unwrap();
        quality
    }

    fn assert_retained(fixture: &Fixture, expected: &QualityRecord) {
        let loaded =
            crate::transcribe::load_recording_quality(&fixture.id, &fixture.meta.audio_path, None)
                .unwrap();
        assert_eq!(&loaded.quality, expected);
        assert!(loaded.storage_complete());
        assert!(!loaded.quality.requires_batch_recovery());
        let stored = corti_core::quality_store::load_quality(
            &fixture.meta.audio_path,
            &fixture.id,
            Some(16),
        )
        .unwrap();
        assert_eq!(
            stored.snapshot.unwrap().quality(),
            &fixture.capture,
            "immutable capture authority is unchanged"
        );
    }

    #[test]
    fn successful_live_quality_survives_completion_replay_and_publication_failure() {
        for fault in [
            "clean",
            "none",
            "sidecar",
            "publication",
            "publication_and_checkpoint",
        ] {
            let mut fixture = fixture(fault, if fault == "clean" { 0 } else { 2 });
            let live = if fault == "clean" {
                fixture.capture.clone()
            } else {
                with_metadata(&fixture.capture)
            };
            let audio = fixture.meta.audio_path.clone();
            let authority = corti_core::quality_store::quality_sidecar_path(&audio).unwrap();
            let pending =
                corti_core::quality_store::quality_temporary_paths(&audio).unwrap()[0].clone();
            let original = std::fs::read(&authority).unwrap();
            if fault == "sidecar" {
                std::fs::write(&authority, b"synthetic invalid authority").unwrap();
            }
            if fault.starts_with("publication") {
                std::fs::write(&pending, b"synthetic interrupted publication").unwrap();
            }
            if fault == "publication_and_checkpoint" {
                std::fs::create_dir(checkpoint_path(&audio)).unwrap();
            }
            process_live_result(
                &mut fixture.ctx,
                &fixture.id,
                &fixture.meta,
                &audio,
                Some(LiveOutcome::Filed {
                    note_path: fixture.note.clone(),
                    settled_quality: (fault != "clean").then(|| live.clone()),
                }),
            );
            assert_eq!(
                fixture.calls.load(Ordering::SeqCst),
                0,
                "metadata retention never invokes ASR"
            );
            if !matches!(fault, "clean" | "none") {
                assert_ne!(
                    fixture.ctx.queue.get(&fixture.id).unwrap().unwrap().status,
                    JobStatus::Done
                );
                assert_eq!(
                    std::fs::read(&authority).unwrap(),
                    if fault == "sidecar" {
                        b"synthetic invalid authority".to_vec()
                    } else {
                        original.clone()
                    }
                );
                let payload = fixture
                    .ctx
                    .queue
                    .jobs()
                    .recording_payload(crate::jobs::RETRY_TRANSCRIPTION, &fixture.id)
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    corti_jobs::settled_quality(&payload).unwrap(),
                    Some(live.clone()),
                    "retry retains facts even if both publications fail"
                );
                assert_eq!(payload["note_canonical"], true);
                if fault != "publication_and_checkpoint" {
                    assert_eq!(
                        FilingCheckpoint::load(&audio).unwrap().quality,
                        Some(live.clone())
                    );
                } else {
                    std::fs::remove_dir(checkpoint_path(&audio)).unwrap();
                }
                if fault == "sidecar" {
                    std::fs::write(&authority, &original).unwrap();
                } else {
                    assert_eq!(
                        std::fs::read(&pending).unwrap(),
                        b"synthetic interrupted publication"
                    );
                    std::fs::remove_file(&pending).unwrap();
                }
                fixture.ctx.queue = Queue::open_at(fixture.directory.join("queue.db")).unwrap();
                let claimed = fixture
                    .ctx
                    .queue
                    .jobs()
                    .claim_due(Utc::now() + chrono::Duration::hours(1))
                    .unwrap()
                    .unwrap();
                complete_canonical_note(
                    &fixture.ctx,
                    &fixture.id,
                    &fixture.meta,
                    &audio,
                    &fixture.note,
                )
                .unwrap();
                fixture.ctx.queue.jobs().complete(&claimed).unwrap();
            }
            assert_eq!(
                fixture.ctx.queue.get(&fixture.id).unwrap().unwrap().status,
                JobStatus::Done
            );
            assert!(!checkpoint_path(&audio).exists());
            assert!(
                fixture
                    .ctx
                    .queue
                    .jobs()
                    .recording_payload(crate::jobs::RETRY_TRANSCRIPTION, &fixture.id)
                    .unwrap()
                    .is_none()
            );
            fixture.ctx.queue = Queue::open_at(fixture.directory.join("queue.db")).unwrap();
            assert_retained(&fixture, &live);
            if fault != "clean" {
                let mut newer =
                    crate::transcribe::load_recording_quality(&fixture.id, &audio, None).unwrap();
                newer
                    .quality
                    .record_omitted_spans(
                        QualityClass::DegradedOnly,
                        QualityResolution::Outstanding,
                        4,
                    )
                    .unwrap();
                crate::transcribe::persist_recording_quality(&fixture.id, &audio, &newer).unwrap();
                for _ in 0..2 {
                    process_live_result(
                        &mut fixture.ctx,
                        &fixture.id,
                        &fixture.meta,
                        &audio,
                        Some(LiveOutcome::Filed {
                            note_path: fixture.note.clone(),
                            settled_quality: Some(live.clone()),
                        }),
                    );
                    assert_retained(&fixture, &newer.quality);
                }
            }
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
            drop(fixture.ctx);
            std::fs::remove_dir_all(fixture.directory).unwrap();
        }
    }

    #[test]
    fn note_less_quality_uses_existing_batch_payload_without_fallback() {
        for metadata in [false, true] {
            let mut fixture = fixture(
                if metadata {
                    "silent-metadata"
                } else {
                    "silent-clean"
                },
                0,
            );
            let expected = if metadata {
                Some(with_metadata(&fixture.capture))
            } else {
                None
            };
            assert!(matches!(
                resolve_live(Some(LiveOutcome::NoNote {
                    settled_quality: expected.clone()
                })),
                LiveResolution::Batch { fallback: None, .. }
            ));
            let audio = fixture.meta.audio_path.clone();
            process_live_result(
                &mut fixture.ctx,
                &fixture.id,
                &fixture.meta,
                &audio,
                Some(LiveOutcome::NoNote {
                    settled_quality: expected.clone(),
                }),
            );
            assert_eq!(
                fixture.calls.load(Ordering::SeqCst),
                0,
                "normal Process only arms the existing batch job"
            );
            let payload = fixture
                .ctx
                .queue
                .jobs()
                .recording_payload(crate::jobs::RETRY_TRANSCRIPTION, &fixture.id)
                .unwrap()
                .unwrap();
            assert_eq!(corti_jobs::settled_quality(&payload).unwrap(), expected);
            assert_eq!(payload["capture_finished"], true);
            assert!(payload.get("first_batch").is_none());
            assert!(payload.get("preferred_note").is_none());
            let claimed = fixture
                .ctx
                .queue
                .jobs()
                .claim_due(Utc::now())
                .unwrap()
                .unwrap();
            assert_eq!(
                claimed.max_attempts, 6,
                "the ordinary initial live batch budget is unchanged"
            );
            let origin = crate::jobs::original_backend(&payload);
            assert_eq!(origin, Some(crate::transcribe::FirstAsrBackend::Local));
            // Give the ordinary batch filing path an already-owned synthetic note, keeping this
            // production-path test independent of a host vagus executable or external vault.
            fixture
                .ctx
                .queue
                .update(
                    &fixture.id,
                    JobUpdate {
                        note_path: Some(fixture.note.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
            transcribe_and_file(
                &mut fixture.ctx,
                &fixture.id,
                &fixture.meta,
                &audio,
                None,
                crate::transcribe::RetryOrigin::Retry(origin),
            )
            .unwrap();
            fixture.ctx.queue.jobs().complete(&claimed).unwrap();
            assert_eq!(
                fixture.calls.load(Ordering::SeqCst),
                1,
                "only the existing batch route invokes ASR"
            );
            assert_retained(&fixture, expected.as_ref().unwrap_or(&fixture.capture));
            assert!(!checkpoint_path(&audio).exists());
            assert!(
                fixture
                    .ctx
                    .queue
                    .jobs()
                    .recording_payload(crate::jobs::RETRY_TRANSCRIPTION, &fixture.id)
                    .unwrap()
                    .is_none()
            );
            drop(fixture.ctx);
            std::fs::remove_dir_all(fixture.directory).unwrap();
        }
    }
}
