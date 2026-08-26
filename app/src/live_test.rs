//! Explicit, ephemeral microphone/transcription test for the Live Transcript window (issue #105).
//!
//! Test mode owns the default input directly through `MicrophoneCapture`: no system-audio tap, WAV, queue
//! row, Vagus note, or durable transcript. The detector acknowledges a pause before Corti opens the mic,
//! preventing Corti's own orange-dot edge from creating a duplicate detected call. The same one-model gate
//! used by live filing excludes overlap with calls and finishing/discarding sessions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use tauri::{AppHandle, State};

use crate::config::{AppConfig, BackendChoice};
use crate::live_view::LiveTranscriptStore;
use crate::settings::SharedConfig;

#[cfg(feature = "local")]
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
#[cfg(feature = "local")]
use std::time::Duration;

/// About eleven seconds / four MiB of mono 48 kHz PCM: bounded slack after the model is already loaded.
#[cfg(feature = "local")]
const TEST_TEE_BACKLOG: usize = 128;

pub(crate) struct LiveTestManager {
    inner: Mutex<Option<TestSlot>>,
    window: LiveWindowLifecycle,
    next_generation: AtomicU64,
    live: Arc<crate::live::LiveManager>,
    config: SharedConfig,
    transcript: LiveTranscriptStore,
}

#[derive(Default)]
struct LiveWindowLifecycle {
    current: Mutex<Option<u64>>,
    next_generation: AtomicU64,
}

impl LiveWindowLifecycle {
    fn begin(&self) -> u64 {
        let mut current = self.current.lock().unwrap();
        if let Some(generation) = *current {
            return generation;
        }
        let generation = self
            .next_generation
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        *current = Some(generation);
        generation
    }

    fn current(&self) -> Option<u64> {
        *self.current.lock().unwrap()
    }

    fn is_current(&self, generation: u64) -> bool {
        self.current() == Some(generation)
    }

    fn close(&self, generation: u64) -> bool {
        let mut current = self.current.lock().unwrap();
        if *current != Some(generation) {
            return false;
        }
        *current = None;
        true
    }
}

enum TestSlot {
    Starting {
        generation: u64,
        window_generation: u64,
        stop_requested: bool,
    },
    Running(TestSession),
}

struct TestSession {
    generation: u64,
    id: String,
    stop_tx: Sender<()>,
    handle: JoinHandle<()>,
}

impl LiveTestManager {
    pub(crate) fn new(
        live: Arc<crate::live::LiveManager>,
        config: SharedConfig,
        transcript: LiveTranscriptStore,
    ) -> Self {
        Self {
            inner: Mutex::new(None),
            window: LiveWindowLifecycle::default(),
            next_generation: AtomicU64::new(1),
            live,
            config,
            transcript,
        }
    }

    fn reap_finished(&self) {
        let finished = {
            let mut inner = self.inner.lock().unwrap();
            match inner.as_ref() {
                Some(TestSlot::Running(session)) if session.handle.is_finished() => {
                    let Some(TestSlot::Running(session)) = inner.take() else {
                        unreachable!()
                    };
                    Some(session.handle)
                }
                Some(TestSlot::Starting { .. }) | Some(TestSlot::Running(_)) | None => None,
            }
        };
        if let Some(handle) = finished
            && handle.join().is_err()
        {
            tracing::warn!(target: "corti::live_test", "microphone-test worker panicked");
        }
    }

    pub(crate) fn begin_live_window(&self) -> u64 {
        self.window.begin()
    }

    pub(crate) fn close_live_window(&self, window_generation: u64) {
        if self.window.close(window_generation) {
            self.stop();
        }
    }

    pub(crate) fn current_live_window_generation(&self) -> Option<u64> {
        self.window.current()
    }

    pub(crate) fn start_for_window(&self, app: &AppHandle, window_generation: u64) -> Result<()> {
        self.reap_finished();
        if crate::imp::detector_recording(app) {
            anyhow::bail!("a call is already recording; use Read live transcript instead");
        }
        if crate::imp::webinar_active(app) {
            anyhow::bail!("stop the webinar recording before testing the microphone");
        }
        if crate::imp::transcription_active(app) {
            anyhow::bail!(
                "wait for the current transcription to finish before testing the microphone"
            );
        }
        if crate::imp::live_test_active(app) {
            return Ok(());
        }

        let generation = self.reserve_start_slot(window_generation)?;
        // Publish the reservation before any fallible preflight so a concurrent webinar tray click cannot
        // start another capture between our earlier check and the microphone opening.
        crate::imp::set_live_test_active(app, true);

        let start = self.start_reserved(app, generation, window_generation);
        if start.is_err() {
            crate::imp::set_live_test_active(app, false);
            let mut inner = self.inner.lock().unwrap();
            if matches!(
                inner.as_ref(),
                Some(TestSlot::Starting {
                    generation: current,
                    ..
                }) if *current == generation
            ) {
                *inner = None;
            }
        }
        start
    }

