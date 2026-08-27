//! The menu-bar tray: a blinking record icon, a status line, recent notes, settings, and quit.
//!
//! AppKit menu/status-item mutations must happen on the main thread, so every tray change here marshals
//! through [`AppHandle::run_on_main_thread`]. The menu is **rebuilt from [`AppState`]** on each change (the
//! status string + recent-notes list are the source of truth) and swapped in via `TrayIcon::set_menu` —
//! simpler and flicker-free since it's a dropdown the user only sees on click.

use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Local};
use corti_core::{JobStatus, RecordingMode};
use tauri::image::Image;
use tauri::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{
    ActivationPolicy, AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
    Wry,
};

use crate::imp::{AppState, HISTORY_LIMIT, HistoryEntry};
use crate::permissions::PRIVACY_SCREEN_CAPTURE;

const TRAY_ID: &str = "corti-tray";
pub(crate) const SETTINGS_NAVIGATION_REQUESTED_EVENT: &str = "settings-navigation-requested";

/// Backend-owned latest-wins handoff. The frontend event is only a wake-up; no webview-provided payload can
/// choose a Preferences destination, and a newly loading Settings window can take the pending value on mount.
#[derive(Default)]
pub(crate) struct PreferencesNavigation {
    pending: Mutex<Option<String>>,
}

impl PreferencesNavigation {
    fn request(&self, section: &str) {
        *self.pending.lock().unwrap() = Some(section.to_string());
    }

    fn take(&self) -> Option<String> {
        self.pending.lock().unwrap().take()
    }
}

fn preferences_section(section: &str) -> Option<&'static str> {
    match section {
        "transcription" => Some("transcription"),
        "hosted" => Some("hosted"),
        "hosted-provider" => Some("hosted-provider"),
        "hosted-routing" => Some("hosted-routing"),
        "hosted-language" => Some("hosted-language"),
        "hosted-advanced" => Some("hosted-advanced"),
        "storage" => Some("storage"),
        _ => None,
    }
}

/// Menu-bar template icons, embedded at build time as raw RGBA (no `image-*` feature needed). Monochrome
/// black + alpha; `icon_as_template(true)` lets macOS tint them for light/dark mode.
const ICON_IDLE: Image<'static> = tauri::include_image!("icons/tray-idle.png");
const ICON_REC: Image<'static> = tauri::include_image!("icons/tray-rec.png");

/// Menu item id prefix that encodes a recent note's path (`note::/path/to/note.md`).
const NOTE_PREFIX: &str = "note::";

/// Build the tray icon + initial menu. Called from `setup` (already on the main thread).
pub fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let menu = build_menu(app)?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(ICON_IDLE)
        .icon_as_template(true)
        .tooltip("Corti")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .build(app)?;
    Ok(())
}

