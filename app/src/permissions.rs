//! Best-effort macOS permission handling at startup.
//!
//! Two distinct TCC grants matter (design/LESSONS.md §1):
//!   - **Microphone** (`kTCCServiceMicrophone`) — checkable/requestable via `tauri-plugin-macos-permissions`.
//!   - **Audio capture** (`kTCCServiceAudioCapture`, the "Screen & System Audio Recording" pane) — the grant
//!     that actually delivers process-tap audio. Apple exposes **no** preflight for it, so we can't check it
//!     up front; instead the detector surfaces a capture `Error` if it's missing and the user can open the
//!     pane from the tray's Settings item.
//!
//! We only *request* the mic when running as a bundled `.app` (the only context with the Info.plist usage
//! string that `request_microphone_permission` needs — calling it unbundled crashes the process).

use tauri::AppHandle;

/// URL anchor for System Settings → Privacy & Security → Screen & System Audio Recording.
pub const PRIVACY_SCREEN_CAPTURE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

pub fn check_on_startup(app: AppHandle) {
    use tauri_plugin_macos_permissions::{
        check_microphone_permission, request_microphone_permission,
    };

    tauri::async_runtime::spawn(async move {
        if check_microphone_permission().await {
            return;
        }
        if is_bundled() {
            // notDetermined → shows the prompt; the completion handler is null, so re-poll for the result.
            let _ = request_microphone_permission().await;
            if !check_microphone_permission().await {
                // Previously denied → the prompt won't reappear; point the user at Settings.
                crate::tray::set_status(
                    &app,
                    "⚠ Microphone access needed — see Settings ▸ Open Privacy Settings".to_string(),
                );
            }
        } else {
            eprintln!(
                "[corti] microphone not granted; run as a signed .app (LaunchServices) to be prompted"
            );
        }
    });
}

/// Whether we're running from inside an `.app` bundle (vs. a bare `cargo run` / `tauri dev` binary).
fn is_bundled() -> bool {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
        .unwrap_or(false)
}