    fn reserve_start_slot(&self, window_generation: u64) -> Result<u64> {
        let mut inner = self.inner.lock().unwrap();
        if inner.is_some() {
            anyhow::bail!("the previous microphone test is still stopping");
        }
        // Check lifecycle while holding the same slot lock that publishes Starting. Window destruction
        // invalidates the lifecycle before requesting stop, so it either wins here or observes this slot.
        if !self.window.is_current(window_generation) {
            anyhow::bail!("the Live Transcript window closed before the microphone test started");
        }
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        *inner = Some(TestSlot::Starting {
            generation,
            window_generation,
            stop_requested: false,
        });
        Ok(generation)
    }

    fn start_reserved(
        &self,
        app: &AppHandle,
        generation: u64,
        window_generation: u64,
    ) -> Result<()> {
        let cfg = self.config.lock().unwrap().clone();
        validate_test_config(&cfg)?;
        if self.starting_canceled(generation, window_generation) {
            self.finish_canceled_start(app, generation);
            return Ok(());
        }
        crate::imp::pause_detector(app)
            .context("pausing call detection for the microphone test")?;
        if self.starting_canceled(generation, window_generation) {
            crate::imp::resume_detector(app);
            self.finish_canceled_start(app, generation);
            return Ok(());
        }
        if !self.live.reserve_test(generation) {
            crate::imp::resume_detector(app);
            anyhow::bail!("the local transcription model is still in use by a call");
        }
        if self.starting_canceled(generation, window_generation) {
            self.live.release_test(generation);
            crate::imp::resume_detector(app);
            self.finish_canceled_start(app, generation);
            return Ok(());
        }

        let id = format!("microphone-test-{generation}");
        self.transcript.begin_test(&id);
        crate::tray::set_status(app, "Loading microphone transcription test…".to_string());

        let (stop_tx, stop_rx) = std::sync::mpsc::channel();
        let worker_app = app.clone();
        let worker_store = self.transcript.clone();
        let worker_live = self.live.clone();
        let worker_id = id.clone();
        let handle = match std::thread::Builder::new()
            .name("corti-live-test".into())
            .spawn(move || {
                run_test_worker(
                    &worker_app,
                    generation,
                    cfg,
                    worker_store,
                    worker_live,
                    worker_id,
                    stop_rx,
                );
            }) {
            Ok(handle) => handle,
            Err(error) => {
                self.live.release_test(generation);
                crate::imp::resume_detector(app);
                self.transcript
                    .set_error(&id, "Could not start the microphone-test worker.");
                crate::imp::set_live_test_active(app, false);
                return Err(error).context("spawning microphone-test worker");
            }
        };

        let mut inner = self.inner.lock().unwrap();
        let stop_immediately = matches!(
            inner.as_ref(),
            Some(TestSlot::Starting {
                generation: current,
                stop_requested: true,
                ..
            }) if *current == generation
        );
        debug_assert!(matches!(
            inner.as_ref(),
            Some(TestSlot::Starting {
                generation: current,
                ..
            }) if *current == generation
        ));
        if stop_immediately {
            let _ = stop_tx.send(());
        }
        *inner = Some(TestSlot::Running(TestSession {
            generation,
            id,
            stop_tx,
            handle,
        }));
        Ok(())
    }

    fn starting_canceled(&self, generation: u64, window_generation: u64) -> bool {
        if !self.window.is_current(window_generation) {
            return true;
        }
        let inner = self.inner.lock().unwrap();
        !matches!(
            inner.as_ref(),
            Some(TestSlot::Starting {
                generation: current,
                window_generation: owner,
                stop_requested: false,
            }) if *current == generation && *owner == window_generation
        )
    }

    /// Keep the canceled generation installed until every reservation has been released. Clearing the
    /// global flag before conditionally removing the slot means a racing restart either sees Active or the
    /// old slot; stale cleanup can never run underneath a new generation.
    fn finish_canceled_start(&self, app: &AppHandle, generation: u64) {
        crate::imp::set_live_test_active(app, false);
        let mut inner = self.inner.lock().unwrap();
        if matches!(
            inner.as_ref(),
            Some(TestSlot::Starting {
                generation: current,
                ..
            }) if *current == generation
        ) {
            *inner = None;
        }
    }

