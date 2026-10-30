#[cfg(test)]
mod tests {
    use super::*;
    use corti_core::{JobStatus, OwningApp, RecordingMeta};

    #[test]
    fn actual_sweep_optout_and_deletion_cover_active_and_aws_owned_rows() {
        for (index, status) in [
            JobStatus::Recording,
            JobStatus::PendingNote,
            JobStatus::Failed,
            JobStatus::Done,
        ]
        .into_iter()
        .enumerate()
        {
            let dir =
                std::env::temp_dir().join(format!("evidence-sweep-{index}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let audio = dir.join("recording.wav");
            let meta = RecordingMeta {
                audio_path: audio.clone(),
                started_at: Local::now(),
                ended_at: None,
                owning_app: OwningApp::unknown(),
            };
            let queue = Queue::open_at(dir.join("queue.db")).unwrap();
            let id = queue.enqueue(&meta).unwrap();
            queue.set_status(&id, status).unwrap();
            let evidence = crate::evidence::path_for(&audio);
            let temp = PathBuf::from(format!("{}.tmp-19-3", evidence.display()));
            let quality = corti_core::quality_store::quality_sidecar_path(&audio).unwrap();
            for path in [&audio, &evidence, &temp, &quality] {
                std::fs::write(path, b"synthetic placeholder").unwrap();
            }
            let staging = crate::checkpoint::AwsStaging {
                bucket: "synthetic".into(),
                key_prefix: "corti/".into(),
                job_name: id.clone(),
                region: None,
            };
            staging.store(&audio).unwrap();
            assert!(
                crate::checkpoint::temporary_paths(&audio)
                    .unwrap()
                    .contains(&temp)
            );
            assert_eq!(remove_disabled_evidence(&queue).unwrap(), 2);
            assert!(audio.exists());
            assert!(quality.exists());
            assert!(crate::checkpoint::aws_staging_path_for(&audio).exists());
            assert!(!evidence.exists());
            assert!(!temp.exists());
            crate::checkpoint::AwsStaging::remove(&audio).unwrap();
            std::fs::write(&evidence, b"synthetic evidence").unwrap();
            if status.is_terminal() {
                let (files, _) =
                    sweep_recordings(&queue, 7, Local::now() + chrono::Duration::days(8)).unwrap();
                assert_eq!(files, 3);
                assert!(!audio.exists());
                assert!(!evidence.exists());
                assert!(!quality.exists());
            } else {
                assert_eq!(
                    sweep_recordings(&queue, 7, Local::now() + chrono::Duration::days(8)).unwrap(),
                    (0, 0)
                );
                assert!(audio.exists());
                assert!(evidence.exists());
                assert_eq!(remove_recording_artifacts(&id, &audio), (3, true));
            }
            drop(queue);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
