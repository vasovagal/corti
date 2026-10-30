use super::*;
use corti_core::{OwningApp, Speaker, TranscriptSegment};
use corti_transcribe::evidence::{
    AecOrigin, CleanupInput, RecordingIdentity, SignalIdentity, SignalSource, StageKind,
};
use corti_transcribe::segment::CleanupConfig;
use std::os::unix::fs::PermissionsExt;

fn fixture(name: &str) -> (PathBuf, RecordingMeta) {
    let dir = std::env::temp_dir().join(format!("corti-evidence-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = RecordingMeta {
        started_at: chrono::Local::now(),
        ended_at: None,
        owning_app: OwningApp::unknown(),
        audio_path: dir.join("recording.wav"),
    };
    (dir, meta)
}

fn append(session: &mut Session, window: u64, text: &str) {
    let identity = session.identity();
    let actual = session.ledger_mut().cleanup(
        RecordingIdentity {
            window,
            kind: StageKind::TrueAsr,
            identity,
            signal: SignalIdentity {
                source: SignalSource::OfflineInput,
                aec: AecOrigin::None,
                source_rate_hz: Some(16_000),
                lookahead_frames: None,
            },
        },
        vec![TranscriptSegment {
            speaker: Speaker::Me,
            start: 1.234,
            end: 2.987,
            text: text.into(),
        }],
        &CleanupConfig::default(),
        CleanupInput {
            origins: &[],
            source_links: None,
            carry: &[],
            carry_links: None,
            protected_input_indices: &[],
        },
        None,
        None,
    );
    assert!(actual.evidence.is_ok());
}

#[test]
fn capture_optout_discovers_only_owned_names_and_preserves_unrelated_targets() {
    let (directory, meta) = fixture("capture-discovery");
    let stem = corti_capture::recording_stem(&meta.owning_app, meta.started_at);
    let canonical = format!("{stem}.evidence.json");
    let owned = [canonical.clone(), format!("{canonical}.tmp-19-3")];
    let preserved = [
        "recording.evidence.json".to_string(),
        "20261030-010203-Upper.evidence.json".to_string(),
        "20261030-010203--app.evidence.json".to_string(),
        "20261030-010203-app-.evidence.json".to_string(),
        "20261030-010203-åpp.evidence.json".to_string(),
        format!("{canonical}.tmp-019-3"),
        format!("{canonical}.tmp-19-03"),
        format!("{canonical}.tmp-19-3-extra"),
        format!("{canonical}.tmp-4294967296-3"),
        format!("{canonical}.tmp-19-18446744073709551616"),
        format!("{canonical}.backup"),
        format!("{stem}.wav"),
        format!("{stem}.quality.json"),
    ];
    for name in owned.iter().chain(&preserved) {
        std::fs::write(directory.join(name), b"synthetic sentinel").unwrap();
    }
    let nested = directory.join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join(&canonical), b"nested sentinel").unwrap();
    let target = directory.join("unrelated-target");
    std::fs::write(&target, b"symlink target sentinel").unwrap();
    let link = directory.join(format!("{stem}-link.evidence.json"));
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(remove_capture_evidence_in(&directory), Ok(3));
    for name in owned {
        assert!(!directory.join(name).exists());
    }
    for name in preserved {
        assert_eq!(
            std::fs::read(directory.join(name)).unwrap(),
            b"synthetic sentinel"
        );
    }
    assert!(!link.exists());
    assert_eq!(std::fs::read(target).unwrap(), b"symlink target sentinel");
    assert_eq!(
        std::fs::read(nested.join(&canonical)).unwrap(),
        b"nested sentinel"
    );
    // An unlink failure is reported without preventing the other known artifact's removal.
    let blocked = directory.join(&canonical);
    std::fs::create_dir(&blocked).unwrap();
    let temporary = directory.join(format!("{canonical}.tmp-19-4"));
    std::fs::write(&temporary, b"synthetic temporary").unwrap();
    assert_eq!(remove_capture_evidence_in(&directory), Err(Error::Io));
    assert!(blocked.is_dir());
    assert!(!temporary.exists());
    assert_eq!(remove_capture_evidence_in(&directory.join("absent")), Ok(0));
    assert_eq!(
        remove_capture_evidence_in(&directory.join("unrelated-target")),
        Err(Error::Io)
    );
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn private_atomic_windows_keep_precise_asr_and_bind_generation() {
    let (dir, meta) = fixture("windows");
    let mut session = Session::begin("recording", &meta).unwrap();
    append(&mut session, 0, "Um, Very very important.");
    let first = session.persist(&meta.audio_path);
    assert!(matches!(first, Status::Available { .. }));
    let first_bytes = std::fs::read(path_for(&meta.audio_path)).unwrap();
    let loaded = Session::load("recording", &meta, &meta.audio_path, first).unwrap();
    assert_eq!(loaded.encode().unwrap(), first_bytes);
    let row = &loaded.ledger().groups()[0].rows[0].segment;
    assert_eq!(
        (row.start.to_bits(), row.end.to_bits()),
        (1.234_f64.to_bits(), 2.987_f64.to_bits())
    );
    assert_eq!(row.text, "Um, Very very important.");
    assert_eq!(
        std::fs::metadata(path_for(&meta.audio_path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    append(&mut session, 1, "Another synthetic window.");
    let second = session.persist(&meta.audio_path);
    let loaded = Session::load("recording", &meta, &meta.audio_path, second).unwrap();
    assert_eq!(
        loaded
            .ledger()
            .groups()
            .iter()
            .flat_map(|g| &g.stages)
            .filter(|s| s.kind == StageKind::TrueAsr)
            .count(),
        2
    );
    assert!(Session::load("different", &meta, &meta.audio_path, second).is_err());
    let replacement = Session::begin("recording", &meta).unwrap();
    replacement.persist(&meta.audio_path);
    assert!(Session::load("recording", &meta, &meta.audio_path, second).is_err());
    assert!(temporary_paths(&meta.audio_path).unwrap().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn failed_atomic_publication_preserves_old_bytes_and_cleans_registered_temps() {
    use crate::private_file::{AtomicWritePhase, atomic_write_private_with_hook};
    for (index, phase) in [
        AtomicWritePhase::Write,
        AtomicWritePhase::Sync,
        AtomicWritePhase::Rename,
    ]
    .into_iter()
    .enumerate()
    {
        let (dir, meta) = fixture(&format!("atomic-{index}"));
        let mut session = Session::begin("recording", &meta).unwrap();
        append(&mut session, 0, "Prior synthetic generation.");
        let receipt = session.persist(&meta.audio_path);
        let prior = std::fs::read(path_for(&meta.audio_path)).unwrap();
        append(&mut session, 1, "Replacement synthetic generation.");
        let next = session.encode().unwrap();
        let result = atomic_write_private_with_hook(
            &path_for(&meta.audio_path),
            &next,
            "local evidence",
            |at| {
                if at == phase {
                    anyhow::bail!("synthetic injected fault")
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read(path_for(&meta.audio_path)).unwrap(), prior);
        assert!(Session::load("recording", &meta, &meta.audio_path, receipt).is_ok());
        assert!(temporary_paths(&meta.audio_path).unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn truncated_evidence_is_durable_and_cannot_fail_filing() {
    let (dir, meta) = fixture("cap");
    let mut session = Session::begin("recording", &meta).unwrap();
    session.ledger = Ledger::with_limits(
        corti_transcribe::evidence::METADATA_RESERVE
            + corti_transcribe::evidence::QUALITY_RESERVE
            + 256,
        DEFAULT_OWNED_CAP,
    )
    .unwrap();
    append(&mut session, 0, &"Synthetic large input. ".repeat(100));
    let status = session.persist(&meta.audio_path);
    assert!(matches!(status, Status::Truncated { .. }));
    let loaded = Session::load("recording", &meta, &meta.audio_path, status).unwrap();
    assert!(loaded.ledger().truncation().cause.is_some());
    assert!(std::fs::metadata(path_for(&meta.audio_path)).unwrap().len() < MAX_FILE_BYTES as u64);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn directory_sync_failure_reports_error_after_complete_atomic_replacement() {
    use crate::private_file::{AtomicWritePhase, atomic_write_private_with_hook};
    let (dir, meta) = fixture("directory-sync");
    let mut session = Session::begin("recording", &meta).unwrap();
    append(&mut session, 0, "Prior synthetic generation.");
    let status = session.persist(&meta.audio_path);
    append(&mut session, 1, "New complete synthetic generation.");
    let next = session.encode().unwrap();
    let result = atomic_write_private_with_hook(
        &path_for(&meta.audio_path),
        &next,
        "local evidence",
        |phase| {
            if phase == AtomicWritePhase::DirectorySync {
                anyhow::bail!("synthetic directory sync fault");
            }
            Ok(())
        },
    );
    assert!(result.is_err());
    assert_eq!(std::fs::read(path_for(&meta.audio_path)).unwrap(), next);
    assert_eq!(
        Session::load("recording", &meta, &meta.audio_path, status)
            .unwrap()
            .encode()
            .unwrap(),
        next
    );
    assert!(temporary_paths(&meta.audio_path).unwrap().is_empty());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn optout_removes_evidence_and_temps_but_never_audio_or_quality() {
    let (dir, meta) = fixture("optout");
    std::fs::write(&meta.audio_path, b"synthetic audio placeholder").unwrap();
    let quality = corti_core::quality_store::quality_sidecar_path(&meta.audio_path).unwrap();
    std::fs::write(&quality, b"mandatory quality placeholder").unwrap();
    let session = Session::begin("recording", &meta).unwrap();
    session.persist(&meta.audio_path);
    let temp = PathBuf::from(format!(
        "{}.tmp-123-4",
        path_for(&meta.audio_path).display()
    ));
    std::fs::write(&temp, b"synthetic interrupted evidence").unwrap();
    assert_eq!(remove(&meta.audio_path), Ok(2));
    assert!(meta.audio_path.exists());
    assert!(quality.exists());
    assert!(!temp.exists());
    assert!(!path_for(&meta.audio_path).exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn optout_unlinks_known_file_when_directory_listing_is_denied_and_reports_partial_failure() {
    let (dir, meta) = fixture("optout-unlistable");
    let path = path_for(&meta.audio_path);
    std::fs::write(&path, b"synthetic optional evidence").unwrap();
    std::fs::write(&meta.audio_path, b"synthetic retained audio").unwrap();
    let quality = corti_core::quality_store::quality_sidecar_path(&meta.audio_path).unwrap();
    std::fs::write(&quality, b"synthetic mandatory quality").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300)).unwrap();
    let listing_denied = std::fs::read_dir(&dir).is_err();
    let result = remove(&meta.audio_path);
    // Restore fixture permissions before assertions so failures never strand test cleanup.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!path.exists());
    assert!(meta.audio_path.exists());
    assert!(quality.exists());
    if listing_denied {
        assert_eq!(
            result,
            Err(Error::Io),
            "unverified temporary cleanup must stay unavailable"
        );
    } else {
        assert_eq!(result, Ok(1));
    } // Root test runners bypass directory DAC, unlike production.
    assert_eq!(remove(&meta.audio_path), Ok(0));
    let blocked = PathBuf::from(format!("{}.tmp-1-1", path.display()));
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(&path, b"synthetic optional evidence").unwrap();
    assert_eq!(remove(&meta.audio_path), Err(Error::Io));
    assert!(!path.exists());
    assert!(blocked.is_dir());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn identity_documents_are_recoverable_and_exclude_secrets_paths_and_quality() {
    use crate::config::{AppConfig, BackendChoice};
    let (dir, meta) = fixture("identity");
    let cfg = AppConfig {
        transcribe_backend: BackendChoice::Aws,
        ..Default::default()
    };
    let provenance = crate::provenance::from_config(&cfg, GenerationMode::Batch);
    let session = Session::with_provenance("recording", &meta, &provenance).unwrap();
    let first = session.identity();
    let mut secret_cfg = cfg.clone();
    secret_cfg.aws_bucket = Some("synthetic-private-bucket".into());
    secret_cfg.aws_profile = Some("synthetic-private-profile".into());
    secret_cfg.local_model_dir = Some("/private/synthetic-model-root".into());
    let mut private_provenance = crate::provenance::from_config(&secret_cfg, GenerationMode::Batch);
    private_provenance
        .configuration
        .insert("quality".into(), serde_json::json!({"not_identity": true}));
    private_provenance.configuration.insert(
        "unrecognized_private_key".into(),
        serde_json::json!("synthetic-private-value"),
    );
    let private = Session::with_provenance("recording", &meta, &private_provenance).unwrap();
    assert_eq!(
        first.configuration_hash,
        private.identity().configuration_hash
    );
    assert_eq!(first.model_hash, private.identity().model_hash);
    assert_eq!(first.runtime_hash, private.identity().runtime_hash);
    let encoded = String::from_utf8(private.encode().unwrap()).unwrap();
    for forbidden in ["synthetic-private", "synthetic-model-root", "not_identity"] {
        assert!(!encoded.contains(forbidden));
    }
    assert!(encoded.contains("aws/transcribe-default"));
    assert!(encoded.contains("sha256_identity_document_not_artifact"));
    let mut changed = provenance.clone();
    changed
        .configuration
        .insert("language".into(), serde_json::json!("es-ES"));
    assert_ne!(
        first.configuration_hash,
        Session::with_provenance("recording", &meta, &changed)
            .unwrap()
            .identity()
            .configuration_hash
    );
    changed = provenance.clone();
    changed.models.asr.id = "synthetic-other-model".into();
    assert_ne!(
        first.model_hash,
        Session::with_provenance("recording", &meta, &changed)
            .unwrap()
            .identity()
            .model_hash
    );
    changed = provenance.clone();
    changed.version = "synthetic-runtime-version".into();
    assert_ne!(
        first.runtime_hash,
        Session::with_provenance("recording", &meta, &changed)
            .unwrap()
            .identity()
            .runtime_hash
    );
    changed
        .configuration
        .insert("language".into(), serde_json::json!("x".repeat(8192)));
    assert!(Session::with_provenance("recording", &meta, &changed).is_err());
    let status = session.persist(&meta.audio_path);
    let loaded = Session::load("recording", &meta, &meta.audio_path, status).unwrap();
    assert_eq!(loaded.identity.version, provenance.version);
    assert_eq!(loaded.identity.models, provenance.models);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn merged_maximum_quality_and_activity_fit_reserved_capacity_after_text_truncation() {
    use corti_core::activity::{ActivityBlock, ChannelActivity};
    use corti_core::{
        ActivityInput, ActivityScope, CaptureEpoch, CaptureEpochReason, QualityCause, QualityClass,
        QualityRecord, QualityResolution, QualitySpan, QualityStage, QualityTrack,
    };
    let (dir, meta) = fixture("merged-full-quality");
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
    for (index, scope) in [
        ActivityScope::Capture,
        ActivityScope::Offline,
        ActivityScope::Live,
    ]
    .into_iter()
    .enumerate()
    {
        let mut observation = ChannelActivity::new(
            scope,
            ActivityInput::new([index as u8 + 1; 32], 48_000).unwrap(),
        )
        .unwrap();
        observation
            .push(
                ActivityBlock {
                    epoch: quality.epochs[0],
                    retained_start: 0,
                    mic: &[0.1; 8],
                    tap: &[0.2; 8],
                    unavailable: Some(&[]),
                },
                &mut quality,
            )
            .unwrap();
        observation.finish(Some(8), &mut quality).unwrap();
    }
    quality.overflow.omitted_spans = [u64::MAX; 3];
    quality.overflow.outstanding_omitted_spans = [u64::MAX; 3];
    quality.overflow.omitted_epochs = u64::MAX;
    quality.overflow.counter_saturated = true;
    quality.validate().unwrap();
    assert_eq!(quality.epochs.len(), corti_core::MAX_QUALITY_EPOCHS);
    assert_eq!(quality.spans.len(), corti_core::MAX_QUALITY_SPANS);
    assert!(serde_json::to_vec(&quality).unwrap().len() <= 256 * 1024);
    let _authority =
        corti_core::quality_store::QualityStore::begin(&meta.audio_path, "recording", &quality)
            .unwrap();
    let quality_path = corti_core::quality_store::quality_sidecar_path(&meta.audio_path).unwrap();
    let authority_bytes = std::fs::read(&quality_path).unwrap();
    let mut session = Session::begin("recording", &meta).unwrap();
    session.ledger = Ledger::with_limits(256 * 1024 + 4096 + 8192, DEFAULT_OWNED_CAP).unwrap();
    append(&mut session, 0, "Retained synthetic prefix.");
    append(&mut session, 1, &"Large synthetic suffix. ".repeat(8192));
    assert!(session.ledger.truncation().cause.is_some());
    session.ledger.set_final_quality(quality.clone()).unwrap();
    let status = session.persist(&meta.audio_path);
    assert!(matches!(status, Status::Truncated { .. }));
    let loaded = Session::load("recording", &meta, &meta.audio_path, status).unwrap();
    assert_eq!(loaded.ledger.final_quality(), Some(&quality));
    assert!(std::fs::metadata(path_for(&meta.audio_path)).unwrap().len() <= MAX_FILE_BYTES as u64);
    assert_eq!(std::fs::read(&quality_path).unwrap(), authority_bytes);
    remove(&meta.audio_path).unwrap();
    assert_eq!(std::fs::read(quality_path).unwrap(), authority_bytes);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn readback_rejects_rebound_identity_documents_and_keeps_fixed_error_categories() {
    let (dir, meta) = fixture("identity-readback");
    let mut session = Session::begin("recording", &meta).unwrap();
    append(&mut session, 0, "Synthetic retained words.");
    let status = session.persist(&meta.audio_path);
    let original: serde_json::Value = serde_json::from_slice(&session.encode().unwrap()).unwrap();
    for mutation in ["kind", "unknown_key", "configuration", "stage_hash"] {
        let mut changed = original.clone();
        match mutation {
            "kind" => {
                changed["identity"]["fingerprint_kind"] = serde_json::json!("syntheticquartzfalcon")
            }
            "unknown_key" => {
                changed["identity"]["configuration"]["private_path"] =
                    serde_json::json!("syntheticquartzfalcon")
            }
            "configuration" => {
                changed["identity"]["configuration"]["language"] =
                    serde_json::json!("syntheticquartzfalcon")
            }
            _ => {
                changed["ledger"]["groups"][0]["identity"]["configuration_hash"] =
                    serde_json::json!("0".repeat(64))
            }
        }
        crate::private_file::atomic_write_private(
            &path_for(&meta.audio_path),
            &serde_json::to_vec(&changed).unwrap(),
            "synthetic identity fixture",
        )
        .unwrap();
        let error = match Session::load("recording", &meta, &meta.audio_path, status) {
            Err(error) => error,
            Ok(_) => panic!("altered identity unexpectedly accepted"),
        };
        assert!(!format!("{error:?}").contains("syntheticquartzfalcon"));
    }
    std::fs::remove_dir_all(dir).unwrap();
}