/// Construct the current menu from [`AppState`]. Must run on the main thread.
fn build_menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let state = app.state::<AppState>();

    let status_text = state.status.lock().unwrap().clone();
    let mut items: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::new();
    items.push(Box::new(MenuItem::with_id(
        app,
        "status",
        status_text,
        false,
        None::<&str>,
    )?));
    items.push(Box::new(PredefinedMenuItem::separator(app)?));

    let detector_live = state.detector_recording.load(Ordering::Relaxed);
    let test_live = state.live_test_active.load(Ordering::Relaxed);
    let webinar_live = crate::imp::webinar_active(app);
    let (live_label, live_enabled) =
        live_transcript_menu_state(detector_live, test_live, webinar_live);
    items.push(Box::new(MenuItem::with_id(
        app,
        "live_transcript",
        live_label,
        live_enabled,
        None::<&str>,
    )?));
    items.push(Box::new(PredefinedMenuItem::separator(app)?));

    // Manual "Webinar mode" toggle (tap-only, mic never opened). The label reflects whether a webinar is
    // currently recording.
    let webinar_label = if crate::imp::webinar_active(app) {
        "■ Stop webinar recording"
    } else {
        "▶ Start webinar recording"
    };
    items.push(Box::new(MenuItem::with_id(
        app,
        "webinar_toggle",
        webinar_label,
        true,
        None::<&str>,
    )?));
    items.push(Box::new(PredefinedMenuItem::separator(app)?));

    // History submenu: the most recent recordings (newest first), each a compact one-line entry showing
    // relative time, duration, and live transcription state. A filed entry (has a note) is clickable and
    // opens its note; in-flight/failed entries are shown but disabled (issue #3).
    {
        let history = state.history.lock().unwrap();
        if !history.is_empty() {
            let now = Local::now();
            let mut sub_items: Vec<Box<dyn IsMenuItem<Wry>>> = Vec::with_capacity(history.len());
            for entry in history.iter() {
                let (menu_id, enabled) = match &entry.note_path {
                    Some(path) => (note_menu_id(path), true),
                    None => (format!("history::{}", entry.id), false),
                };
                sub_items.push(Box::new(MenuItem::with_id(
                    app,
                    menu_id,
                    history_entry_label(entry, now),
                    enabled,
                    None::<&str>,
                )?));
            }
            let refs: Vec<&dyn IsMenuItem<Wry>> = sub_items.iter().map(|b| &**b).collect();
            let history_menu = Submenu::with_items(app, "History", true, &refs)?;
            items.push(Box::new(history_menu));
            items.push(Box::new(PredefinedMenuItem::separator(app)?));
        }
    }

    // Preferences section. The Backend:/Bucket: lines are a read-only summary derived live from the current
    // config (so they reflect saved edits); the Preferences window itself is the editor.
    let (backend_label, bucket_label) = settings_summary(app);
    items.push(Box::new(MenuItem::with_id(
        app,
        "backend",
        format!("Backend: {backend_label}"),
        false,
        None::<&str>,
    )?));
    if let Some(bucket) = bucket_label {
        items.push(Box::new(MenuItem::with_id(
            app,
            "bucket",
            format!("Bucket: {bucket}"),
            false,
            None::<&str>,
        )?));
    }
    items.push(Box::new(MenuItem::with_id(
        app,
        "open_queue",
        "Recording Queue…",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "open_settings",
        "Preferences…",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "ethics_guide",
        "Ethics & Legality Guide…",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "open_how",
        "How Corti Works…",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "open_diagnostics",
        "Diagnostics…",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "open_privacy",
        "Open Privacy Settings…",
        true,
        None::<&str>,
    )?));
    items.push(Box::new(PredefinedMenuItem::separator(app)?));
    items.push(Box::new(MenuItem::with_id(
        app,
        "quit",
        "Quit Corti",
        true,
        None::<&str>,
    )?));

    let refs: Vec<&dyn IsMenuItem<Wry>> = items.iter().map(|b| &**b).collect();
    Menu::with_items(app, &refs)
}

fn live_transcript_menu_state(
    detector_live: bool,
    test_live: bool,
    webinar_live: bool,
) -> (&'static str, bool) {
    if detector_live {
        ("Read live transcript…", true)
    } else if test_live {
        ("Read microphone test transcript…", true)
    } else if webinar_live {
        ("Test microphone & live transcription…", false)
    } else {
        ("Test microphone & live transcription…", true)
    }
}

fn handle_live_transcript_action(app: &AppHandle) {
    let manager = app.try_state::<crate::live_test::LiveTestManager>();
    let window_generation = manager.as_ref().map(|manager| manager.begin_live_window());
    let (detector_live, test_live) = app
        .try_state::<AppState>()
        .map(|state| {
            (
                state.detector_recording.load(Ordering::Relaxed),
                state.live_test_active.load(Ordering::Relaxed),
            )
        })
        .unwrap_or((false, false));
    if !detector_live && !test_live {
        let result = manager
            .as_ref()
            .zip(window_generation)
            .ok_or_else(|| anyhow::anyhow!("microphone-test manager is unavailable"))
            .and_then(|(manager, generation)| manager.start_for_window(app, generation));
        if let Err(error) = result {
            let detail = format!("Could not start microphone test: {error:#}");
            // Do not overwrite a call that won the race after the tray menu snapshot was built.
            if !crate::imp::detector_recording(app)
                && let Some(store) = app.try_state::<crate::live_view::LiveTranscriptStore>()
            {
                store.show_test_error(&detail);
            }
            set_status(app, format!("⚠ {detail}"));
        }
    }
    open_live_transcript_window(app, window_generation);
}

/// Update the status line and rebuild the menu.
pub fn set_status(app: &AppHandle, text: String) {
    if let Some(state) = app.try_state::<AppState>() {
        *state.status.lock().unwrap() = text;
    }
    refresh_menu(app);
}

