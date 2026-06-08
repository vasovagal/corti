//! The menu-bar tray: a blinking record icon, a status line, recent notes, settings, and quit.
//!
//! AppKit menu/status-item mutations must happen on the main thread, so every tray change here marshals
//! through [`AppHandle::run_on_main_thread`]. The menu is **rebuilt from [`AppState`]** on each change (the
//! status string + recent-notes list are the source of truth) and swapped in via `TrayIcon::set_menu` —
//! simpler and flicker-free since it's a dropdown the user only sees on click.

use std::sync::atomic::Ordering;
use std::time::Duration;

use chrono::{DateTime, Local};
use corti_core::{JobStatus, RecordingMode};
use tauri::image::Image;
use tauri::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIconBuilder;
use tauri::{
    ActivationPolicy, AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent, Wry,
};

use crate::imp::{AppState, HISTORY_LIMIT, HistoryEntry};
use crate::permissions::PRIVACY_SCREEN_CAPTURE;

const TRAY_ID: &str = "corti-tray";

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

    // Settings section. The Backend:/Bucket: lines are a read-only summary derived live from the current
    // config (so they reflect edits saved in the Settings window); the window itself is the editor.
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
        "open_settings",
        "Settings…",
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
    if let Some(state) = app.try_state::<AppState>() {
        let mut history = state.history.lock().unwrap();
        history.retain(|e| e.id != entry.id);
        history.push_front(entry);
        while history.len() > HISTORY_LIMIT {
            history.pop_back();
        }
    }
    refresh_menu(app);
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
        "ethics_guide" => open_ethics_window(app),
        "webinar_toggle" => crate::imp::toggle(app),
        // A recent-note click opens the note; disabled labels (status/backend/bucket/header) never fire.
        _ => {
            if let Some(path) = note_path_from_id(id) {
                open_url(path);
            }
        }
    }
}

/// Open (or focus, if already open) the in-app "Ethics & Legality Guide" webview window. This is the
/// app's only window — the tray/pipeline stay windowless (ADR 0004). Window + AppKit work must run on
/// the main thread, like every other tray mutation here.
fn open_ethics_window(app: &AppHandle) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        // Singleton: focus the existing window instead of spawning a second.
        if let Some(win) = app.get_webview_window("ethics") {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
            return;
        }

        // A real window wants focusability + a Dock presence: flip Accessory → Regular while it lives.
        let _ = app.set_activation_policy(ActivationPolicy::Regular);

        match WebviewWindowBuilder::new(&app, "ethics", WebviewUrl::App("index.html".into()))
            .title("Ethics & Legality Guide")
            .inner_size(900.0, 700.0)
            .min_inner_size(640.0, 480.0)
            .resizable(true)
            .build()
        {
            Ok(win) => {
                // On close, drop back to menu-bar-only so no stale Dock icon lingers.
                let app_for_evt = app.clone();
                win.on_window_event(move |event| {
                    if matches!(event, WindowEvent::Destroyed) {
                        revert_activation_policy_if_no_windows(&app_for_evt);
                    }
                });
            }
            Err(e) => {
                eprintln!("[corti] opening ethics window failed: {e}");
                // Don't leave a dangling Regular policy with no window.
                revert_activation_policy_if_no_windows(&app);
            }
        }
    });
}

/// Open (or focus, if already open) the in-app Settings window. Created on demand like the Ethics window
/// (ADR 0004): flips the app to `Regular` while it lives and reverts to menu-bar-only `Accessory` once it
/// (and any other window) closes. It loads the same SPA bundle as the Ethics window, selecting the settings
/// view via the `?view=settings` query. Window + AppKit work runs on the main thread.
fn open_settings_window(app: &AppHandle) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        // Singleton: focus the existing window instead of spawning a second.
        if let Some(win) = app.get_webview_window("settings") {
            let _ = win.unminimize();
            let _ = win.show();
            let _ = win.set_focus();
            return;
        }

        // A real window wants focusability + a Dock presence: flip Accessory → Regular while it lives.
        let _ = app.set_activation_policy(ActivationPolicy::Regular);

        match WebviewWindowBuilder::new(
            &app,
            "settings",
            WebviewUrl::App("index.html?view=settings".into()),
        )
        .title("Settings")
        .inner_size(720.0, 640.0)
        .min_inner_size(560.0, 420.0)
        .resizable(true)
        .build()
        {
            Ok(win) => {
                // On close, drop back to menu-bar-only so no stale Dock icon lingers.
                let app_for_evt = app.clone();
                win.on_window_event(move |event| {
                    if matches!(event, WindowEvent::Destroyed) {
                        revert_activation_policy_if_no_windows(&app_for_evt);
                    }
                });
            }
            Err(e) => {
                eprintln!("[corti] opening settings window failed: {e}");
                // Don't leave a dangling Regular policy with no window.
                revert_activation_policy_if_no_windows(&app);
            }
        }
    });
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
        NOTE_PREFIX, format_duration, history_entry_label, mode_tag, note_menu_id,
        note_path_from_id, relative_time, status_label,
    };
    use crate::imp::HistoryEntry;
    use chrono::{DateTime, Duration, Local, TimeZone};
    use corti_core::{JobStatus, RecordingMode};
    use std::path::Path;

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
