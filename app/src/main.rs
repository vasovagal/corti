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
//!   not `Sync`). It resumes crashed jobs on startup, then runs each recording through the pipeline serially
//!   on its own thread (transcription blocks; guardrail 9 keeps it off the UI loop).
//! - **Blink thread** toggles two template icons ~every 500 ms, marshalling AppKit calls to the main thread.

// macOS-only by design — like the rest of the workspace, this compiles to a stub elsewhere.
#[cfg(target_os = "macos")]
mod cli;
#[cfg(target_os = "macos")]
mod config;
#[cfg(target_os = "macos")]
mod console;
#[cfg(target_os = "macos")]
mod permissions;
#[cfg(target_os = "macos")]
mod pipeline;
#[cfg(target_os = "macos")]
mod settings;
#[cfg(target_os = "macos")]
mod transcribe;
#[cfg(target_os = "macos")]
mod tray;

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
mod imp {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::{Arc, Mutex};

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

    /// Shared app state (managed singleton). Everything here is read/written from background threads, so it
    /// is `Send + Sync`; the tray rebuilds its menu from this snapshot whenever it changes.
    pub struct AppState {
        /// Whether the detector (mic-triggered) capture is in flight.
        pub detector_recording: AtomicBool,
        /// Whether a manual webinar (tap-only) capture is in flight. Kept separate from
        /// [`detector_recording`] so the two independent sources never clobber each other's blink/guard
        /// state; the icon blinks while *either* is true.
        pub webinar_recording: AtomicBool,
        /// The status line shown at the top of the tray menu.
        pub status: Mutex<String>,
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
                status: Mutex::new("Starting…".to_string()),
                history: Mutex::new(VecDeque::new()),
            }
        }
    }

    /// Keeps the [`Detector`] alive for the app's lifetime. `Detector` is `Send` but not `Sync`, so the
    /// `Mutex` makes the managed holder `Send + Sync`. We never lock it — it exists only to own the detector
    /// (whose `Drop` stops the worker + removes HAL listeners).
    struct DetectorHandle(#[allow(dead_code)] Mutex<Detector>);

    /// Manual "Webinar mode": a live tap-only [`Recorder`] driven by the tray toggle, plus a clone of the
    /// channel to the pipeline worker so a finished webinar enters the same transcribe → file path as a
    /// detected call. The `Mutex` makes the handle `Send + Sync` (both `Recorder` and `Sender` are `Send`
    /// but not `Sync`).
    struct Webinar(Mutex<WebinarState>);

    struct WebinarState {
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

    // The manual webinar's owning-app name (`corti_core::WEBINAR_NAME`) is the single signal `RecordingMeta::mode`
    // derives the webinar/call distinction from — kept in corti-core so the producer here and the consumer agree.

    /// Holds the file-appender's non-blocking [`WorkerGuard`] for the process lifetime. Dropping the guard
    /// would stop the background writer thread and lose any buffered log lines, so it lives in this static
    /// (set once in [`run_app`]) and is never dropped while the app runs.
    static LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
        std::sync::OnceLock::new();

    pub fn run_app() -> Result<()> {
        // Install the global tracing subscriber FIRST so every later line (including config-load warnings
        // below) lands in the diagnostics console + on-disk log. `init_tracing` itself falls back to
        // `eprintln!` for any pre-subscriber failure (e.g. an unwritable log dir).
        let (console_buffer, log_guard) = console::init_tracing();
        if let Some(guard) = log_guard {
            // Keep the non-blocking writer's worker thread alive for the whole process.
            let _ = LOG_GUARD.set(guard);
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
            ])
            .setup(move |app| {
                setup(app, &cfg, console_buffer.clone()).map_err(|e| {
                    Box::new(SetupError(format!("{e:#}"))) as Box<dyn std::error::Error>
                })
            })
            .build(tauri::generate_context!())
            .context("building the tauri app")?;

        // Keep the (windowless) event loop alive. Exit only ever comes from the tray's Quit item
        // (`app_handle.exit(0)`), so there is nothing to veto here.
        app.run(|_app, _event| {});
        Ok(())
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

        // The diagnostics console ring buffer the `get_console_logs*`/`save_console_logs` commands read.
        // It already backs the live `ConsoleLayer` (installed in `run_app`); managing it just exposes the
        // same shared buffer to the webview.
        app.manage(console_buffer);

        // Pipeline channel + shared runtime config. Both are created before the tray (its menu summary reads
        // the config via `ConfigState`) and before the worker (which holds the shared config to rebuild its
        // backend when the Settings screen saves).
        let (pipe_tx, pipe_rx) = std::sync::mpsc::channel::<PipelineMsg>();
        let shared_cfg: crate::settings::SharedConfig = Arc::new(Mutex::new(cfg.clone()));
        app.manage(crate::settings::ConfigState {
            config: shared_cfg.clone(),
            reload_tx: Mutex::new(pipe_tx.clone()),
        });

        // Tray + blink (icons swap on the main thread).
        tray::build_tray(app.handle()).context("building tray")?;
        tray::spawn_blink(app.handle().clone());

        // Pipeline worker (sole Queue owner). It resumes crashed jobs, then drains the channel serially.
        {
            let handle = app.handle().clone();
            let shared_cfg = shared_cfg.clone();
            std::thread::Builder::new()
                .name("corti-pipeline".to_string())
                .spawn(move || pipeline::run(handle, shared_cfg, pipe_rx))
                .context("spawning pipeline worker")?;
        }

        // Manual "Webinar mode" handle: owns the live tap-only recorder + a clone of the pipeline channel.
        // Managed after the channel exists and before the detector closure consumes `pipe_tx`.
        app.manage(Webinar::new(pipe_tx.clone()));

        // Detector: mic on/off → recordings. Its callback runs off the HAL thread (guardrail 9).
        let handle = app.handle().clone();
        let detector =
            Detector::start(move |event| handle_detector_event(&handle, &pipe_tx, event))
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
                tray::push_history_recording(app, &meta);
                tray::set_status(app, format!("● Recording — {}", meta.owning_app.name));
            }
            DetectorEvent::RecordingFinished { meta, audio_path } => {
                set_detector_recording(app, false);
                tray::set_status(app, format!("Transcribing — {}…", meta.owning_app.name));
                if pipe_tx
                    .send(PipelineMsg::Process { meta, audio_path })
                    .is_err()
                {
                    tracing::error!(target: "corti::detector", "pipeline worker gone; dropped a finished recording");
                }
            }
            DetectorEvent::Error(e) => {
                set_detector_recording(app, false);
                tracing::error!(target: "corti::detector", error = %e, "detector error");
                // A capture failure is most often the missing audio-capture TCC grant (design/LESSONS §1).
                tray::set_status(app, format!("⚠ {e}"));
            }
        }
    }

    fn set_detector_recording(app: &tauri::AppHandle, on: bool) {
        if let Some(state) = app.try_state::<AppState>() {
            state.detector_recording.store(on, Ordering::Relaxed);
        }
    }

    fn set_webinar_recording(app: &tauri::AppHandle, on: bool) {
        if let Some(state) = app.try_state::<AppState>() {
            state.webinar_recording.store(on, Ordering::Relaxed);
        }
    }

    /// Whether the detector (mic-triggered) capture is currently running — used to refuse a webinar start.
    fn detector_recording(app: &tauri::AppHandle) -> bool {
        app.try_state::<AppState>()
            .map(|s| s.detector_recording.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// Whether a manual webinar recording is currently in flight — drives the tray toggle's label.
    pub fn webinar_active(app: &tauri::AppHandle) -> bool {
        app.try_state::<Webinar>()
            .map(|w| w.lock().recorder.is_some())
            .unwrap_or(false)
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
                recorder: Recorder,
                started_at: DateTime<Local>,
                tx: Sender<PipelineMsg>,
            },
            /// No webinar running and no call in flight: start one (outside the lock).
            StartRequested,
            Busy,
        }

        let next = {
            let mut w = state.lock();
            if let Some(recorder) = w.recorder.take() {
                // Stop: hand the recorder off to a worker thread (finish writes the whole WAV).
                let started_at = w.started_at.take().unwrap_or_else(chrono::Local::now);
                Next::Stopping {
                    recorder,
                    started_at,
                    tx: w.tx.clone(),
                }
            } else if detector_recording(app) {
                // A detected call is already recording; refuse to double-capture.
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
                tray::set_status(app, format!("Transcribing — {WEBINAR_NAME}…"));
                let app = app.clone();
                std::thread::Builder::new()
                    .name("corti-webinar-finish".to_string())
                    .spawn(move || finish_webinar(&app, recorder, started_at, tx))
                    .expect("spawning webinar-finish thread");
            }
            Next::Busy => tray::set_status(
                app,
                "Can't start webinar — a call is already recording".to_string(),
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
    ) {
        // `webinar_recording` was already cleared by the toggle's Stopping branch before this thread spawned.
        let audio_path = match recorder.finish_tap_only() {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(target: "corti::detector", error = %format!("{e:#}"), "webinar capture produced no audio");
                tray::set_status(app, format!("⚠ webinar capture failed: {e:#}"));
                return;
            }
        };
        let meta = RecordingMeta {
            started_at,
            ended_at: Some(chrono::Local::now()),
            owning_app: OwningApp {
                bundle_id: None,
                name: WEBINAR_NAME.to_string(),
            },
            audio_path: audio_path.clone(),
        };
        if tx.send(PipelineMsg::Process { meta, audio_path }).is_err() {
            tracing::error!(target: "corti::detector", "pipeline worker gone; dropped a finished webinar recording");
        }
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
}