/// Push a newly-started recording onto the history (capped) as a `Recording` entry and rebuild the menu.
/// Keyed by [`corti_queue::job_id`] so the worker's later [`update_history`] calls find the same row. If an
/// entry with this id already exists (e.g. a resume), it's refreshed in place rather than duplicated.
pub fn push_history_recording(app: &AppHandle, meta: &corti_core::RecordingMeta) {
    let entry = HistoryEntry {
        id: corti_queue::job_id(meta),
        label: meta.owning_app.name.clone(),
        started_at: meta.started_at,
        ended_at: meta.ended_at,
        status: JobStatus::Recording,
        mode: meta.mode(),
        error: None,
        note_path: None,
    };
    push_history(app, entry);
}

/// Insert or replace a history entry (front = newest), capped at [`HISTORY_LIMIT`], then rebuild the menu.
/// An existing entry with the same id is moved to the front and replaced (keeps the list de-duplicated when
/// a recording is re-seen, e.g. on resume).
pub fn push_history(app: &AppHandle, entry: HistoryEntry) {
    let id = entry.id.clone();
    if let Some(state) = app.try_state::<AppState>() {
        let mut history = state.history.lock().unwrap();
        history.retain(|e| e.id != entry.id);
        history.push_front(entry);
        while history.len() > HISTORY_LIMIT {
            history.pop_back();
        }
    }
    refresh_menu(app);
    emit_queue_changed(app, &id);
}

/// Update an existing history entry in place (status / error / note_path / ended_at), then rebuild the
/// menu. A no-op if no entry with `id` is tracked (e.g. it aged out of the capped list) — the worker still
/// advances the durable queue regardless.
pub fn update_history(
    app: &AppHandle,
    id: &str,
    status: JobStatus,
    ended_at: Option<DateTime<Local>>,
    error: Option<String>,
    note_path: Option<std::path::PathBuf>,
) {
    if let Some(state) = app.try_state::<AppState>() {
        let mut history = state.history.lock().unwrap();
        if let Some(entry) = history.iter_mut().find(|e| e.id == id) {
            entry.status = status;
            if let Some(ended) = ended_at {
                entry.ended_at = Some(ended);
            }
            if error.is_some() {
                entry.error = error;
            }
            if note_path.is_some() {
                entry.note_path = note_path;
            }
        }
    }
    refresh_menu(app);
    emit_queue_changed(app, id);
}

/// Tell any open Recording Queue window to refetch its list. Coarse-grained by design: the payload is
/// just the touched id; the window re-pulls everything (the list is tiny). Every tray history change
/// routes through here, so the window tracks the pipeline for free.
pub fn emit_queue_changed(app: &AppHandle, id: &str) {
    use tauri::Emitter;
    let _ = app.emit("queue-changed", id);
}

/// The read-only Settings-summary labels for the tray, derived live from the current config so they reflect
/// edits saved in the Settings window. `None` bucket ⇒ no "Bucket:" line.
fn settings_summary(app: &AppHandle) -> (String, Option<String>) {
    let Some(state) = app.try_state::<crate::settings::ConfigState>() else {
        return ("…".to_string(), None);
    };
    let cfg = state.config.lock().unwrap();
    let backend = cfg.backend_name().to_string();
    #[cfg(feature = "aws")]
    let bucket = (cfg.transcribe_backend == crate::config::BackendChoice::Aws).then(|| {
        cfg.aws_bucket
            .clone()
            .unwrap_or_else(|| "(unset — set in Settings…)".to_string())
    });
    #[cfg(not(feature = "aws"))]
    let bucket = None::<String>;
    (backend, bucket)
}

/// Rebuild the menu from state and swap it in (marshalled to the main thread). `pub(crate)` so the Settings
/// `set_config` command can refresh the read-only summary after a save.
pub(crate) fn refresh_menu(app: &AppHandle) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        let Some(tray) = app.tray_by_id(TRAY_ID) else {
            return;
        };
        match build_menu(&app) {
            Ok(menu) => {
                let _ = tray.set_menu(Some(menu));
            }
            Err(e) => eprintln!("[corti] rebuilding tray menu failed: {e}"),
        }
    });
}

