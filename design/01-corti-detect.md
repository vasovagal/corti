# 01 — corti-detect

The start/stop trigger and attribution **state machine** that turns mic-in-use transitions into recordings.
It is the glue between `corti-coreaudio` (raw signals) and `corti-capture` (the Recorder).

## Responsibility
- Watch the mic-in-use signal via `corti_coreaudio::MicMonitor`.
- **Debounce** transitions (~1–2 s) so a notification chirp or a brief device reacquisition doesn't spawn a
  recording. Coalesce short gaps (e.g. mic drops for <2 s mid-call) into one recording.
- On a confirmed **rising edge**: attribute the owner (`corti_coreaudio::mic_owner()` → `MicOwner{app,pid}`),
  build a `RecordingMeta`, start a `corti_capture::Recorder` with the attributed PID (per-app tap, global
  fallback).
- On a confirmed **falling edge**: `Recorder::finish()` → a 2-track WAV on disk + finalized `RecordingMeta`
  (sets `ended_at`). Emit a "recording complete" event for the queue/pipeline.

## Public API (target)
```rust
pub enum DetectorEvent {
    RecordingStarted { meta: RecordingMeta },
    RecordingFinished { meta: RecordingMeta, audio_path: PathBuf },
    Error(String),
}

pub struct Detector { /* MicMonitor + debounce timers + current Recorder */ }

impl Detector {
    /// Begin watching. `on_event` is called on the main/async side (NOT the HAL callback thread).
    pub fn start(on_event: impl Fn(DetectorEvent) + Send + 'static) -> Result<Self>;
}
```

## State machine
```
Idle ──mic on (held ≥ DEBOUNCE)──▶ Recording(meta, recorder)
Recording ──mic off (held ≥ DEBOUNCE)──▶ finish ▶ emit RecordingFinished ▶ Idle
Recording ──mic off then on within COALESCE──▶ stay Recording (ignore the blip)
```

## Design notes / gotchas
- `MicMonitor`'s callback runs on a **CoreAudio HAL thread** (guardrail 9). Do not block it and do not start
  capture from it — push transitions over a channel to a worker thread that owns the debounce timers and the
  `Recorder`. The current `MicMonitor` already takes a `Fn(bool) + Send`.
- Capture from a HAL callback would re-enter CoreAudio; always hop threads first.
- Re-bind on default-input-device change (AirPods connect mid-session): `MicMonitor` is bound to one device
  id; on a `kAudioHardwarePropertyDefaultInputDevice` change, drop and recreate it. (Listener for that
  property is a small addition to `corti-coreaudio::listener`.)
- Attribution can be wrong/empty; never let it block capture (guardrail 8). `pid = None` ⇒ global tap.
- Minimum duration floor: discard recordings shorter than ~3 s (accidental mic blips) before emitting.

## Tests
- Pure debounce/coalesce logic should be unit-testable by feeding a synthetic sequence of `(bool, t)`
  transitions into the state machine (factor the timing logic out of the HAL callback).

## Depends on
`corti-core`, `corti-coreaudio` (MicMonitor, mic_owner, MicOwner), `corti-capture` (Recorder).