    pub(crate) fn stop(&self) {
        self.reap_finished();
        let (id, generation, sent) = {
            let mut inner = self.inner.lock().unwrap();
            match inner.as_mut() {
                Some(TestSlot::Running(session)) => (
                    session.id.clone(),
                    session.generation,
                    session.stop_tx.send(()).is_ok(),
                ),
                Some(TestSlot::Starting {
                    generation,
                    stop_requested,
                    ..
                }) => {
                    *stop_requested = true;
                    (format!("microphone-test-{generation}"), *generation, false)
                }
                None => return,
            }
        };
        if sent {
            self.transcript
                .set_stopping(&id, "Stopping the microphone and flushing final words…");
            tracing::info!(target: "corti::live_test", generation, "stopping microphone test");
        }
    }
}

fn validate_test_config(cfg: &AppConfig) -> Result<()> {
    if cfg.transcribe_backend != BackendChoice::Local {
        anyhow::bail!("microphone live transcription requires the local backend");
    }
    #[cfg(feature = "local")]
    {
        use corti_transcribe_local::{LocalConfig, LocalTranscriber};
        LocalTranscriber::new(LocalConfig {
            model_dir: cfg.local_model_dir.clone(),
            diarize_far_end: false,
            asr_engine: cfg.local_asr_engine.clone(),
            ggml_model: cfg.local_ggml_model.clone(),
            ..LocalConfig::default()
        })
        .validate_models()
        .context("selected local models are unavailable")?;
        Ok(())
    }
    #[cfg(not(feature = "local"))]
    {
        anyhow::bail!("local transcription is not compiled into this build")
    }
}

fn run_test_worker(
    app: &AppHandle,
    generation: u64,
    cfg: AppConfig,
    transcript: LiveTranscriptStore,
    live: Arc<crate::live::LiveManager>,
    id: String,
    stop_rx: Receiver<()>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_microphone_test(app, &cfg, &transcript, &id, &stop_rx)
    }));
    let status = match result {
        Ok(Ok(detail)) => {
            transcript.set_complete(&id, detail.clone());
            Ok(detail)
        }
        Ok(Err(error)) => {
            let detail = format!("Microphone test failed: {error:#}");
            transcript.set_error(&id, &detail);
            Err(detail)
        }
        Err(_) => {
            let detail = "Microphone test panicked; the microphone was closed.".to_string();
            transcript.set_error(&id, &detail);
            Err(detail)
        }
    };

    // Ordering is load-bearing: close/drop capture inside `run_microphone_test`, then re-enable the detector.
    live.release_test(generation);
    crate::imp::resume_detector(app);
    match status {
        Ok(_) => crate::tray::set_status(app, "Idle — microphone test complete".to_string()),
        Err(detail) => crate::tray::set_status(app, format!("⚠ {detail}")),
    }
    // Publish availability last: a due retry may start immediately and should own any later tray status.
    crate::imp::set_live_test_active(app, false);
}

#[cfg(feature = "local")]
fn run_microphone_test(
    app: &AppHandle,
    cfg: &AppConfig,
    transcript: &LiveTranscriptStore,
    id: &str,
    stop_rx: &Receiver<()>,
) -> Result<String> {
    use corti_capture::{CaptureChunk, CaptureTee, MicrophoneCapture};
    use corti_core::Speaker;
    use corti_transcribe_local::{LocalConfig, LocalTranscriber};

    let engine = LocalTranscriber::new(LocalConfig {
        model_dir: cfg.local_model_dir.clone(),
        num_threads: cfg.local_threads,
        diarize_far_end: false,
        asr_engine: cfg.local_asr_engine.clone(),
        ggml_model: cfg.local_ggml_model.clone(),
        ..LocalConfig::default()
    })
    .live_engine()
    .context("loading the local live engine")?;
    let mut channel = engine
        .channel()
        .context("building the microphone transcriber")?;
    if stop_rx.try_recv().is_ok() {
        return Ok("Microphone test stopped before capture began.".to_string());
    }

    let (tx, rx) = sync_channel::<CaptureChunk>(TEST_TEE_BACKLOG);
    let capture =
        MicrophoneCapture::start(CaptureTee::new(tx)).context("opening the default microphone")?;
    let sample_rate = capture.sample_rate();
    transcript.set_listening(
        id,
        "Listening to this microphone only — say a sentence, then pause.",
    );
    crate::tray::set_status(app, "● Microphone test — listening".to_string());

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(chunk) => publish_test_chunk(&mut channel, chunk, sample_rate, transcript, id),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("microphone capture ended unexpectedly")
            }
        }
    }

    let quality = capture.stop().context("stopping microphone capture")?;
    while let Ok(chunk) = rx.try_recv() {
        publish_test_chunk(&mut channel, chunk, sample_rate, transcript, id);
    }
    let tail = channel.finish();
    transcript.append_words(id, Speaker::Me, &tail);

    let gaps = quality.dropped_samples > 0 || quality.tee_dropped_chunks > 0;
    tracing::info!(
        target: "corti::live_test",
        frames = quality.frames,
        sample_rate = quality.sample_rate,
        callbacks = quality.callbacks,
        dropped_samples = quality.dropped_samples,
        dropped_chunks = quality.tee_dropped_chunks,
        "microphone transcription test finished"
    );
    Ok(if gaps {
        "Test complete, but capture dropped audio; the displayed transcript may have gaps."
            .to_string()
    } else if quality.frames == 0 {
        "Test stopped before the microphone delivered audio.".to_string()
    } else {
        "Test complete — no audio or transcript was saved.".to_string()
    })
}

