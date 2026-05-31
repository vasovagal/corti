# Lessons learned — corti audio capture (READ THIS FIRST when resuming)

Getting synchronized mic + system-audio capture working on macOS 26 (Apple Silicon) was the hard part of
this project. Everything below is verified on a real machine, not theory. The working code lives in
[`crates/corti-coreaudio/src/capture.rs`](../crates/corti-coreaudio/src/capture.rs); this file explains
*why* it's shaped that way so you don't re-learn it the expensive way.

## The architecture that works (proven 2026-05-30)

One CoreAudio **aggregate device** that combines the **default mic** (a clock-leading sub-device) with a
**process tap** of system output, pulled through a **single IO proc**. This yields mic + far-end audio on
one shared clock — no ScreenCaptureKit, no virtual audio device (BlackHole), no two-stream alignment. The
spike produced a clean synchronized 3-channel WAV (ch0 = mic, ch1/2 = system tap).

Recipe (matches `capture.rs`):
1. `CATapDescription` — `initStereoGlobalTapButExcludeProcesses:[]` (all system audio) or
   `initStereoMixdownOfProcesses:[procObjID]` (one app). Create the tap with `AudioHardwareCreateProcessTap`.
2. Read the tap's UID (`kAudioTapPropertyUID`) and the mic's UID (`kAudioDevicePropertyDeviceUID`).
3. Build a **private aggregate device** dict: mic UID as `MainSubDevice` + sole `SubDeviceList` entry; the
   tap UID in `TapList` with `DriftCompensation` on; `IsPrivate` + `TapAutoStart` true.
4. `AudioDeviceCreateIOProcID` + `AudioDeviceStart` on the **aggregate** id (not the tap, not the mic).
5. In the IO proc, the input `AudioBufferList` carries **buffer 0 = mic channels, then tap channels** — so
   `buffers[0].mNumberChannels` is the mic-channel count and the split is free.

## The traps that cost us the most time

### 1. TCC silently denies audio capture — no prompt, `io proc fired 0 times`
The process-tap + aggregate + IOProc + start calls **all return `noErr` even with no permission**, then
deliver zero callbacks. This is *the* failure mode. Confirmed against Guilherme Rambo's **AudioCap** (the
canonical reference) and ports (cpal, screenpipe).

- The permission is the **audio-capture** service (`kTCCServiceAudioCapture`), Info.plist key
  **`NSAudioCaptureUsageDescription`** — *not* microphone, *not* screen recording. It surfaces under
  **System Settings → Privacy & Security → Screen & System Audio Recording**.
- **The binary must have a TCC identity reached via LaunchServices.** Running
  `./Foo.app/Contents/MacOS/foo` directly from a shell makes the **terminal** the responsible process →
  silent denial, no prompt. You **must** `open Foo.app` (LaunchServices) so the bundle's own identity is
  used. A loose binary with an embedded plist is *not* enough — we tried; it was silently denied.
- `open` detaches stdout, so the spike logs diagnostics to a `<out>.log` file. Keep that pattern.
- Ad-hoc signing (`codesign -s -`) works but the cdhash changes every rebuild, so macOS re-prompts and
  orphans the old grant. For repeated dev runs make a **stable self-signed cert** and pass its name via
  `CORTI_SIGN_ID` (see `bundle-spike.sh`).

See [`crates/corti-coreaudio/SPIKE.md`](../crates/corti-coreaudio/SPIKE.md) for the run procedure.

### 2. objc2 `msg_send` argument encoding (`@` vs `^v`)
`initStereoGlobalTapButExcludeProcesses:` wants an Objective-C **object** pointer (`@`). Passing a
`*mut c_void` (or a bridged `CFArrayRef`) is rejected at runtime by objc2's encoding check (`^v`). Pass a
real `NSArray` by reference (`&*NSArray::from_retained_slice(&[...])`). objc2's runtime verification caught
this — exactly the payoff of owning safe-ish bindings.

### 3. `-sectcreate` must be split across `-Xlinker`
Embedding `Info.plist` into a CLI binary's `__TEXT,__info_plist` needs four ld tokens
(`-sectcreate __TEXT __info_plist <path>`). The packed `-Wl,...` form gets mis-split by ld
(`file not found: __TEXT`). Emit each token as its own `cargo:rustc-link-arg-bins=-Xlinker` /
`=<token>` pair. See `crates/corti-coreaudio/build.rs`. (Note: for the real app we ship a `.app` bundle with
a normal `Contents/Info.plist`, so this `-sectcreate` trick is mainly for the standalone spike.)

### 4. Mic bleed is acoustic, not a capture bug
If you play far-end audio on **speakers**, the mic physically hears it → it appears on the mic track. No
capture method can prevent that. The *tap* channels are always clean (pulled digitally). With **headphones**
the mic track is clean too. This is why AEC (offline NLMS) is a v1 feature — see
[`04-corti-aec.md`](04-corti-aec.md) — and why we always preserve the raw mic + far-end tracks.

### 5. Attribution: Electron helpers and Apple helpers
- Conferencing apps (Slack/Chrome/Discord/Teams) open the mic from a **helper** process whose bundle id is
  the parent id plus a suffix (`com.tinyspeck.slackmacgap.helper`). Match known apps by **prefix**
  (`corti-core::is_known_conferencing_app`).
- Multiple processes hold input at once; `com.apple.CoreSpeech` (dictation) often does too. Rank: known
  conferencing app → any non-`com.apple.*` → first. Skip `com.apple.*` helpers.
- For per-app tapping, attribution must also return the **PID** (`MicOwner.pid`) so capture can translate it
  to a process object (`kAudioHardwarePropertyTranslatePIDToProcessObject`).

### 6. Our own capture pins the mic-in-use signal — the falling edge needs process polling
Once corti starts capturing, it builds an **aggregate device around the mic**, which keeps
`kAudioDevicePropertyDeviceIsRunningSomewhere` on the physical input device **stuck true for as long as we
record** — even before any audio flows (the IO is "running" regardless of the TCC grant). So the
`MicMonitor` *rising* edge works, but the **falling edge never fires while we hold the device**: the call
ends and the listener stays silent (observed live — a recording started but never finished). Detect
end-of-call at the **process** level instead: poll `corti_coreaudio::other_app_holds_input(self_pid)` —
"is any app besides us (and besides `com.apple.*` helpers) still holding mic input?" — and feed that into
the debounce machine. `corti-detect` stays event-driven while idle (clean signal) and switches to polling
(`POLL_INTERVAL`, ~1 s) only while a recording is in flight. Excluding our own PID is essential once corti
ships as a signed `.app` with its own bundle id (it would otherwise rank as a mic owner and never stop).

## Environment facts
- Verified on macOS 26.4.1 (Tahoe), Apple Silicon (arm64), `cargo 1.94`, rustup stable.
- `coreaudio-sys 0.2.17` exposes the HAL property API, aggregate-device + IOProc functions, and all the tap
  *constants* — but **not** `AudioHardwareCreateProcessTap`/`Destroy` (declared by hand in `capture.rs`) and
  not the `CATapDescription` ObjC class (called via objc2).
- The user's `vagus` CLI is at `/opt/homebrew/bin/vagus` (v0.1.8); corti shells out to it.

## Verification commands
```sh
cargo build && cargo clippy --all-targets && cargo fmt --check && cargo test
cargo run -p corti-coreaudio --bin probe                 # live mic-on/off + attribution
./crates/corti-coreaudio/bundle-spike.sh 15 /tmp/x.wav   # full capture (needs TCC grant)
cargo run -p corti-coreaudio --example splitwav -- /tmp/x.wav  # split channels, then afplay
```
