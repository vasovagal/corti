//! corti — the menu-bar tray app that ties the pipeline together.
//!
//! A blinking record icon while a call is captured, and the detect → capture → transcribe → vagus pipeline
//! behind it. See `design/05-app-tauri.md`. Apple-Silicon + latest-macOS only (guardrail 2).
//!
//! ## Shape
//! - **Tray-only** (`ActivationPolicy::Accessory` + `LSUIElement`): no Dock icon, no window.
//! - **Detector callback** (off the HAL thread) flips the blink state + status line immediately and hands
//!   each finished recording to the **pipeline worker** over a channel — it never blocks on transcription.
//! - **Pipeline worker** is the sole owner of the `corti_queue::Queue` (rusqlite `Connection` is `Send` but
//!   not `Sync`). It runs each recording through the pipeline serially on its own thread (transcription
//!   blocks; guardrail 9 keeps it off the UI loop). The same thread also drains durable background jobs
//!   (`corti-jobs`): transcribe/file retry with backoff and the hourly retention sweep (#85).
//! - **Blink thread** toggles two template icons ~every 500 ms, marshalling AppKit calls to the main thread.

// macOS-only by design — like the rest of the workspace, this compiles to a stub elsewhere.
#[cfg(target_os = "macos")]
mod activity;
#[cfg(target_os = "macos")]
mod bedrock_creds;
#[cfg(target_os = "macos")]
mod checkpoint;
#[cfg(target_os = "macos")]
mod cli;
#[cfg(target_os = "macos")]
mod config;
#[cfg(target_os = "macos")]
mod console;
#[cfg(target_os = "macos")]
mod evidence;
#[cfg(target_os = "macos")]
mod jobs;
#[cfg(target_os = "macos")]
mod lexicon;
#[cfg(target_os = "macos")]
mod live;
#[cfg(target_os = "macos")]
mod live_test;
#[cfg(target_os = "macos")]
mod live_view;
#[cfg(target_os = "macos")]
mod permissions;
#[cfg(target_os = "macos")]
mod pipeline;
#[cfg(target_os = "macos")]
mod postprocess;
#[cfg(target_os = "macos")]
mod postprocess_app;
#[cfg(target_os = "macos")]
mod postprocess_config;
#[cfg(target_os = "macos")]
mod private_file;
#[cfg(target_os = "macos")]
mod provenance;
#[cfg(target_os = "macos")]
mod queue_ui;
#[cfg(target_os = "macos")]
mod secret_store;
#[cfg(target_os = "macos")]
mod secure_entry;
#[cfg(target_os = "macos")]
mod settings;
#[cfg(target_os = "macos")]
mod stats;
#[cfg(target_os = "macos")]
mod transcribe;
#[cfg(target_os = "macos")]
mod tray;
#[cfg(target_os = "macos")]
mod vertex_creds;
#[cfg(target_os = "macos")]
mod word_bank;