/// Spawn the blink loop: while recording, alternate the dot/ring icons ~every 500 ms; otherwise rest on the
/// idle icon. A plain thread (not async) keeps this independent of any tokio runtime.
pub fn spawn_blink(app: AppHandle) {
    std::thread::Builder::new()
        .name("corti-blink".to_string())
        .spawn(move || {
            let mut phase = false;
            let mut shown: Option<bool> = None; // Some(true)=rec icon, Some(false)=idle icon
            loop {
                std::thread::sleep(Duration::from_millis(500));
                // Blink while EITHER capture source is live (detector call or manual webinar); they own
                // independent flags so one ending never stops the blink for the other.
                let recording = app
                    .try_state::<AppState>()
                    .map(|s| {
                        s.detector_recording.load(Ordering::Relaxed)
                            || s.webinar_recording.load(Ordering::Relaxed)
                            || s.live_test_active.load(Ordering::Relaxed)
                    })
                    .unwrap_or(false);
                let want = if recording {
                    phase = !phase;
                    phase
                } else {
                    false
                };
                if shown == Some(want) {
                    continue; // idle steady-state: don't spam set_icon
                }
                shown = Some(want);
                let app = app.clone();
                let _ = app.clone().run_on_main_thread(move || {
                    if let Some(tray) = app.tray_by_id(TRAY_ID) {
                        let _ = tray.set_icon(Some(if want { ICON_REC } else { ICON_IDLE }));
                    }
                });
            }
        })
        .expect("spawning blink thread");
}

/// Handle a tray menu click (registered globally on the Builder so it also catches dynamically-set menus).
pub fn handle_menu_event(app: &AppHandle, event: &MenuEvent) {
    let id = event.id().as_ref();
    match id {
        "quit" => app.exit(0),
        "open_privacy" => open_url(PRIVACY_SCREEN_CAPTURE),
        "open_settings" => open_settings_window(app),
        "open_queue" => open_queue_window(app),
        "ethics_guide" => open_ethics_window(app),
        "open_how" => open_how_window(app),
        "open_diagnostics" => open_console_window(app),
        "live_transcript" => handle_live_transcript_action(app),
        "webinar_toggle" => crate::imp::toggle(app),
        // A recent-note click opens the note; disabled labels (status/backend/bucket/header) never fire.
        _ => {
            if let Some(path) = note_path_from_id(id) {
                open_url(path);
            }
        }
    }
}

/// Open (or focus, if already open) one of the app's singleton webview windows. They all load the same
/// SPA bundle, selecting a view via the URL query (ADR 0004: the tray/pipeline stay windowless; windows
/// are created on demand). Flips the app to `Regular` (Dock presence + focusability) while any window
/// lives and reverts to menu-bar-only `Accessory` once the last one closes. Window + AppKit work must
/// run on the main thread, like every other tray mutation here.
fn open_app_window(
    app: &AppHandle,
    label: &'static str,
    url: &'static str,
    title: &'static str,
    size: (f64, f64),
    min_size: (f64, f64),
    live_generation: Option<u64>,
) {
    let app = app.clone();
    let cleanup_app = app.clone();
    let scheduled = app.clone().run_on_main_thread(move || {
        // Singleton: focus the existing window instead of spawning a second.
        if let Some(win) = app.get_webview_window(label) {
            foreground_window(&app, &win);
            return;
        }

        // A real window wants focusability + a Dock presence: flip Accessory → Regular while it lives.
        let _ = app.set_activation_policy(ActivationPolicy::Regular);

        match WebviewWindowBuilder::new(&app, label, WebviewUrl::App(url.into()))
            .title(title)
            .inner_size(size.0, size.1)
            .min_inner_size(min_size.0, min_size.1)
            .resizable(true)
            .center()
            .build()
        {
            Ok(win) => {
                foreground_window(&app, &win);
                // On close, drop back to menu-bar-only so no stale Dock icon lingers.
                let app_for_evt = app.clone();
                win.on_window_event(move |event| {
                    if matches!(event, WindowEvent::Destroyed) {
                        if let Some(generation) = live_generation
                            && let Some(manager) =
                                app_for_evt.try_state::<crate::live_test::LiveTestManager>()
                        {
                            manager.close_live_window(generation);
                        }
                        revert_activation_policy_if_no_windows(&app_for_evt);
                    }
                });
            }
            Err(e) => {
                eprintln!("[corti] opening {label} window failed: {e}");
                if let Some(generation) = live_generation
                    && let Some(manager) = app.try_state::<crate::live_test::LiveTestManager>()
                {
                    manager.close_live_window(generation);
                }
                // Don't leave a dangling Regular policy with no window.
                revert_activation_policy_if_no_windows(&app);
            }
        }
    });
    if scheduled.is_err()
        && let Some(generation) = live_generation
        && let Some(manager) = cleanup_app.try_state::<crate::live_test::LiveTestManager>()
    {
        manager.close_live_window(generation);
    }
}

