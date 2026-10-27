#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{AppConfig, BackendChoice},
        transcribe::{FirstAsrBackend, RetryOrigin},
    };
    use chrono::Utc;
    use corti_core::DiarizedTranscript;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    #[test]
    fn full_production_chain_counts_backend_and_replays_checkpoint_without_audio() {
        for (observed, selected) in [
            (true, BackendChoice::Local),
            (true, BackendChoice::Aws),
            (false, BackendChoice::Local),
            (false, BackendChoice::Aws),
        ] {
            let dir = std::env::temp_dir().join(format!(
                "decode163-full-{observed}-{:?}-{}",
                selected,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let audio = dir.join("recording.wav");
            let note = dir.join("live.md");
            let database = dir.join("queue.db");
            let meta = RecordingMeta {
                started_at: chrono::Local::now(),
                ended_at: None,
                owning_app: corti_core::OwningApp::unknown(),
                audio_path: audio.clone(),
            };
            let queue = Queue::open_at(&database).unwrap();
            let id = queue.enqueue(&meta).unwrap();
            std::fs::write(
                &note,
                format!(
                    "---\nsource: synthetic\n---\n\n# Synthetic\n\n{}\nSynthetic live prefix.\n",
                    corti_vagus::live_initial_body(&meta)
                ),
            )
            .unwrap();
            let inode = std::fs::metadata(&note).unwrap().ino();
            let mut capture = corti_core::QualityRecord::observed();
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
            if !observed {
                capture = corti_core::QualityRecord::default();
            }
            let mut store = observed.then(|| {
                corti_core::quality_store::QualityStore::begin(&audio, &id, &capture).unwrap()
            });
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
            for _ in 0..64 {
                wav.write_sample(0i16).unwrap();
            }
            if let Some(store) = &mut store {
                store.prepare_finalization(&capture, 32).unwrap();
            }
            wav.finalize().unwrap();
            if let Some(store) = &mut store {
                store.complete_after_audio_sync(32).unwrap();
            }
            drop(store);
            let mut live = capture.clone();
            corti_core::insert_retained_processing_loss(
                &mut live,
                16,
                32,
                corti_core::QualityTrack::Mic,
                corti_core::QualityCause::DecodeFailure,
                corti_core::QualityStage::Live,
            )
            .unwrap();
            let payload = crate::jobs::with_settled_quality(
                crate::jobs::retry_payload_with_origin(
                    &id,
                    Some(&OwnedNote::partial(note.clone())),
                    Some(FirstAsrBackend::Local),
                ),
                Some(&live),
            )
            .unwrap();
            queue
                .jobs()
                .enqueue_recording(crate::jobs::RETRY_TRANSCRIPTION, &payload, 6, Utc::now())
                .unwrap();
            let control = rusqlite::Connection::open(&database).unwrap();
            control.execute_batch("CREATE TRIGGER reject_done BEFORE UPDATE ON recordings WHEN NEW.status='done' BEGIN SELECT RAISE(ABORT,'synthetic completion failure'); END;").unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let counted = calls.clone();
            let cfg = AppConfig {
                transcribe_backend: selected,
                aec_enabled: false,
                ..Default::default()
            };
            let backend = Backend::scripted(
                cfg.clone(),
                Box::new(move |_, _, quality| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let mut quality = quality.clone();
                    corti_core::insert_retained_processing_loss(
                        &mut quality,
                        16,
                        24,
                        corti_core::QualityTrack::Mic,
                        corti_core::QualityCause::DecodeFailure,
                        corti_core::QualityStage::Batch,
                    )?;
                    Ok(corti_transcribe::TranscriptionOutcome {
                        transcript: DiarizedTranscript::new(vec![corti_core::TranscriptSegment {
                            speaker: corti_core::Speaker::Me,
                            start: 0.0,
                            end: 0.0005,
                            text: "Synthetic surviving batch output.".into(),
                        }]),
                        quality,
                        decode_complete: false,
                    })
                }),
            );
            let mut ctx = Ctx {
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
            let error = transcribe_and_file(
                &mut ctx,
                &id,
                &meta,
                &audio,
                Some(&OwnedNote::partial(note.clone())),
                RetryOrigin::Retry(Some(FirstAsrBackend::Local)),
            )
            .unwrap_err();
            assert!(
                format!("{error:#}").contains("synthetic completion failure"),
                "fallback must reach the injected completion boundary: {error:#}"
            );
            let expected = usize::from(selected == BackendChoice::Local);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected,
                "full production ASR invocation count"
            );
            let checkpoint = FilingCheckpoint::load(&audio).unwrap();
            assert!(
                checkpoint
                    .quality
                    .as_ref()
                    .unwrap()
                    .requires_batch_recovery()
            );
            assert_eq!(std::fs::metadata(&note).unwrap().ino(), inode);
            let body = std::fs::read_to_string(&note).unwrap();
            assert!(
                body.contains("Transcript coverage is partial"),
                "the filed transcript must disclose the remaining processing loss"
            );
            if selected == BackendChoice::Local {
                assert_eq!(body.matches("Synthetic surviving batch output.").count(), 1);
                assert!(!body.contains("Synthetic live prefix."));
                assert!(
                    !observed
                        || checkpoint
                            .quality
                            .as_ref()
                            .unwrap()
                            .spans
                            .iter()
                            .any(|s| s.stage == corti_core::QualityStage::Batch
                                && s.resolution == corti_core::QualityResolution::Outstanding)
                );
                if !observed {
                    assert!(
                        checkpoint
                            .quality
                            .as_ref()
                            .unwrap()
                            .overflow
                            .outstanding_omitted_spans[1]
                            > 0,
                        "legacy batch failure stays explicit without invented source coordinates"
                    );
                }
            }
            std::fs::remove_file(&audio).unwrap();
            control.execute_batch("DROP TRIGGER reject_done;").unwrap();
            assert!(
                transcribe_and_file(&mut ctx, &id, &meta, &audio, None, RetryOrigin::Retry(None))
                    .is_ok()
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                expected,
                "checkpoint replay never calls ASR despite missing audio and unknown retry origin"
            );
            assert_eq!(ctx.queue.get(&id).unwrap().unwrap().status, JobStatus::Done);
            assert!(
                !checkpoint_path(&audio).exists(),
                "successful completion removes its checkpoint"
            );
            let reloaded = crate::transcribe::load_recording_quality(&id, &audio, None).unwrap();
            assert!(
                reloaded.quality.requires_batch_recovery(),
                "Done cleanup cannot erase partial decode history"
            );
            if !observed {
                assert_eq!(
                    reloaded.storage_status,
                    crate::transcribe::RecordingStorageStatus::LegacyUnknown
                );
                assert_eq!(
                    reloaded.quality.coverage,
                    corti_core::QualityCoverage::LegacyUnknown
                );
                assert!(
                    reloaded.quality.epochs.is_empty(),
                    "history does not invent capture authority"
                );
                assert!(!reloaded.storage_complete());
            }
            drop(ctx);
            drop(control);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
