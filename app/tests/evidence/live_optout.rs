#[test]
fn actual_live_pre_note_optout_sweep_removes_capture_owned_evidence() {
    const CHILD: &str = "CORTI_EVIDENCE_PRE_NOTE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Isolate the real capture-root environment override from concurrently running tests. The
        // child executes the same production functions and native capture path helpers below.
        let directory = std::env::temp_dir().join(format!(
            "corti-evidence-pre-note-capture-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "live::tests::actual_live_pre_note_optout_sweep_removes_capture_owned_evidence",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("CORTI_RECORDINGS_DIR", &directory)
            .output()
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }

    for fail_first_note in [false, true] {
        let (mut window, mut writer) = evidence_window("pre-note-optout", false);
        // Match Recorder::new_recording_path, including its stem grammar, without native capture.
        let recordings = ::corti_capture::recordings_dir().unwrap();
        let stem = ::corti_capture::recording_stem(&writer.meta.owning_app, writer.meta.started_at);
        writer.meta.audio_path = recordings.join(format!("{stem}.wav"));
        std::fs::write(&writer.meta.audio_path, b"synthetic capture bytes").unwrap();
        window.arm_evidence(&writer, &crate::config::AppConfig::default());
        let queue = corti_queue::Queue::open_at(writer.filer.dir.join("queue.db")).unwrap();
        let quality =
            corti_core::quality_store::quality_sidecar_path(&writer.meta.audio_path).unwrap();
        std::fs::write(&quality, b"mandatory quality sentinel").unwrap();
        if fail_first_note {
            std::fs::create_dir(writer.filer.note()).unwrap();
        }
        let text = if fail_first_note {
            "Substantive synthetic speech before first note failure."
        } else {
            "um"
        };
        publish_mic_words(&mut window, &NoopPublisher, vec![word(1.234, 2.987, text)]);
        release_held_mic_words(&mut window, &NoopPublisher);
        assert_eq!(
            flush_window(&NoDiarizer, &mut window, &mut writer).is_err(),
            fail_first_note
        );
        assert!(writer.path().is_none());
        assert!(queue.all().unwrap().is_empty());
        let path = crate::evidence::path_for(&writer.meta.audio_path);
        assert!(std::fs::read_to_string(&path).unwrap().contains(text));
        let temporary = PathBuf::from(format!("{}.tmp-19-3", path.display()));
        std::fs::write(&temporary, b"synthetic interrupted publication").unwrap();
        writer
            .evidence_config
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .evidence
            .enabled = false;
        assert_eq!(crate::evidence_retention::optout_sweep(&queue).unwrap(), 2);
        // The next sweep must finish removal before the worker polls disabled settings.
        assert!(!path.exists());
        assert!(!temporary.exists());
        assert!(window.evidence.is_some());
        assert_eq!(
            std::fs::read(&quality).unwrap(),
            b"mandatory quality sentinel"
        );
        assert_eq!(
            std::fs::read(&writer.meta.audio_path).unwrap(),
            b"synthetic capture bytes"
        );
        window.persist_evidence(&writer, None);
        assert!(window.evidence.is_none());
        assert!(!path.exists());
        assert!(queue.all().unwrap().is_empty());
        // Restart needs no in-memory registration: an interrupted publication remains discoverable.
        drop(window);
        std::fs::write(&temporary, b"synthetic restart leftover").unwrap();
        assert_eq!(crate::evidence_retention::optout_sweep(&queue).unwrap(), 1);
        assert!(!temporary.exists());
        drop(queue);
        std::fs::remove_dir_all(&writer.filer.dir).unwrap();
    }
}