/// Activate Corti itself before focusing the webview. `set_focus` alone can leave an Accessory app's new
/// window behind the currently active application (the tray-open bug reported with issue #105).
fn foreground_window(app: &AppHandle, win: &tauri::WebviewWindow) {
    let _ = app.set_activation_policy(ActivationPolicy::Regular);
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        let ns_app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        #[allow(deprecated)]
        ns_app.activateIgnoringOtherApps(true);
    }
    let _ = win.unminimize();
    let _ = win.show();
    let _ = win.set_focus();
}

/// The in-app "Ethics & Legality Guide" window.
fn open_ethics_window(app: &AppHandle) {
    open_app_window(
        app,
        "ethics",
        "index.html",
        "Ethics & Legality Guide",
        (900.0, 700.0),
        (640.0, 480.0),
        None,
    );
}

/// The Preferences editor window. A normal tray click preserves an existing pane; targeted repair links use
/// `open_settings_section` below to navigate deliberately.
fn open_settings_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("settings") {
        foreground_window(app, &win);
        return;
    }
    open_app_window(
        app,
        "settings",
        "index.html?view=settings&section=transcription",
        "Preferences",
        (1040.0, 760.0),
        (620.0, 500.0),
        None,
    );
}

/// Open Preferences at one allowlisted section. An existing singleton receives an in-app navigation event;
/// a new window gets the same destination in its initial URL so Live can offer reliable repair links.
fn open_settings_section(app: &AppHandle, section: &'static str) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.emit(SETTINGS_NAVIGATION_REQUESTED_EVENT, ());
        foreground_window(app, &win);
        return;
    }
    let url = match section {
        "transcription" => "index.html?view=settings&section=transcription",
        "hosted" => "index.html?view=settings&section=hosted",
        "hosted-provider" => "index.html?view=settings&section=hosted-provider",
        "hosted-routing" => "index.html?view=settings&section=hosted-routing",
        "hosted-language" => "index.html?view=settings&section=hosted-language",
        "hosted-advanced" => "index.html?view=settings&section=hosted-advanced",
        "storage" => "index.html?view=settings&section=storage",
        _ => "index.html?view=settings&section=hosted",
    };
    open_app_window(
        app,
        "settings",
        url,
        "Preferences",
        (1040.0, 760.0),
        (620.0, 500.0),
        None,
    );
}

/// Bridge for actionable hosted setup/error links in Live (and same-window Settings actions). The section
/// allowlist prevents a webview payload from turning this into arbitrary navigation, and the caller allowlist
/// preserves every other window boundary.
#[tauri::command]
pub(crate) fn open_preferences_section(
    section: String,
    app: AppHandle,
    navigation: tauri::State<'_, PreferencesNavigation>,
    window: tauri::WebviewWindow,
) -> Result<(), String> {
    if window.label() != "live" && window.label() != "settings" {
        return Err("preferences navigation is unavailable from this window".to_string());
    }
    let section =
        preferences_section(&section).ok_or_else(|| "unknown preferences section".to_string())?;
    navigation.request(section);
    open_settings_section(&app, section);
    Ok(())
}

/// Take the backend-owned repair destination after subscribing to the wake event. A spoofed frontend event
/// can at most cause another empty read; it cannot supply or retain a destination.
#[tauri::command]
pub(crate) fn take_preferences_section_request(
    navigation: tauri::State<'_, PreferencesNavigation>,
    window: tauri::WebviewWindow,
) -> Result<Option<String>, String> {
    if window.label() != "settings" {
        return Err(
            "preferences navigation requests are available only in Preferences".to_string(),
        );
    }
    Ok(navigation.take())
}

