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

/// Whether a meaningful mic owner *other than* `self_pid` currently holds input.
///
/// corti's own synchronized capture builds an aggregate device around the mic, which pins the
/// device-level `kAudioDevicePropertyDeviceIsRunningSomewhere` signal true for as long as we record (the
/// IO is "running" even before any audio flows / before the TCC grant). So the
/// [`MicMonitor`](crate::MicMonitor) *falling* edge never fires while we're capturing. To detect that the
/// *call* actually ended, poll this instead: it asks, at the process level, whether any app besides us
/// still has mic input running. Mirrors [`best_running_input_app`]'s ranking via [`counts_as_call_owner`]:
/// a recognized conferencing app counts even when it's an Apple app (FaceTime, Safari are `com.apple.*`),
/// as does any non-Apple app with a bundle id — but not an unrecognized `com.apple.*` audio helper.
pub fn other_app_holds_input(self_pid: i32) -> bool {
    let Ok(objects) = process_object_list() else {
        // Best-effort: if we can't enumerate, report "no other owner" so a stuck recording can still
        // wind down through the debounce window rather than recording forever.
        return false;
    };
    objects.into_iter().any(|obj| {
        is_running_input(obj)
            && pid_of(obj) != Some(self_pid)
            && bundle_id(obj).is_some_and(|b| counts_as_call_owner(&b))
    })
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

/// Whether an input-holding process (other than us) should keep a recording alive in the end-of-call poll.
/// A recognized conferencing app counts **even when it's an Apple app** — FaceTime and Safari are
/// `com.apple.*` yet are real call apps (without this they were dropped as "system helpers", so their calls
/// were cut off after the first poll). Any non-Apple app with a bundle id also counts; an unrecognized
/// `com.apple.*` audio helper (CoreSpeech, controlcenter, …) does not. Symmetric with the start trigger, so a
/// call that can *start* can also be *kept alive*.
fn counts_as_call_owner(bundle_id: &str) -> bool {
    is_known_conferencing_app(bundle_id) || !is_system_helper(bundle_id)
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

#[cfg(test)]
mod tests {
    use super::counts_as_call_owner;

    #[test]
    fn apple_conferencing_apps_keep_a_recording_alive() {
        // Regression: FaceTime/Safari are `com.apple.*` but ARE conferencing apps. They used to be filtered
        // out as "system helpers", so their calls were force-stopped ~1-3 s in and discarded.
        assert!(counts_as_call_owner("com.apple.FaceTime"));
        assert!(counts_as_call_owner("com.apple.Safari"));
        assert!(counts_as_call_owner("com.apple.Safari.WebApp")); // a Safari helper (prefix-matched)
    }

    #[test]
    fn non_apple_apps_keep_a_recording_alive() {
        assert!(counts_as_call_owner("us.zoom.xos"));
        assert!(counts_as_call_owner("com.tinyspeck.slackmacgap.helper"));
        assert!(counts_as_call_owner("com.some.random.app"));
    }

    #[test]
    fn unrecognized_apple_helpers_do_not() {
        assert!(!counts_as_call_owner("com.apple.CoreSpeech"));
        assert!(!counts_as_call_owner("com.apple.controlcenter"));
        assert!(!counts_as_call_owner(
            "com.apple.audio.AudioComponentRegistrar"
        ));
    }
}