#[cfg(target_os = "macos")]
fn main() {
    // Parse argv first: with no/blank args this is `Cli::Run` and falls through to the tray (unchanged);
    // every other command runs headlessly and exits before the Tauri event loop ever starts.
    match cli::parse() {
        cli::Cli::Run => {
            if let Err(e) = imp::run_app() {
                eprintln!("[corti] fatal: {e:#}");
                std::process::exit(1);
            }
        }
        other => {
            // Headless path: install a stderr-only subscriber so the operator-facing diagnostics that are
            // now `tracing` events (AEC fallback, CPU fallback, AWS job failures, …) still print — the
            // console buffer + on-disk log in `run_app` are GUI-only and never run here.
            console::init_cli_tracing();
            std::process::exit(cli::dispatch(other))
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("corti is macOS-only (Apple Silicon, latest macOS).");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
pub(crate) mod imp {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread::JoinHandle;

    use anyhow::{Context, Result};
    use chrono::{DateTime, Local};
    use corti_capture::Recorder;
    use corti_core::{JobStatus, OwningApp, RecordingMeta, RecordingMode, WEBINAR_NAME};
    use corti_detect::{Detector, DetectorEvent};
    use tauri::Manager;

    use crate::config::AppConfig;
    use crate::pipeline::{self, PipelineMsg};
    use crate::{console, permissions, tray};

    /// One recording shown in the tray's `History ▸` submenu. Unlike the old `Done`-only "Recent notes"
    /// list this tracks a recording from the moment it starts (`Recording`) through every pipeline
    /// transition, so in-flight and failed recordings appear too (issue #3). The pipeline worker is the
    /// sole owner of the `Queue`, so the tray never queries it — the worker (and the capture-start sites)
    /// push/update this snapshot via [`tray::push_history`]/[`tray::update_history`].
    #[derive(Clone)]
    pub struct HistoryEntry {
        /// The queue job id (recording filename stem) — the key for [`tray::update_history`]. Computed at
        /// capture start from the recorder's output path so it matches the id the queue assigns later.
        pub id: String,
        /// Display label (owning-app name / note title).
        pub label: String,
        pub started_at: DateTime<Local>,
        /// `None` while still `Recording`; set once capture finishes.
        pub ended_at: Option<DateTime<Local>>,
        pub status: JobStatus,
        /// How the recording was captured (call vs. webinar), derived from existing signals (issue #28).
        /// Surfaced as a compact tag on the history line.
        pub mode: RecordingMode,
        /// Failure message, set alongside `JobStatus::Failed`.
        pub error: Option<String>,
        /// Path of the filed vagus note once `Done` — drives click-to-open.
        pub note_path: Option<PathBuf>,
    }

    /// The coarse pipeline stage the "How Corti Works" window highlights. A single low-cardinality signal
    /// distinct from the free-text `status` line: the UI maps it to which diagram box pulses. The shipped
    /// pipeline echo-cancels during `Recording` (in the capture writer, #74), so
    /// [`Stage::CancellingEcho`] is defined for completeness but not emitted on its own.
    #[derive(Clone, Copy, PartialEq, Eq)]
    #[repr(u8)]
    pub enum Stage {
        Idle = 0,
        Recording = 1,
        CancellingEcho = 2,
        Transcribing = 3,
        Filing = 4,
    }

    impl Stage {
        fn from_u8(v: u8) -> Stage {
            match v {
                1 => Stage::Recording,
                2 => Stage::CancellingEcho,
                3 => Stage::Transcribing,
                4 => Stage::Filing,
                _ => Stage::Idle,
            }
        }

        /// Stable id the UI keys its diagram on — mirrored in `app/ui/src/lib/pipeline.ts`.
        pub fn as_str(self) -> &'static str {
            match self {
                Stage::Idle => "idle",
                Stage::Recording => "recording",
                Stage::CancellingEcho => "cancelling_echo",
                Stage::Transcribing => "transcribing",
                Stage::Filing => "filing",
            }
        }
    }

    /// Shared app state (managed singleton). Everything here is read/written from background threads, so it
    /// is `Send + Sync`; the tray rebuilds its menu from this snapshot whenever it changes.
    pub struct AppState {
        /// Whether the detector (mic-triggered) capture is in flight.
        pub detector_recording: AtomicBool,
        /// Whether a manual webinar (tap-only) capture is in flight. Kept separate from
        /// [`detector_recording`] so the independent sources never clobber each other's blink/guard state.
        pub webinar_recording: AtomicBool,
        /// Whether Corti's explicit, non-filing microphone transcription test owns the default input.
        /// It participates in tray context/blink and excludes detector/webinar capture, but is not a
        /// pipeline `Recording` stage and never creates history.
        pub live_test_active: AtomicBool,
        /// The status line shown at the top of the tray menu.
        pub status: Mutex<String>,
        /// The current pipeline [`Stage`], read by the `get_pipeline_activity` command. A single global
        /// stage (like `status`), last-writer-wins — not "newest recording": with overlapping jobs an older
        /// job's worker transitions can clobber a live capture's `Recording`. The UI compensates by treating
        /// the `recording` flag as authoritative for the Detect/Capture boxes regardless of stage.
        pub stage: AtomicU8,
        /// The most-recent recordings (front = newest), capped at [`HISTORY_LIMIT`] — the `History ▸`
        /// submenu's source of truth, covering in-flight, failed, and filed recordings (issue #3).
        pub history: Mutex<VecDeque<HistoryEntry>>,
    }

    /// How many recordings the `History ▸` submenu shows (newest first).
    pub const HISTORY_LIMIT: usize = 5;

    impl AppState {
        // The tray's Backend:/Bucket: summary is derived live from `settings::ConfigState` in `build_menu`,
        // so it stays in sync with edits saved in the Settings window (no static labels here).
        fn new() -> Self {
            Self {
                detector_recording: AtomicBool::new(false),
                webinar_recording: AtomicBool::new(false),
                live_test_active: AtomicBool::new(false),
                status: Mutex::new("Starting…".to_string()),
                stage: AtomicU8::new(Stage::Idle as u8),
                history: Mutex::new(VecDeque::new()),
            }
        }

        /// The current pipeline stage.
        pub fn stage(&self) -> Stage {
            Stage::from_u8(self.stage.load(Ordering::Relaxed))
        }
    }

    /// Keeps the [`Detector`] alive for the app's lifetime. `Detector` is `Send` but not `Sync`, so the
    /// `Mutex` makes the managed holder `Send + Sync`. The live-test controller briefly locks it only to
    /// pause/resume edge detection around Corti's own microphone use.
    struct DetectorHandle(Mutex<Detector>);

    /// Manual "Webinar mode": a live tap-only [`Recorder`] driven by the tray toggle, plus a clone of the
    /// channel to the pipeline worker so a finished webinar enters the same transcribe → file path as a
    /// detected call. The `Mutex` makes the handle `Send + Sync` (both `Recorder` and `Sender` are `Send`
    /// but not `Sync`).
    struct Webinar(Mutex<WebinarState>);

    struct WebinarState {
        /// Stop workers remain owned until shutdown, including earlier user-initiated stops.
        finishing: Vec<JoinHandle<std::result::Result<(), String>>>,
        pending_finishes: usize,
        /// The in-flight tap-only recorder; `Some` while a webinar is being recorded.
        recorder: Option<Recorder>,
        /// When the in-flight recording started (for the filed note's `RecordingMeta`).
        started_at: Option<DateTime<Local>>,
        /// Hands a finished recording to the pipeline worker.
        tx: Sender<PipelineMsg>,
    }

    impl Webinar {
        fn new(tx: Sender<PipelineMsg>) -> Self {
            Self(Mutex::new(WebinarState {
                finishing: Vec::new(),
                pending_finishes: 0,
                recorder: None,
                started_at: None,
                tx,
            }))
        }

        /// Lock the state, recovering from a poisoned mutex instead of propagating the panic. A poisoned
        /// `Webinar` lock must not brick the tray: `webinar_active`/`build_menu` lock this on every menu
        /// rebuild, so a single panic while holding it would otherwise make every later refresh panic.
        fn lock(&self) -> std::sync::MutexGuard<'_, WebinarState> {
            self.0.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    /// The gate is set before any capture producer is stopped. The pipeline reads the same flag
    /// to avoid unrelated due-job backlog while waiting for the FIFO drain receipt.
    pub(crate) struct Shutdown {
        quitting: Arc<AtomicBool>,
        phase: Mutex<QuitPhase>,
        permit_exit: AtomicBool,
        detector_result: Mutex<Option<std::result::Result<Option<PathBuf>, String>>>,
        pipe_tx: Sender<PipelineMsg>,
        live: Arc<crate::live::LiveManager>,
    }

    pub(crate) fn is_quitting(app: &tauri::AppHandle) -> bool {
        app.try_state::<Shutdown>()
            .is_some_and(|s| s.quitting.load(Ordering::Acquire))
    }

    /// Explicit tray Quit progress. `Failed` is entered only after `finish_for_quit` returns, so every
    /// capture producer, live/discard thread and the pipeline drain has stopped; only then may the
    /// distinct "Quit anyway — recording failed" action exit.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum QuitPhase {
        Idle,
        Draining,
        Failed,
    }

    pub(crate) fn quit_phase(app: &tauri::AppHandle) -> QuitPhase {
        app.try_state::<Shutdown>()
            .map_or(QuitPhase::Idle, |state| *lock_phase(&state.phase))
    }

    fn lock_phase(phase: &Mutex<QuitPhase>) -> std::sync::MutexGuard<'_, QuitPhase> {
        phase.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Only an idle coordinator starts a drain. An ordinary Quit repeated (or queued) while draining
    /// or after a failure is ignored; it never acknowledges the failure.
    fn claim_quit(phase: &Mutex<QuitPhase>, quitting: &AtomicBool) -> bool {
        let mut phase = lock_phase(phase);
        if *phase != QuitPhase::Idle {
            return false;
        }
        *phase = QuitPhase::Draining;
        quitting.store(true, Ordering::Release);
        true
    }

    /// The acknowledgement action is honored only in the settled `Failed` phase.
    fn claim_failure_ack(phase: &Mutex<QuitPhase>) -> bool {
        *lock_phase(phase) == QuitPhase::Failed
    }

    /// Remove and join the handles that already returned. Their outcome was reported when they
    /// finished, so it belongs to no later Quit.
    fn take_finished<T>(handles: &mut Vec<JoinHandle<T>>) -> Vec<std::thread::Result<T>> {
        let mut finished = Vec::new();
        let mut index = 0;
        while index < handles.len() {
            if handles[index].is_finished() {
                finished.push(handles.swap_remove(index).join());
            } else {
                index += 1;
            }
        }
        finished
    }

    /// Combine producer failures with the drain receipt. Called only after the receipt arrives or the
    /// pipeline thread is proven gone.
    fn settle_quit(
        mut errors: Vec<String>,
        receipt: std::result::Result<(), String>,
    ) -> std::result::Result<(), String> {
        if let Err(error) = receipt {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    /// Close the nonblocking log writer, then permit the one programmatic exit. Existing recordings,
    /// notes and queue rows stay in place.
    fn exit_after_quit(app: &tauri::AppHandle, state: &Shutdown) {
        // The static otherwise never drops; process termination does not run static destructors.
        if let Some(guard) = LOG_GUARD.get() {
            drop(guard.lock().unwrap().take());
        }
        state.permit_exit.store(true, Ordering::Release);
        app.exit(0);
    }

    /// The distinct tray action shown only after a settled failed Quit; ignored in every other phase.
    pub(crate) fn acknowledge_quit_failure(app: &tauri::AppHandle) {
        let Some(state) = app.try_state::<Shutdown>() else {
            return;
        };
        if !claim_failure_ack(&state.phase) {
            return;
        }
        tracing::warn!(target: "corti::shutdown", "exiting after acknowledged recording failure; existing files kept");
        exit_after_quit(app, &state);
    }

    fn failed_quit_status(detail: &str) -> String {
        let summary: String = detail
            .lines()
            .next()
            .unwrap_or("write failed")
            .chars()
            .take(100)
            .collect();
        format!("⚠ Recording failed: {summary}")
    }

    /// Preserve an owned recorder when thread creation fails: Builder::spawn consumes and
    /// drops its closure on error, so the producer retains a separate reference to the slot.
    fn spawn_owned<T: Send + 'static, R: Send + 'static>(
        value: T,
        work: impl FnOnce(T) -> R + Send + 'static,
        spawn: impl FnOnce(Box<dyn FnOnce() -> R + Send>) -> std::io::Result<JoinHandle<R>>,
    ) -> std::result::Result<JoinHandle<R>, (std::io::Error, T)> {
        let slot = Arc::new(Mutex::new(Some(value)));
        let worker_slot = slot.clone();
        let task: Box<dyn FnOnce() -> R + Send> = Box::new(move || {
            let value = worker_slot
                .lock()
                .unwrap()
                .take()
                .expect("finish runs once");
            work(value)
        });
        match spawn(task) {
            Ok(handle) => Ok(handle),
            Err(error) => Err((
                error,
                slot.lock().unwrap().take().expect("worker did not start"),
            )),
        }
    }

    /// Only an explicit tray Quit enters this path; ordinary window closure remains tray-only.
    pub(crate) fn request_quit(app: &tauri::AppHandle) {
        let Some(state) = app.try_state::<Shutdown>() else {
            return;
        };
        if !claim_quit(&state.phase, &state.quitting) {
            return;
        }
        // Reaper panics count from here on; earlier ones were logged when reaped.
        state.live.set_quit_accounting(true);
        // A webinar finish that returned before Quit already reported itself; only finishes still
        // writing at the gate count toward this Quit.
        let historical = take_finished(&mut app.state::<Webinar>().lock().finishing);
        for result in historical {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::info!(target: "corti::shutdown", %error, "earlier webinar failure already reported; not counted against Quit")
                }
                Err(_) => {
                    tracing::error!(target: "corti::shutdown", "earlier webinar finish worker panicked; not counted against Quit")
                }
            }
        }
        tray::set_status(app, "Finish writes before exiting…".to_string());
        let app = app.clone();
        let worker_app = app.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("corti-quit".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    finish_for_quit(&worker_app)
                }));
                let state = worker_app.state::<Shutdown>();
                match result {
                    Ok(Ok(())) => exit_after_quit(&worker_app, &state),
                    Ok(Err(detail)) => {
                        // Settled: every writer has returned, so the acknowledgement may be offered.
                        tracing::error!(target: "corti::shutdown", %detail, "recording failed during quit");
                        *lock_phase(&state.phase) = QuitPhase::Failed;
                        tray::set_status(&worker_app, failed_quit_status(&detail));
                    }
                    Err(_) => {
                        // Quiescence is unproven; only an ordinary Quit that reruns the drain may follow.
                        tracing::error!(target: "corti::shutdown", "shutdown worker panicked");
                        *lock_phase(&state.phase) = QuitPhase::Idle;
                        tray::set_status(
                            &worker_app,
                            "⚠ Quit incomplete: shutdown worker panicked; see Diagnostics — Quit to retry"
                                .to_string(),
                        );
                    }
                }
            })
        {
            // The worker never ran and no producer was stopped: reopen capture for a clean retry.
            state.live.set_quit_accounting(false);
            state.quitting.store(false, Ordering::Release);
            *lock_phase(&state.phase) = QuitPhase::Idle;
            tray::set_status(
                &app,
                format!("⚠ Cannot start quit worker: {error} — Quit to retry"),
            );
        }
    }

    /// Returns only after the detector, webinar finishes, mic test, discard reapers and live writers
    /// have stopped and the pipeline drain has returned (or the pipeline thread is gone), so an `Err`
    /// is a settled failure.
    fn finish_for_quit(app: &tauri::AppHandle) -> std::result::Result<(), String> {
        let state = app.state::<Shutdown>();
        // Detector ack follows its terminal event; no app mutex is held across any pipeline wait.
        let detector_result = {
            let mut saved = state.detector_result.lock().unwrap();
            saved
                .get_or_insert_with(|| {
                    let detector = app.state::<DetectorHandle>();
                    detector
                        .0
                        .lock()
                        .unwrap()
                        .finish_and_shutdown()
                        .map_err(|error| format!("{error:#}"))
                })
                .clone()
        };
        let mut errors = Vec::new();
        if let Err(error) = detector_result {
            errors.push(format!("call capture could not finish: {error:#}"));
        }
        tray::set_status(
            app,
            "Finish writes before exiting… waiting for recordings".to_string(),
        );
        let webinar = app.state::<Webinar>();
        let (active, finishes) = loop {
            let mut w = webinar.lock();
            if w.pending_finishes == 0 {
                break (
                    w.recorder.take().map(|recorder| {
                        (
                            recorder,
                            w.started_at.take().unwrap_or_else(Local::now),
                            w.tx.clone(),
                        )
                    }),
                    std::mem::take(&mut w.finishing),
                );
            }
            drop(w);
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        if let Some((recorder, started_at, tx)) = active
            && let Err(error) = finish_webinar(app, recorder, started_at, tx)
        {
            errors.push(error);
        }
        for finish in finishes {
            match finish.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error),
                Err(_) => errors.push("webinar finish worker panicked".to_string()),
            }
        }
        if let Some(test) = app.try_state::<crate::live_test::LiveTestManager>() {
            test.stop_and_wait();
        }
        if let Err(error) = state.live.join_discards() {
            errors.push(error);
        }
        // Every producer has stopped. Drain even after a capture failure so the failure action is
        // never offered while an accepted recording is still transcribing or persisting.
        tray::set_status(
            app,
            "Finish writes before exiting… waiting for transcription".to_string(),
        );
        // A finish thread that no `Process` will collect (detector panic, dead pipeline) may still be
        // flushing its note. Wait before the marker so its late messages queue ahead of `Drain`; an
        // unprovable writer keeps Quit Draining instead of offering the acknowledgement.
        state.live.wait_for_live_writers();
        let (tx, rx) = mpsc::channel();
        let receipt = match state.pipe_tx.send(PipelineMsg::Drain(tx)) {
            Ok(()) => rx
                .recv()
                .unwrap_or_else(|_| Err("pipeline stopped without confirming writes".to_string())),
            Err(_) => Err("pipeline unavailable; verify recording queue".to_string()),
        };
        settle_quit(errors, receipt)
    }

    // The manual webinar's owning-app name (`corti_core::WEBINAR_NAME`) is the single signal `RecordingMeta::mode`
    // derives the webinar/call distinction from — kept in corti-core so the producer here and the consumer agree.

    /// Keep the nonblocking writer alive until the successful quit coordinator drops its guard.
    static LOG_GUARD: std::sync::OnceLock<
        Mutex<Option<tracing_appender::non_blocking::WorkerGuard>>,
    > = std::sync::OnceLock::new();

    pub fn run_app() -> Result<()> {
        // Install the global tracing subscriber FIRST so every later line (including config-load warnings
        // below) lands in the diagnostics console + on-disk log. `init_tracing` itself falls back to
        // `eprintln!` for any pre-subscriber failure (e.g. an unwritable log dir).
        let (console_buffer, log_guard) = console::init_tracing();
        if let Some(guard) = log_guard {
            let _ = LOG_GUARD.set(Mutex::new(Some(guard)));
        }

        let cfg = AppConfig::load();

        let app = tauri::Builder::default()
            // Global menu handler: catches events from the dynamically-rebuilt tray menu.
            .on_menu_event(|app, event| tray::handle_menu_event(app, &event))
            // Commands the Settings webview calls (own app commands; scoped to the settings window via
            // `app/capabilities/settings.json`).
            .invoke_handler(tauri::generate_handler![
                crate::settings::get_config,
                crate::settings::set_config,
                crate::settings::get_backends,
                crate::settings::get_aws_status,
                crate::settings::verify_aws,
                crate::settings::get_paths,
                crate::settings::reveal_path,
                crate::settings::set_models_dir,
                crate::settings::get_models_status,
                crate::settings::get_embedding_models,
                crate::settings::download_model,
                crate::console::get_console_logs,
                crate::console::get_console_logs_text,
                crate::console::save_console_logs,
                crate::stats::get_stats,
                crate::activity::get_pipeline_activity,
                crate::tray::open_preferences_section,
                crate::tray::take_preferences_section_request,
                crate::live_view::get_live_transcript,
                crate::postprocess_app::get_hosted_settings,
                crate::postprocess_app::patch_hosted_settings,
                crate::postprocess_app::update_hosted_steering,
                crate::postprocess_app::replace_hosted_word_bank,
                crate::postprocess_app::update_hosted_provider_scope,
                crate::postprocess_app::purge_hosted_reusable_cache,
                crate::postprocess_app::refresh_hosted_provider,
                crate::postprocess_app::submit_hosted_question,
                crate::postprocess_app::cancel_hosted_question,
                crate::postprocess_app::set_hosted_subscriptions,
                crate::postprocess_app::run_hosted_subscription_now,
                crate::postprocess_app::get_hosted_assistant,
                crate::postprocess_app::list_aws_credential_options,
                crate::postprocess_app::save_bedrock_setup,
                crate::postprocess_app::clear_bedrock_setup,
                crate::postprocess_app::set_bedrock_credential_mode,
                crate::postprocess_app::set_hosted_vertex_models,
                crate::postprocess_app::start_chatgpt_device_login,
                crate::postprocess_app::cancel_chatgpt_device_login,
                crate::postprocess_app::sign_out_chatgpt_subscription,
                crate::postprocess_app::open_chatgpt_device_login,
                crate::postprocess_app::prompt_for_provider_secret,
                crate::postprocess_app::clear_provider_secret,
                crate::live_test::get_live_test_window_generation,
                crate::live_test::start_live_test,
                crate::live_test::stop_live_test,
                crate::queue_ui::list_recordings,
                crate::queue_ui::get_recording_postprocess_history,
                crate::queue_ui::retry_recording,
                crate::queue_ui::open_note,
                crate::queue_ui::reveal_audio,
            ])
            .setup(move |app| {
                setup(app, &cfg, console_buffer.clone()).map_err(|e| {
                    Box::new(SetupError(format!("{e:#}"))) as Box<dyn std::error::Error>
                })
            })
            .build(tauri::generate_context!())
            .context("building the tauri app")?;

        // Window closes remain tray-only; ExitRequested does not identify whether code=None came
        // from closing a window or a keyboard Quit. Only an explicit drained tray Quit may exit.
        app.run(|app, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                let permit = app
                    .try_state::<Shutdown>()
                    .is_some_and(|state| state.permit_exit.load(Ordering::Acquire));
                if should_prevent_exit(code, permit) {
                    api.prevent_exit();
                }
            }
        });
        Ok(())
    }

    /// Implicit closes (including an indistinguishable Cmd-Q) remain tray-only. Only the
    /// coordinator's successful drain can allow the programmatic exit.
    fn should_prevent_exit(_code: Option<i32>, permit: bool) -> bool {
        !permit
    }

    /// All fallible startup wiring, in dependency order. Runs on the main thread (Tauri's setup hook).
    fn setup(
        app: &mut tauri::App,
        cfg: &AppConfig,
        console_buffer: console::ConsoleBuffer,
    ) -> Result<()> {
        // Menu-bar agent: no Dock icon, no app menu.
        app.set_activation_policy(tauri::ActivationPolicy::Accessory);

        // Managed state must exist before the tray (build_menu reads it) and before any worker touches it.
        app.manage(AppState::new());
        app.manage(crate::tray::PreferencesNavigation::default());

        // Transient, bounded timestamped transcript state. The same clone feeds call/test workers; managing
        // it exposes open-late snapshots to the singleton Live Transcript webview.
        let live_transcript = crate::live_view::LiveTranscriptStore::for_app(app.handle().clone());
        app.manage(live_transcript.clone());

        // The diagnostics console ring buffer the `get_console_logs*`/`save_console_logs` commands read.
        // It already backs the live `ConsoleLayer` (installed in `run_app`); managing it just exposes the
        // same shared buffer to the webview.
        app.manage(console_buffer);

        // Dedicated COUNT-capped stats ring (separate from the console log ring): the 1 Hz sampler
        // writes here and the get_stats command reads it. Clone first — manage() consumes one handle.
        let stats_buffer = crate::stats::StatsBuffer::new();
        app.manage(stats_buffer.clone());

        // Pipeline channel + shared runtime config. Both are created before the tray (its menu summary reads
        // the config via `ConfigState`) and before the worker (which holds the shared config to rebuild its
        // backend when the Settings screen saves).
        let (pipe_tx, pipe_rx) = std::sync::mpsc::channel::<PipelineMsg>();
        let quitting = Arc::new(AtomicBool::new(false));
        let shared_cfg: crate::settings::SharedConfig = Arc::new(Mutex::new(cfg.clone()));
        app.manage(crate::settings::ConfigState {
            config: shared_cfg.clone(),
            reload_tx: Mutex::new(pipe_tx.clone()),
        });

        // Hosted coordinator state is separate from AppConfig and starts fail-closed. It owns its bounded
        // control thread before any live/pipeline producer receives a handle.
        let (hosted_state, hosted) = crate::postprocess_app::start(
            app.handle().clone(),
            live_transcript.clone(),
            pipe_tx.clone(),
        )
        .context("starting hosted post-processing coordinator")?;
        app.manage(hosted_state);

        // Tray + blink (icons swap on the main thread).
        tray::build_tray(app.handle()).context("building tray")?;
        tray::spawn_blink(app.handle().clone());

        // Live-filing sessions (#87): the detector hook owns spawn/terminal verdict delivery; the
        // pipeline collects recording-scoped outcomes and owns durable fallback cleanup.
        let live_manager = Arc::new(crate::live::LiveManager::with_transcript_and_hosted(
            live_transcript.clone(),
            hosted.clone(),
        ));
        app.manage(crate::live_test::LiveTestManager::new(
            live_manager.clone(),
            shared_cfg.clone(),
            live_transcript.clone(),
            Some(hosted.clone()),
            quitting.clone(),
        ));
        app.manage(Shutdown {
            quitting: quitting.clone(),
            phase: Mutex::new(QuitPhase::Idle),
            permit_exit: AtomicBool::new(false),
            detector_result: Mutex::new(None),
            pipe_tx: pipe_tx.clone(),
            live: live_manager.clone(),
        });

        // Pipeline worker (sole Queue owner). Seeds tray history from the queue, recovers orphaned
        // background jobs, then drains recordings + due durable jobs (retry/sweep) serially (#85).
        {
            let handle = app.handle().clone();
            let shared_cfg = shared_cfg.clone();
            let stats = stats_buffer.clone();
            let live = live_manager.clone();
            let hosted = hosted.clone();
            let worker_quitting = quitting.clone();
            std::thread::Builder::new()
                .name("corti-pipeline".to_string())
                .spawn(move || {
                    pipeline::run(
                        handle,
                        shared_cfg,
                        pipe_rx,
                        stats,
                        live,
                        hosted,
                        worker_quitting,
                    )
                })
                .context("spawning pipeline worker")?;
        }

        // 1 Hz stats sampler on its OWN `corti-stats` thread — never on the pipeline thread (guardrail 9).
        // `shared_cfg` (the outer binding from setup) is still in scope here; the in-block shadow was
        // block-scoped to the pipeline `{ }` above.
        crate::stats::spawn_sampler(app.handle().clone(), shared_cfg.clone(), stats_buffer);

        // Manual "Webinar mode" handle: owns the live tap-only recorder + a clone of the pipeline channel.
        // Managed after the channel exists and before the detector closure consumes `pipe_tx`.
        app.manage(Webinar::new(pipe_tx.clone()));

        // The Recording Queue window's path to the pipeline thread (its Retry button is a message,
        // never a direct queue write).
        app.manage(crate::queue_ui::PipelineTx(Mutex::new(pipe_tx.clone())));

        // Detector: mic on/off → recordings. Its callback runs off the HAL thread (guardrail 9).
        // The live hook (#87) is consulted at every recording start; when live filing is eligible it
        // attaches a bounded tee and spawns the per-recording `corti-live` thread.
        let live_hook =
            crate::live::AppLiveHook::new(live_manager, shared_cfg.clone(), pipe_tx.clone());
        let handle = app.handle().clone();
        let detector = Detector::start_with_live_hook_and_quit_gate(
            move |event| handle_detector_event(&handle, &pipe_tx, event),
            Some(Box::new(live_hook)),
            quitting,
        )
        .context("starting detector")?;
        app.manage(DetectorHandle(Mutex::new(detector)));

        // Best-effort permission check (microphone via the plugin; system-audio via the Settings link).
        permissions::check_on_startup(app.handle().clone());

        Ok(())
    }

    /// React to a detector event. Tray/blink updates happen here (fast, non-blocking); the heavy
    /// transcription work is handed to the pipeline worker over the channel.
    fn handle_detector_event(
        app: &tauri::AppHandle,
        pipe_tx: &Sender<PipelineMsg>,
        event: DetectorEvent,
    ) {
        match event {
            DetectorEvent::RecordingStarted { meta } => {
                tracing::info!(
                    target: "corti::detector",
                    app = %meta.owning_app.name,
                    started_at = %meta.started_at,
                    path = %meta.audio_path.display(),
                    "recording started"
                );
                set_detector_recording(app, true);
                let id = corti_queue::job_id(&meta);
                if let Some(store) = app.try_state::<crate::live_view::LiveTranscriptStore>() {
                    store.ensure_unavailable_call(
                        &id,
                        &meta.owning_app.name,
                        "Live streaming is unavailable for this call. It requires live filing, the local backend, and all selected local models.".to_string(),
                    );
                }
                set_stage(app, Stage::Recording);
                tray::push_history_recording(app, &meta);
                tray::set_status(app, format!("● Recording — {}", meta.owning_app.name));
            }
            DetectorEvent::RecordingFinished {
                meta,
                audio_path,
                capture_processing,
            } => {
                set_detector_recording(app, false);
                // Bridge to the pipeline's own Transcribing set so the diagram doesn't sit on Recording
                // while the finished job waits in the channel.
                set_stage(app, Stage::Transcribing);
                tray::set_status(app, format!("Transcribing — {}…", meta.owning_app.name));
                if pipe_tx
                    .send(PipelineMsg::Process {
                        meta,
                        audio_path,
                        capture_processing,
                    })
                    .is_err()
                {
                    tracing::error!(target: "corti::detector", "pipeline worker gone; dropped a finished recording");
                    // Worker gone: nothing will transcribe, so don't leave the diagram on Transcribing.
                    set_stage(app, Stage::Idle);
                }
            }
            DetectorEvent::RecordingDiscarded { meta } => {
                set_detector_recording(app, false);
                set_stage(app, Stage::Idle);
                let id = corti_queue::job_id(&meta);
                // Resolve the orphaned `Recording` history row the RecordingStarted push created. JobStatus
                // has no "discarded" state, so mark it Failed with a clear reason. Also clears the stuck blink.
                tray::update_history(
                    app,
                    &id,
                    JobStatus::Failed,
                    None,
                    Some("Discarded — too short".to_string()),
                    None,
                );
                tray::set_status(app, "Discarded — too short".to_string());
                // #87: the detector's LiveHook already delivered this ID's discard verdict before
                // emitting the event. The later serial-pipeline message only closes a queue row
                // LiveNoteCreated may have made (and repeats manager discard idempotently).
                let _ = pipe_tx.send(PipelineMsg::LiveDiscarded { id });
            }
            DetectorEvent::Error(e) => {
                set_detector_recording(app, false);
                set_stage(app, Stage::Idle);
                tracing::error!(target: "corti::detector", error = %e, "detector error");
                // A capture failure is most often the missing audio-capture TCC grant (design/LESSONS §1).
                tray::set_status(app, format!("⚠ {e}"));
                // #87: no live teardown here — `Error` also fires for failures the recording
                // SURVIVES (e.g. a mic-monitor rebind), and killing the session then would delete a
                // live note mid-call. The one terminal case (capture failed to finish) reaches the
                // pipeline via the detector's `LiveHook::failed` instead.
            }
        }
    }

    fn set_detector_recording(app: &tauri::AppHandle, on: bool) {
        if let Some(state) = app.try_state::<AppState>() {
            state.detector_recording.store(on, Ordering::Relaxed);
        }
    }

    /// Set the current pipeline [`Stage`] (read by `get_pipeline_activity`). Tracks the tray status line:
    /// set wherever the status changes phase.
    pub(crate) fn set_stage(app: &tauri::AppHandle, stage: Stage) {
        if let Some(state) = app.try_state::<AppState>() {
            state.stage.store(stage as u8, Ordering::Relaxed);
        }
    }

    fn set_webinar_recording(app: &tauri::AppHandle, on: bool) {
        if let Some(state) = app.try_state::<AppState>() {
            state.webinar_recording.store(on, Ordering::Relaxed);
        }
    }

    pub(crate) fn set_live_test_active(app: &tauri::AppHandle, on: bool) {
        if let Some(state) = app.try_state::<AppState>() {
            state.live_test_active.store(on, Ordering::Relaxed);
        }
        tray::refresh_menu(app);
    }

    pub(crate) fn live_test_active(app: &tauri::AppHandle) -> bool {
        app.try_state::<AppState>()
            .map(|state| state.live_test_active.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub(crate) fn transcription_active(app: &tauri::AppHandle) -> bool {
        app.try_state::<AppState>()
            .is_some_and(|state| state.stage() == Stage::Transcribing)
    }

    /// Whether the detector (mic-triggered) capture is currently running — used to exclude manual modes.
    pub(crate) fn detector_recording(app: &tauri::AppHandle) -> bool {
        app.try_state::<AppState>()
            .map(|s| s.detector_recording.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Whether a manual webinar recording is currently in flight — drives the tray toggle's label.
    pub(crate) fn webinar_active(app: &tauri::AppHandle) -> bool {
        app.try_state::<Webinar>()
            .map(|w| w.lock().recorder.is_some())
            .unwrap_or(false)
    }

    /// Pause detector edges only after its worker confirms no real recording is in flight.
    pub(crate) fn pause_detector(app: &tauri::AppHandle) -> Result<()> {
        let detector = app
            .try_state::<DetectorHandle>()
            .context("detector is unavailable")?;
        if detector.0.lock().unwrap().pause()? {
            Ok(())
        } else {
            anyhow::bail!("a detected call is already recording")
        }
    }

    pub(crate) fn resume_detector(app: &tauri::AppHandle) {
        if let Some(detector) = app.try_state::<DetectorHandle>()
            && let Err(error) = detector.0.lock().unwrap().resume()
        {
            tracing::warn!(
                target: "corti::live_test",
                error = %format!("{error:#}"),
                "could not resume call detection after microphone test"
            );
        }
    }

    /// Start or stop a manual tap-only "webinar" recording from the tray. Invoked on the main thread (the
    /// menu-event handler). The `Webinar` lock is only held for the brief state read/swap and is always
    /// dropped before any tray update (`build_menu` re-locks it; `std::sync::Mutex` is not reentrant) and,
    /// crucially, before the fallible `Recorder::start_tap_only` so a panic there can't poison it. Starting
    /// a capture (creating the tap + aggregate) is fast and runs inline; only the WAV write on *stop* —
    /// which can be large for a long session — is moved to a worker thread.
    pub fn toggle(app: &tauri::AppHandle) {
        let Some(state) = app.try_state::<Webinar>() else {
            return;
        };

        /// What `toggle` decided to do, computed under the lock and acted on after it's dropped.
        enum Next {
            Stopping {
                // Only the cold stop handoff owns this larger native capture state.
                recorder: Box<Recorder>,
                started_at: DateTime<Local>,
                tx: Sender<PipelineMsg>,
            },
            /// No webinar running and no call in flight: start one (outside the lock).
            StartRequested,
            Busy,
            Quitting,
        }

        let next = {
            let mut w = state.lock();
            if is_quitting(app) {
                Next::Quitting
            } else if let Some(recorder) = w.recorder.take() {
                // Reserve the finish under the lock so Quit cannot overtake its handoff.
                w.pending_finishes += 1;
                let started_at = w.started_at.take().unwrap_or_else(chrono::Local::now);
                Next::Stopping {
                    recorder: Box::new(recorder),
                    started_at,
                    tx: w.tx.clone(),
                }
            } else if detector_recording(app) || live_test_active(app) {
                // A detected call or explicit mic test already owns capture; refuse to double-capture.
                Next::Busy
            } else {
                Next::StartRequested
            }
        };

        // Lock released. Tray updates (which rebuild the menu, re-reading `webinar_active`) happen here.
        match next {
            Next::Stopping {
                recorder,
                started_at,
                tx,
            } => {
                set_webinar_recording(app, false);
                set_stage(app, Stage::Transcribing);
                tray::set_status(app, format!("Transcribing — {WEBINAR_NAME}…"));
                let worker_app = app.clone();
                let worker_started = started_at;
                let spawned = spawn_owned(
                    recorder,
                    move |recorder| finish_webinar(&worker_app, *recorder, worker_started, tx),
                    |task| {
                        std::thread::Builder::new()
                            .name("corti-webinar-finish".to_string())
                            .spawn(task)
                    },
                );
                let mut w = state.lock();
                match spawned {
                    Ok(handle) => w.finishing.push(handle),
                    Err((error, recorder)) => {
                        w.recorder = Some(*recorder);
                        w.started_at = Some(started_at);
                        tracing::error!(target: "corti::shutdown", %error, "could not spawn webinar finish worker; capture retained");
                    }
                }
                w.pending_finishes -= 1;
                let failed = w.recorder.is_some();
                drop(w);
                if failed {
                    set_webinar_recording(app, true);
                    set_stage(app, Stage::Recording);
                    tray::set_status(
                        app,
                        "⚠ Webinar finish unavailable; capture retained — Quit to retry"
                            .to_string(),
                    );
                }
            }
            Next::Quitting => {}
            Next::Busy => tray::set_status(
                app,
                "Can't start webinar — the microphone is already in use by Corti".to_string(),
            ),
            Next::StartRequested => {
                // Start the capture OUTSIDE the lock: a panic in CoreAudio FFI here won't poison the
                // `Webinar` mutex. The menu event loop is single-threaded, so no second toggle can race in.
                let owner = OwningApp {
                    bundle_id: None,
                    name: WEBINAR_NAME.to_string(),
                };
                match Recorder::start_tap_only(&owner, None) {
                    Ok(recorder) => {
                        // A `Recording` history entry, keyed by the same id the queue will assign once the
                        // finished webinar is enqueued (`job_id` = recorder output stem), so the worker's
                        // later `update_history` calls land on this same row.
                        let started_at = chrono::Local::now();
                        let meta = RecordingMeta {
                            started_at,
                            ended_at: None,
                            owning_app: owner.clone(),
                            audio_path: recorder.output_path().to_path_buf(),
                        };
                        {
                            let mut w = state.lock();
                            w.recorder = Some(recorder);
                            w.started_at = Some(started_at);
                        }
                        tray::push_history_recording(app, &meta);
                        set_webinar_recording(app, true);
                        set_stage(app, Stage::Recording);
                        tray::set_status(app, format!("● Webinar recording — {WEBINAR_NAME}"));
                    }
                    Err(e) => {
                        tracing::error!(target: "corti::detector", error = %format!("{e:#}"), "webinar capture failed to start");
                        // Most often the missing audio-capture TCC grant (design/LESSONS §1).
                        tray::set_status(app, format!("⚠ webinar capture failed: {e:#}"));
                    }
                }
            }
        }
    }

    /// Off-thread tail of a webinar stop: write the tap-only WAV, then hand it to the pipeline worker so it
    /// runs the same enqueue → transcribe → file → Done path as a detected call.
    fn finish_webinar(
        app: &tauri::AppHandle,
        recorder: Recorder,
        started_at: DateTime<Local>,
        tx: Sender<PipelineMsg>,
    ) -> std::result::Result<(), String> {
        // `webinar_recording` was already cleared by the toggle's Stopping branch before this thread spawned.
        let finished = match recorder.finish_tap_only_with_processing() {
            Ok(finished) => finished,
            Err(e) => {
                tracing::error!(target: "corti::detector", error = %format!("{e:#}"), "webinar capture produced no audio");
                // The toggle already bridged the stage to Transcribing; nothing will transcribe now.
                set_stage(app, Stage::Idle);
                tray::set_status(app, format!("⚠ webinar capture failed: {e:#}"));
                return Err(format!("webinar capture could not finish: {e:#}"));
            }
        };
        let meta = RecordingMeta {
            started_at,
            ended_at: Some(chrono::Local::now()),
            owning_app: OwningApp {
                bundle_id: None,
                name: WEBINAR_NAME.to_string(),
            },
            audio_path: finished.path.clone(),
        };
        if tx
            .send(PipelineMsg::Process {
                meta,
                audio_path: finished.path,
                capture_processing: Box::new(finished.processing),
            })
            .is_err()
        {
            tracing::error!(target: "corti::detector", "pipeline worker gone; dropped a finished webinar recording");
            set_stage(app, Stage::Idle);
            return Err("pipeline unavailable after webinar capture".to_string());
        }
        Ok(())
    }

    /// A plain `std::error::Error` so the anyhow setup error coerces cleanly into the `Box<dyn Error>` the
    /// Tauri setup hook expects (anyhow::Error itself does not implement `std::error::Error`).
    #[derive(Debug)]
    struct SetupError(String);
    impl std::fmt::Display for SetupError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for SetupError {}

    #[cfg(test)]
    mod lifecycle_tests {
        use super::{
            QuitPhase, claim_failure_ack, claim_quit, settle_quit, should_prevent_exit,
            spawn_owned, take_finished,
        };
        use std::sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        };

        #[test]
        fn implicit_close_and_unpermitted_exit_are_vetoed() {
            assert!(should_prevent_exit(None, false));
            assert!(should_prevent_exit(Some(0), false));
            assert!(should_prevent_exit(Some(1), false));
            assert!(!should_prevent_exit(Some(0), true));
        }

        #[test]
        fn failed_webinar_finish_spawn_retains_capture_ownership() {
            let result = spawn_owned(
                "owned recording".to_string(),
                |value| value,
                |_task| Err(std::io::Error::other("synthetic thread exhaustion")),
            );
            let (error, recorder) = result.err().unwrap();
            assert!(error.to_string().contains("thread exhaustion"));
            assert_eq!(recorder, "owned recording");
        }

        #[test]
        fn repeated_quit_does_not_restart_or_force_exit() {
            let phase = Mutex::new(QuitPhase::Idle);
            let quitting = AtomicBool::new(false);
            assert!(
                !claim_failure_ack(&phase),
                "no failure action before a Quit"
            );
            assert!(claim_quit(&phase, &quitting));
            assert!(!claim_quit(&phase, &quitting));
            assert!(quitting.load(Ordering::Acquire));
            assert!(
                !claim_failure_ack(&phase),
                "draining never exposes the failure action"
            );
            assert!(should_prevent_exit(Some(0), false));
            // A coordinator panic leaves quiescence unproven: only a full ordinary retry is allowed.
            *phase.lock().unwrap() = QuitPhase::Idle;
            assert!(!claim_failure_ack(&phase));
            assert!(claim_quit(&phase, &quitting));
        }

        #[test]
        fn stale_ordinary_quit_after_failure_does_not_acknowledge() {
            let phase = Mutex::new(QuitPhase::Failed);
            let quitting = AtomicBool::new(true);
            assert!(
                !claim_quit(&phase, &quitting),
                "a queued ordinary Quit is ignored"
            );
            assert_eq!(*phase.lock().unwrap(), QuitPhase::Failed);
            assert!(claim_failure_ack(&phase), "only the distinct action exits");
            assert!(
                should_prevent_exit(None, false),
                "window close stays tray-only"
            );
        }

        #[test]
        fn producer_and_drain_failures_settle_together() {
            assert_eq!(settle_quit(Vec::new(), Ok(())), Ok(()));
            assert_eq!(
                settle_quit(vec!["webinar capture could not finish".into()], Ok(())),
                Err("webinar capture could not finish".to_string())
            );
            assert_eq!(
                settle_quit(
                    vec!["call capture could not finish".into()],
                    Err("queue write failed".into())
                ),
                Err("call capture could not finish; queue write failed".to_string())
            );
        }

        #[test]
        fn finishes_returned_before_quit_do_not_count_against_it() {
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let historical = std::thread::spawn(|| Err::<(), String>("no audio captured".into()));
            let running = std::thread::spawn(move || {
                let _ = release_rx.recv();
                Ok::<(), String>(())
            });
            while !historical.is_finished() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let mut handles = vec![historical, running];
            let finished = take_finished(&mut handles);
            assert!(
                matches!(finished.as_slice(), [Ok(Err(error))] if error == "no audio captured")
            );
            assert_eq!(
                handles.len(),
                1,
                "a finish still writing at the gate stays counted"
            );
            release_tx.send(()).unwrap();
            assert!(handles.pop().unwrap().join().unwrap().is_ok());
        }
    }
}