/// The timestamped live call / ephemeral microphone-test reader.
fn open_live_transcript_window(app: &AppHandle, generation: Option<u64>) {
    open_app_window(
        app,
        "live",
        "index.html?view=live",
        "Live Transcript",
        (1100.0, 700.0),
        (640.0, 420.0),
        generation,
    );
}

/// The printer-queue-style Recording Queue window.
fn open_queue_window(app: &AppHandle) {
    open_app_window(
        app,
        "queue",
        "index.html?view=queue",
        "Recording Queue",
        (760.0, 560.0),
        (560.0, 400.0),
        None,
    );
}

/// The on-demand Diagnostics console window, using the same singleton/focus/activation lifecycle as every
/// other utility window.
fn open_console_window(app: &AppHandle) {
    open_app_window(
        app,
        "console",
        "index.html?view=console",
        "Diagnostics",
        (900.0, 640.0),
        (560.0, 420.0),
        None,
    );
}

/// The "How Corti Works" window: a live diagram of the detect → capture → echo-cancel → transcribe →
/// file pipeline with the active stage pulsing (view selected via `?view=how`).
fn open_how_window(app: &AppHandle) {
    open_app_window(
        app,
        "how",
        "index.html?view=how",
        "How Corti Works",
        (880.0, 560.0),
        (560.0, 420.0),
        None,
    );
}

/// Return to `Accessory` (menu-bar-only) once no webview windows remain — future-proof against more
/// informational windows being added later.
fn revert_activation_policy_if_no_windows(app: &AppHandle) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        if app.webview_windows().is_empty() {
            let _ = app.set_activation_policy(ActivationPolicy::Accessory);
        }
    });
}

/// One compact `History ▸` line: `<app><mode tag> · <relative time> · <HH:MM:SS> · <state>`. The mode tag
/// marks listen-only "webinar" captures so they're distinguishable from two-way calls at a glance, without
/// reading logs (issue #28). Calls carry no tag (the common case stays uncluttered).
fn history_entry_label(entry: &HistoryEntry, now: DateTime<Local>) -> String {
    format!(
        "{}{} · {} · {} · {}",
        entry.label,
        mode_tag(entry.mode),
        relative_time(entry, now),
        format_duration(entry, now),
        status_label(entry),
    )
}

/// A compact tag appended to a history line's app name to mark the capture mode. A two-way call is the
/// default and shows nothing; a listen-only webinar gets a `🎧 webinar` marker (issue #28).
fn mode_tag(mode: RecordingMode) -> &'static str {
    match mode {
        RecordingMode::Call => "",
        RecordingMode::Webinar => " 🎧 webinar",
    }
}

/// How long ago a recording happened, relative to `now`: `recording now` while live, then
/// `just concluded` / `N min ago` / `N hours ago` / `yesterday` / `N days ago` (issue #3).
fn relative_time(entry: &HistoryEntry, now: DateTime<Local>) -> String {
    if entry.status == JobStatus::Recording {
        return "recording now".to_string();
    }
    // Anchor on when it ended (falling back to start if somehow unset); never report a negative age.
    let reference = entry.ended_at.unwrap_or(entry.started_at);
    let secs = (now - reference).num_seconds().max(0);
    if secs < 60 {
        return "just concluded".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins} min ago");
    }
    // Calendar-day-aware: "yesterday" only when it's the previous calendar day, regardless of hour count.
    let today = now.date_naive();
    let then = reference.date_naive();
    let day_diff = (today - then).num_days();
    match day_diff {
        0 => {
            let hours = mins / 60;
            format!("{hours} hours ago")
        }
        1 => "yesterday".to_string(),
        n => format!("{n} days ago"),
    }
}

