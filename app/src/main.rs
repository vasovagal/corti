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
mod jobs;
#[cfg(target_os = "macos")]
mod keychain;
#[cfg(target_os = "macos")]
mod live;
#[cfg(target_os = "macos")]
mod live_test;
#[cfg(target_os = "macos")]
mod live_view;
#[cfg(target_os = "macos")]
mod offline_trace;
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
            // Headless path: compose stderr diagnostics with the independent optional JSONL layer. Close
            // the root span and drain both writer guards before `process::exit`, which runs no destructors.
            let command = other.trace_command();
            let guards = console::init_cli_tracing();
            let trace = offline_trace::cli(command);
            let code = trace.in_scope(|| cli::dispatch(other, &trace));
            if code == 0 {
                trace.ok();
            } else {
                trace.error(offline_trace::ErrorCode::Other);
            }
            drop(trace);
            guards.shutdown();
            std::process::exit(code)
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

    pub fn run_app() -> Result<()> {
        // Install the composed subscriber FIRST so every later diagnostics line is preserved. The returned
        // diagnostics/offline guards remain local across `app.run`, then drain in a defined order.
        let (console_buffer, tracing_guards) = console::init_tracing();

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
                crate::live_view::get_live_transcript,
                crate::postprocess_app::get_hosted_settings,
                crate::postprocess_app::patch_hosted_settings,
                crate::postprocess_app::update_hosted_steering,
                crate::postprocess_app::replace_hosted_word_bank,
                crate::postprocess_app::update_hosted_provider_scope,
                crate::postprocess_app::refresh_hosted_provider,
                crate::postprocess_app::submit_hosted_question,
                crate::postprocess_app::cancel_hosted_question,
                crate::postprocess_app::set_hosted_pinned_question,
                crate::postprocess_app::get_hosted_assistant,
                crate::postprocess_app::list_aws_credential_options,
                crate::postprocess_app::set_bedrock_credential_mode,
                crate::postprocess_app::prompt_for_provider_secret,
                crate::postprocess_app::clear_provider_secret,
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

        // Tray-agent policy: closing the final utility window (and other user-driven/Cmd-Q requests) emits
        // `ExitRequested { code: None }`; veto it so Corti returns to menu-bar-only mode. The tray's explicit
        // `app.exit(0)` carries `Some(0)` and is still allowed to terminate the process.
        app.run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event
                && should_prevent_implicit_exit(code)
            {
                api.prevent_exit();
            }
        });
        // `App::run` consumes and drops managed state before returning; now stop trace admission and drain
        // both writer guards.
        tracing_guards.shutdown();
        Ok(())
    }

    /// Menu-bar agents ignore user-driven implicit exits; only explicit programmatic exit codes terminate.
    fn should_prevent_implicit_exit(code: Option<i32>) -> bool {
        code.is_none()
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
        ));

        // Pipeline worker (sole Queue owner). Seeds tray history from the queue, recovers orphaned
        // background jobs, then drains recordings + due durable jobs (retry/sweep) serially (#85).
        {
            let handle = app.handle().clone();
            let shared_cfg = shared_cfg.clone();
            let stats = stats_buffer.clone();
            let live = live_manager.clone();
            let hosted = hosted.clone();
            let dispatch = crate::offline_trace::Dispatch::capture();
            std::thread::Builder::new()
                .name("corti-pipeline".to_string())
                .spawn(move || {
                    dispatch.with_default(|| {
                        pipeline::run(handle, shared_cfg, pipe_rx, stats, live, hosted)
                    })
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
        let detector = Detector::start_with_live_hook(
            move |event| handle_detector_event(&handle, &pipe_tx, event),
            Some(Box::new(live_hook)),
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
                let app = app.clone();
                std::thread::Builder::new()
                    .name("corti-webinar-finish".to_string())
                    .spawn(move || finish_webinar(&app, recorder, started_at, tx))
                    .expect("spawning webinar-finish thread");
            }
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
    ) {
        // `webinar_recording` was already cleared by the toggle's Stopping branch before this thread spawned.
        let finished = match recorder.finish_tap_only_with_processing() {
            Ok(finished) => finished,
            Err(e) => {
                tracing::error!(target: "corti::detector", error = %format!("{e:#}"), "webinar capture produced no audio");
                // The toggle already bridged the stage to Transcribing; nothing will transcribe now.
                set_stage(app, Stage::Idle);
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
            audio_path: finished.path.clone(),
        };
        if tx
            .send(PipelineMsg::Process {
                meta,
                audio_path: finished.path,
                capture_processing: finished.processing,
            })
            .is_err()
        {
            tracing::error!(target: "corti::detector", "pipeline worker gone; dropped a finished webinar recording");
            set_stage(app, Stage::Idle);
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

    #[cfg(test)]
    mod lifecycle_tests {
        use super::should_prevent_implicit_exit;

        #[test]
        fn implicit_exit_is_vetoed_but_tray_exit_is_allowed() {
            assert!(should_prevent_implicit_exit(None));
            assert!(!should_prevent_implicit_exit(Some(0)));
            assert!(!should_prevent_implicit_exit(Some(1)));
        }
    }
}