#[cfg(feature = "local")]
fn publish_test_chunk(
    channel: &mut corti_transcribe_local::LiveTranscriber,
    chunk: corti_capture::CaptureChunk,
    sample_rate: u32,
    transcript: &LiveTranscriptStore,
    id: &str,
) {
    use corti_core::Speaker;

    if chunk.mic.is_empty() {
        return;
    }
    channel.push(&chunk.mic, sample_rate);
    if let Some(words) = channel.poll_words() {
        transcript.append_words(id, Speaker::Me, &words);
    }
}

#[cfg(not(feature = "local"))]
fn run_microphone_test(
    _app: &AppHandle,
    _cfg: &AppConfig,
    _transcript: &LiveTranscriptStore,
    _id: &str,
    _stop_rx: &Receiver<()>,
) -> Result<String> {
    anyhow::bail!("local transcription is not compiled into this build")
}

#[tauri::command]
pub(crate) fn get_live_test_window_generation(
    manager: State<'_, LiveTestManager>,
    window: tauri::WebviewWindow,
) -> Result<u64, String> {
    if window.label() != "live" {
        return Err("microphone test is unavailable from this window".to_string());
    }
    manager
        .current_live_window_generation()
        .ok_or_else(|| "the Live Transcript window lifecycle is unavailable".to_string())
}

#[tauri::command]
pub(crate) fn start_live_test(
    window_generation: u64,
    app: AppHandle,
    manager: State<'_, LiveTestManager>,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    if window.label() != "live" {
        return Err("microphone test is unavailable from this window".to_string());
    }
    manager
        .start_for_window(&app, window_generation)
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
pub(crate) fn stop_live_test(
    manager: State<'_, LiveTestManager>,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    if window.label() != "live" {
        return Err("microphone test is unavailable from this window".to_string());
    }
    manager.stop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_for_lifecycle_test() -> LiveTestManager {
        LiveTestManager::new(
            Arc::new(crate::live::LiveManager::new()),
            Arc::new(Mutex::new(AppConfig::default())),
            LiveTranscriptStore::detached(),
        )
    }

    #[test]
    fn closed_window_cannot_reserve_a_late_microphone_start() {
        let manager = manager_for_lifecycle_test();
        let closed = manager.begin_live_window();
        manager.close_live_window(closed);
        let current = manager.begin_live_window();
        let error = manager.reserve_start_slot(closed).unwrap_err().to_string();
        assert!(error.contains("window closed"), "{error}");

        let test_generation = manager.reserve_start_slot(current).unwrap();
        manager.close_live_window(current);
        assert!(manager.starting_canceled(test_generation, current));
    }

    #[test]
    fn live_window_generations_reject_stale_close_and_start_owners() {
        let lifecycle = LiveWindowLifecycle::default();
        let first = lifecycle.begin();
        assert_eq!(
            lifecycle.begin(),
            first,
            "one singleton window keeps one owner"
        );
        assert!(lifecycle.is_current(first));
        assert!(lifecycle.close(first));
        assert!(!lifecycle.is_current(first));
        let second = lifecycle.begin();
        assert_ne!(second, first);
        assert!(
            !lifecycle.close(first),
            "stale destruction cannot close replacement"
        );
        assert!(lifecycle.is_current(second));
    }

    #[test]
    fn aws_configuration_is_rejected_before_any_model_or_microphone_work() {
        let cfg = AppConfig {
            transcribe_backend: BackendChoice::Aws,
            ..AppConfig::default()
        };
        let error = validate_test_config(&cfg).unwrap_err().to_string();
        assert!(error.contains("requires the local backend"));
    }
}
