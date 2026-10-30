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
  binaries, no support for >1 release back. The deployment floor tracks **latest-minus-one** —
  `minimumSystemVersion = 15.0` (Sequoia) as of 2026-06, set in `app/tauri.conf.json` and mirrored by the
  Homebrew cask's `depends_on macos: ">= :sequoia"`. (The process-tap API itself only needs 14.2; the 15.0
  floor is a support policy, not an API limit — tighten to `26.0`/`:tahoe` for latest-only.)
- **Capture via CoreAudio** — `AudioHardwareCreateProcessTap` on the owning app's output + an aggregate
  device combining it with the default mic → one clock-synchronized multichannel stream. This avoids
  screen-recording permission, needs no virtual audio device, and removes the two-stream alignment
  problem. ScreenCaptureKit is the documented fallback, not a v1 dependency.
- **Own the bindings.** Where no comprehensive upstream exists (HAL property listeners, process objects,
  process taps), corti ships thin safe wrappers over `coreaudio-sys` / `objc2-*` in `corti-coreaudio`
  rather than risk a dead-end third-party crate. The process-tap symbols absent from `coreaudio-sys` are
  declared in-crate.

## Consequences

### Capture format observations (#162; 2026-10-27)

The capture graph and device-selection policy remain unchanged. Format notifications only invalidate a
fixed atomic mailbox on the HAL listener thread; they never query a property, allocate, lock, or wait.
One owned control worker checks that mailbox at a bounded 5 ms interval and publishes a validated
rate/generation snapshot. The IO callback takes a bounded snapshot before and after copying a block;
an invalidated/unstable format is recorded as unavailable, never guessed from an old rate. This is the
capture-specific off-HAL alternative to the detection listeners' existing async-channel handoff in
guardrail 9. It does not promise that a HAL notification precisely precedes a physical format change;
device transition precision remains an own-Mac acceptance check.

The validated initial rate is retained separately for startup; no later snapshot unwrap can panic during
a notification. Microphone-only transport classifies every input buffer as microphone, including planar
multi-buffer input. Aggregate mic/tap routing is unchanged. Normal stop alone authorizes durable capture
completion after teardown and terminal accounting; Drop or teardown failure preserves an incomplete
quality envelope. If native IO/listener destruction fails, retain the still-owned callback state/device
instead of freeing a pointer HAL may invoke. This failure is reported and cannot certify completion.
An atomic admission gate closes before every teardown attempt, so retained state cannot accept later
audio if native stop/destruction fails. An already-running callback still requires the native destruction
barrier before its state can be freed; disabling admission does not claim that the device itself stopped.

- Detection (`kAudioDevicePropertyDeviceIsRunningSomewhere`), attribution (process-object family), and
  capture (process tap + aggregate device) all live in one owned crate, since all three are CoreAudio HAL.
- The single highest-risk item is the synchronized mic+tap aggregate-device capture; it is gated behind a
  spike before the real `corti-capture` crate is built.
- Attribution is best-effort: when multiple processes hold input, prefer a recognized conferencing app,
  then any non-`com.apple.*` process (a live finding — `com.apple.CoreSpeech` opens input alongside real
  apps).
