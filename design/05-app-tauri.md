# 05 — app (Tauri 2 tray binary)

The menu-bar app that ties everything together: a blinking record icon while capturing, and the
detect → capture → (aec) → transcribe → vagus pipeline.

## Shell / UX
- **Accessory activation policy** (`tauri::ActivationPolicy::Accessory`) → menu-bar only, no Dock icon.
- **Tray icon** via `TrayIconBuilder` in `setup`; **blink** by toggling between two template `Image`s on a
  ~500 ms `tauri::async_runtime::spawn` interval while recording; steady idle icon otherwise.
- Tray menu: status line, recent notes (open in vagus), settings, quit.

## Wiring
- Spawn `corti_detect::Detector` on an `AppHandle`; bridge its events to the Tokio side via a channel
  (the detector already hops off the HAL thread).
- Pipeline per finished recording: `corti-queue::enqueue` → transcribe (selected backend) → optional
  `corti-aec` → `corti-vagus::file_recording` → mark `Done`. Persist each step so a crash resumes
  (`corti-queue::resumable` on startup).
- Backend chosen by Cargo features: `default = ["aws"]`, `whisper` opt-in.

## Packaging & permissions (the part that bites — see LESSONS.md §1)
- Ship a real `.app` bundle (Tauri does this). `Contents/Info.plist` (merged by Tauri) must include
  **`NSAudioCaptureUsageDescription`** and **`NSMicrophoneUsageDescription`**. The audio-capture grant is
  what lets the process tap deliver audio.
- **Entitlements:** sign with BOTH `com.apple.security.device.audio-input` and
  `com.apple.security.device.microphone`, or the mic is silently denied in a signed build. Verify with
  `codesign -d --entitlements - -vvv corti.app`.
- Launching matters: a bundled, LaunchServices-launched app gets its own TCC identity (unlike a loose
  binary). The tray app is launched normally, so this is satisfied — but **test capture in a signed,
  notarized build**, not just `tauri dev`.
- Use `tauri-plugin-macos-permissions` to check/request mic + audio-capture status and route the user to
  System Settings on first run.

## Crate
Binary crate at `app/` (workspace member, `publish = false`). Heavy deps (tauri, the aws/whisper backends)
live here, keeping the library crates lean. Apple-Silicon + latest-macOS only, like the rest.

## Depends on
All library crates: corti-core, corti-coreaudio, corti-capture, corti-detect, corti-transcribe(+backend),
corti-queue, corti-aec, corti-vagus.
