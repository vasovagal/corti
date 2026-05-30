//! Best-effort attribution: which app is currently using the microphone.
//!
//! Primary path (macOS 14.4+, far below our floor): enumerate the HAL process objects
//! (`kAudioHardwarePropertyProcessObjectList`), find the one with `IsRunningInput` true, and read its
//! `BundleID`. Fallback: the frontmost application via `NSWorkspace`. Attribution is inherently
//! best-effort — `IsRunningInput` reflects IO registration, not non-silent audio — so a muted Zoom still
//! reads as the owner, which is exactly what we want for tagging the note.

use anyhow::Result;
use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use coreaudio_sys as ca;
use corti_core::{OwningApp, is_known_conferencing_app};

use crate::property;

/// The app that owns the mic right now, plus its PID when we could attribute it via the HAL process
/// objects. The PID lets capture build a per-app tap (`TapTarget::Process`); `None` means fall back to a
/// global tap.
#[derive(Debug, Clone)]
pub struct MicOwner {
    pub app: OwningApp,
    pub pid: Option<i32>,
}

/// Resolve the app that owns the mic right now (with PID when known), falling back to the frontmost app,
/// then `unknown`.
pub fn mic_owner() -> MicOwner {
    if let Ok(Some((bundle_id, pid))) = best_running_input_app() {
        return MicOwner {
            app: OwningApp::from_bundle_id(bundle_id),
            pid,
        };
    }
    let app = match frontmost_app_bundle_id() {
        Some(bundle_id) => OwningApp::from_bundle_id(bundle_id),
        None => OwningApp::unknown(),
    };
    // The frontmost-app fallback gives us no audio PID, so capture will use a global tap.
    MicOwner { app, pid: None }
}

/// Pick the most meaningful mic owner among all processes holding input, returning its bundle id and PID.
///
/// Several processes can have input running simultaneously (a conferencing app *and* a system helper like
/// `com.apple.CoreSpeech`). Prefer, in order: a recognized conferencing app, then any non-Apple process,
/// then whatever is first. This avoids tagging a meeting note with a background dictation service.
fn best_running_input_app() -> Result<Option<(String, Option<i32>)>> {
    // (bundle_id, pid) for every process currently holding input.
    let mut candidates: Vec<(String, Option<i32>)> = Vec::new();
    for proc_obj in process_object_list()? {
        if is_running_input(proc_obj)
            && let Some(b) = bundle_id(proc_obj)
        {
            candidates.push((b, pid_of(proc_obj)));
        }
    }
    let pick = candidates
        .iter()
        .find(|(b, _)| is_known_conferencing_app(b))
        .or_else(|| candidates.iter().find(|(b, _)| !is_system_helper(b)))
        .or_else(|| candidates.first())
        .cloned();
    Ok(pick)
}

/// The PID of a HAL process object, if readable.
fn pid_of(proc_obj: ca::AudioObjectID) -> Option<i32> {
    let addr = property::global(ca::kAudioProcessPropertyPID);
    unsafe { property::get::<i32>(proc_obj, &addr) }
        .ok()
        .filter(|&p| p > 0)
}

/// macOS audio helpers we never want to attribute a recording to (they open input alongside real apps).
fn is_system_helper(bundle_id: &str) -> bool {
    bundle_id.starts_with("com.apple.")
}

/// All HAL process objects.
fn process_object_list() -> Result<Vec<ca::AudioObjectID>> {
    let addr = property::global(ca::kAudioHardwarePropertyProcessObjectList);
    unsafe { property::get_vec(ca::kAudioObjectSystemObject, &addr) }
}

/// Whether a process object currently has input IO running.
fn is_running_input(proc_obj: ca::AudioObjectID) -> bool {
    let addr = property::global(ca::kAudioProcessPropertyIsRunningInput);
    unsafe { property::get::<u32>(proc_obj, &addr) }
        .map(|v| v != 0)
        .unwrap_or(false)
}

/// The bundle id of a process object, if it has one.
fn bundle_id(proc_obj: ca::AudioObjectID) -> Option<String> {
    let addr = property::global(ca::kAudioProcessPropertyBundleID);
    // The property hands back a CFStringRef owned by us (+1); wrap with the create rule so it is released.
    let cfref: CFStringRef = unsafe { property::get(proc_obj, &addr) }.ok()?;
    if cfref.is_null() {
        return None;
    }
    let s = unsafe { CFString::wrap_under_create_rule(cfref) }.to_string();
    (!s.is_empty()).then_some(s)
}

/// Frontmost application's bundle id, via AppKit (`NSWorkspace`). The heuristic fallback when the HAL
/// process objects don't attribute the mic.
fn frontmost_app_bundle_id() -> Option<String> {
    use objc2_app_kit::NSWorkspace;
    let workspace = NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    let bundle_id = app.bundleIdentifier()?;
    Some(bundle_id.to_string())
}