/// `HH:MM:SS` duration of a recording. While `Recording`, the live elapsed since `started_at`; once ended,
/// `ended_at − started_at`. `—` if no duration is known (issue #3).
fn format_duration(entry: &HistoryEntry, now: DateTime<Local>) -> String {
    let end = if entry.status == JobStatus::Recording {
        now
    } else {
        match entry.ended_at {
            Some(e) => e,
            None => return "—".to_string(),
        }
    };
    let secs = (end - entry.started_at).num_seconds().max(0);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

/// The transcription-state label for a history entry, mapping [`JobStatus`] to user-facing text. The
/// "Uploading to S3" sub-state is not separately tracked today, so it collapses into `Transcribing`
/// (issue #3 known gap; TODO: surface it via a backend phase callback).
fn status_label(entry: &HistoryEntry) -> String {
    match entry.status {
        JobStatus::Recording => "Recording".to_string(),
        JobStatus::PendingTranscription => "Queued".to_string(),
        // TODO(#3 follow-up): split "Uploading to S3" out of Transcribing via a Transcriber phase callback.
        JobStatus::Transcribing => "Transcribing".to_string(),
        // `PendingNote` is a transient "transcript ready, filing now" step — read as Transcribed.
        JobStatus::PendingNote | JobStatus::Done => "Transcribed".to_string(),
        JobStatus::Failed => match &entry.error {
            Some(err) => format!("Error: {err}"),
            None => "Error".to_string(),
        },
    }
}

/// The menu id that encodes a recent note's path (`note::/path/to/note.md`).
fn note_menu_id(path: &std::path::Path) -> String {
    format!("{NOTE_PREFIX}{}", path.display())
}

/// The note path encoded in a menu id, or `None` if `id` isn't a note id.
fn note_path_from_id(id: &str) -> Option<&str> {
    id.strip_prefix(NOTE_PREFIX)
}

/// Open a path or URL with the system handler (`open`).
fn open_url(target: &str) {
    if let Err(e) = std::process::Command::new("open").arg(target).spawn() {
        eprintln!("[corti] `open {target}` failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        NOTE_PREFIX, PreferencesNavigation, format_duration, history_entry_label,
        live_transcript_menu_state, mode_tag, note_menu_id, note_path_from_id, preferences_section,
        relative_time, status_label,
    };
    use crate::imp::HistoryEntry;
    use chrono::{DateTime, Duration, Local, TimeZone};
    use corti_core::{JobStatus, RecordingMode};
    use std::path::Path;

    #[test]
    fn preferences_navigation_is_allowlisted_backend_owned_and_latest_wins() {
        for section in [
            "transcription",
            "hosted",
            "hosted-provider",
            "hosted-routing",
            "hosted-language",
            "hosted-advanced",
            "storage",
        ] {
            assert_eq!(preferences_section(section), Some(section));
        }
        for rejected in ["", "../queue", "https://example.com", "hosted?other=true"] {
            assert_eq!(preferences_section(rejected), None);
        }

        let navigation = PreferencesNavigation::default();
        navigation.request("hosted-provider");
        navigation.request("hosted-routing");
        assert_eq!(navigation.take().as_deref(), Some("hosted-routing"));
        assert_eq!(navigation.take(), None);
    }

    #[test]
    fn live_transcript_action_tracks_call_test_and_webinar_context() {
        assert_eq!(
            live_transcript_menu_state(true, false, false),
            ("Read live transcript…", true)
        );
        assert_eq!(
            live_transcript_menu_state(false, true, false),
            ("Read microphone test transcript…", true)
        );
        assert_eq!(
            live_transcript_menu_state(false, false, false),
            ("Test microphone & live transcription…", true)
        );
        assert_eq!(
            live_transcript_menu_state(false, false, true),
            ("Test microphone & live transcription…", false)
        );
    }

    #[test]
    fn note_menu_id_round_trips() {
        let id = note_menu_id(Path::new("/Users/me/brain/inbox/zoom-call.md"));
        assert!(id.starts_with(NOTE_PREFIX));
        assert_eq!(
            note_path_from_id(&id),
            Some("/Users/me/brain/inbox/zoom-call.md")
        );
    }

    #[test]
    fn non_note_ids_decode_to_none() {
        assert_eq!(note_path_from_id("quit"), None);
        assert_eq!(note_path_from_id("open_privacy"), None);
        assert_eq!(note_path_from_id("status"), None);
    }

    /// A `now` to anchor relative-time tests on: 2026-06-01 12:00:00 local.
    fn now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()
    }

    /// An entry that started `ago` before `now` and ended `ended_ago` before `now` (None ⇒ still running).
    fn entry(
        status: JobStatus,
        started_ago: Duration,
        ended_ago: Option<Duration>,
    ) -> HistoryEntry {
        HistoryEntry {
            id: "20260601-1158-zoom".to_string(),
            label: "Zoom".to_string(),
            started_at: now() - started_ago,
            ended_at: ended_ago.map(|d| now() - d),
            status,
            mode: RecordingMode::Call,
            error: None,
            note_path: None,
        }
    }

    #[test]
    fn relative_time_covers_every_bucket() {
        // Live recording is always "recording now", regardless of timestamps.
        let live = entry(JobStatus::Recording, Duration::minutes(3), None);
        assert_eq!(relative_time(&live, now()), "recording now");

        let just = entry(
            JobStatus::Done,
            Duration::seconds(50),
            Some(Duration::seconds(5)),
        );
        assert_eq!(relative_time(&just, now()), "just concluded");

        let mins = entry(
            JobStatus::Done,
            Duration::minutes(20),
            Some(Duration::minutes(5)),
        );
        assert_eq!(relative_time(&mins, now()), "5 min ago");

        let hours = entry(
            JobStatus::Done,
            Duration::hours(4),
            Some(Duration::hours(3)),
        );
        assert_eq!(relative_time(&hours, now()), "3 hours ago");

        // Ended just over a calendar day ago ⇒ "yesterday".
        let yest = entry(
            JobStatus::Done,
            Duration::hours(30),
            Some(Duration::hours(26)),
        );
        assert_eq!(relative_time(&yest, now()), "yesterday");

        let days = entry(
            JobStatus::Done,
            Duration::days(4),
            Some(Duration::days(3) + Duration::hours(1)),
        );
        assert_eq!(relative_time(&days, now()), "3 days ago");
    }

    #[test]
    fn duration_formats_hms_and_live_elapsed() {
        // Ended: 00:30:00 between start and end.
        let done = entry(
            JobStatus::Done,
            Duration::minutes(35),
            Some(Duration::minutes(5)),
        );
        assert_eq!(format_duration(&done, now()), "00:30:00");

        // Live: elapsed since start = now - started_at.
        let live = entry(
            JobStatus::Recording,
            Duration::seconds(3 * 3600 + 12 * 60 + 9),
            None,
        );
        assert_eq!(format_duration(&live, now()), "03:12:09");

        // Non-recording with no ended_at ⇒ em dash.
        let mut unknown = entry(JobStatus::Transcribing, Duration::minutes(5), None);
        unknown.ended_at = None;
        assert_eq!(format_duration(&unknown, now()), "—");
    }

    #[test]
    fn status_label_maps_every_job_status() {
        let mk = |s| entry(s, Duration::minutes(1), Some(Duration::seconds(0)));
        assert_eq!(status_label(&mk(JobStatus::Recording)), "Recording");
        assert_eq!(status_label(&mk(JobStatus::PendingTranscription)), "Queued");
        assert_eq!(status_label(&mk(JobStatus::Transcribing)), "Transcribing");
        assert_eq!(status_label(&mk(JobStatus::PendingNote)), "Transcribed");
        assert_eq!(status_label(&mk(JobStatus::Done)), "Transcribed");

        let mut failed = mk(JobStatus::Failed);
        failed.error = Some("transcription job failed".to_string());
        assert_eq!(status_label(&failed), "Error: transcription job failed");
        failed.error = None;
        assert_eq!(status_label(&failed), "Error");
    }

    #[test]
    fn entry_label_joins_the_three_required_fields() {
        let e = entry(
            JobStatus::Done,
            Duration::minutes(35),
            Some(Duration::minutes(5)),
        );
        assert_eq!(
            history_entry_label(&e, now()),
            "Zoom · 5 min ago · 00:30:00 · Transcribed"
        );
    }

    #[test]
    fn webinar_mode_tags_the_label_while_calls_stay_bare() {
        assert_eq!(mode_tag(RecordingMode::Call), "");
        assert_eq!(mode_tag(RecordingMode::Webinar), " 🎧 webinar");

        // A webinar capture gets the tag between the app name and the relative time (issue #28).
        let mut w = entry(
            JobStatus::Done,
            Duration::minutes(35),
            Some(Duration::minutes(5)),
        );
        w.mode = RecordingMode::Webinar;
        assert_eq!(
            history_entry_label(&w, now()),
            "Zoom 🎧 webinar · 5 min ago · 00:30:00 · Transcribed"
        );
    }
}
