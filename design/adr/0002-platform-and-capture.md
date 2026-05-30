# ADR 0002 — Apple Silicon + latest macOS only; all-CoreAudio capture; own the bindings

- **Status:** Accepted (2026-05-29)

## Context

corti is a personal tool for one author's Macs. Supporting Intel or old macOS would force `cfg` matrices,
emulation in CI, and avoiding the newest (and best) audio APIs. Separately, the capture problem — record
the user's mic *and* the meeting's far-end audio, time-aligned — has two candidate stacks: (a)
ScreenCaptureKit for far-end + cpal for mic (two independent streams to align), or (b) a CoreAudio process
tap fed into an aggregate device alongside the mic (one synchronized graph).

## Decision

- **Target `aarch64-apple-darwin` only, latest macOS only** (M1 Pro onward). No Intel, no universal
  binaries, no support for >1 release back. Newest APIs are fair game (process taps need only 14.2+).
- **Capture via CoreAudio** — `AudioHardwareCreateProcessTap` on the owning app's output + an aggregate
  device combining it with the default mic → one clock-synchronized multichannel stream. This avoids
  screen-recording permission, needs no virtual audio device, and removes the two-stream alignment
  problem. ScreenCaptureKit is the documented fallback, not a v1 dependency.
- **Own the bindings.** Where no comprehensive upstream exists (HAL property listeners, process objects,
  process taps), corti ships thin safe wrappers over `coreaudio-sys` / `objc2-*` in `corti-coreaudio`
  rather than risk a dead-end third-party crate. The process-tap symbols absent from `coreaudio-sys` are
  declared in-crate.

## Consequences

- Detection (`kAudioDevicePropertyDeviceIsRunningSomewhere`), attribution (process-object family), and
  capture (process tap + aggregate device) all live in one owned crate, since all three are CoreAudio HAL.
- The single highest-risk item is the synchronized mic+tap aggregate-device capture; it is gated behind a
  spike before the real `corti-capture` crate is built.
- Attribution is best-effort: when multiple processes hold input, prefer a recognized conferencing app,
  then any non-`com.apple.*` process (a live finding — `com.apple.CoreSpeech` opens input alongside real
  apps).
