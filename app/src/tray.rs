//! The menu-bar tray: a blinking record icon, a status line, recent notes, settings, and quit.
//!
//! AppKit menu/status-item mutations must happen on the main thread, so every tray change here marshals
//! through [`AppHandle::run_on_main_thread`]. The menu is **rebuilt from [`AppState`]** on each change (the
//! status string + recent-notes list are the source of truth) and swapped in via `TrayIcon::set_menu` —
//! simpler and flicker-free since it's a dropdown the user only sees on click.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tauri::image::Image;
use tauri::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, Wry};

use crate::imp::{AppState, RECENT_LIMIT, RecentNote};
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

    // Recent notes (clickable → open in the default markdown app).
    {
        let recent = state.recent.lock().unwrap();
        if !recent.is_empty() {
            items.push(Box::new(MenuItem::with_id(
                app,
                "recent_header",
                "Recent notes",
                false,
                None::<&str>,
            )?));
            for note in recent.iter() {
                let id = note_menu_id(&note.path);
                items.push(Box::new(MenuItem::with_id(
                    app,
                    id,
                    format!("  {}", note.title),
                    true,
                    None::<&str>,
                )?));
            }
            items.push(Box::new(PredefinedMenuItem::separator(app)?));
        }
    }

    // Settings section.
    items.push(Box::new(MenuItem::with_id(
        app,
        "backend",
        format!("Backend: {}", state.backend_label),
        false,
        None::<&str>,
    )?));
    if !state.bucket_label.is_empty() {
        items.push(Box::new(MenuItem::with_id(
            app,
            "bucket",
            format!("Bucket: {}", state.bucket_label),
            false,
            None::<&str>,
        )?));
    }
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

/// Push a freshly filed note onto the recent list (capped) and rebuild the menu.
pub fn push_recent(app: &AppHandle, title: String, path: std::path::PathBuf) {
    if let Some(state) = app.try_state::<AppState>() {
        let mut recent = state.recent.lock().unwrap();
        recent.push_front(RecentNote { title, path });
        while recent.len() > RECENT_LIMIT {
            recent.pop_back();
        }
    }
    refresh_menu(app);
}

/// Rebuild the menu from state and swap it in (marshalled to the main thread).
fn refresh_menu(app: &AppHandle) {
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
                let recording = app
                    .try_state::<AppState>()
                    .map(|s| s.recording.load(Ordering::Relaxed))
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
        "webinar_toggle" => crate::imp::toggle(app),
        // A recent-note click opens the note; disabled labels (status/backend/bucket/header) never fire.
        _ => {
            if let Some(path) = note_path_from_id(id) {
                open_url(path);
            }
        }
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
    use super::{NOTE_PREFIX, note_menu_id, note_path_from_id};
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
}
