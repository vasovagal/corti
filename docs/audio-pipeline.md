# Audio pipeline — the data plane

> Verified against v0.8.0 + feat/pipeline-docs-and-streaming.

The audio data plane runs mic-in-use → capture → clean, entirely on OS threads with one lock-free
ring. CoreAudio HAL threads push (an event when the mic starts/stops, PCM frames while recording);
those hop onto the `corti-detect` worker and the `corti-capture-writer` thread. The live path only
ever streams to a 2-track WAV on disk — AEC is a **separate, offline, file-to-file** stage that
runs after capture finishes.

## Mic-in-use detection

Two mechanisms, because corti's own capture pollutes the obvious signal:

- **Idle → event-driven.** `MicMonitor` (`crates/corti-coreaudio/src/listener.rs:120`) installs a
  HAL property listener on `kAudioDevicePropertyDeviceIsRunningSomewhere` scoped to the default
  **input** device (`:135`, `AudioObjectAddPropertyListener` `:140`). The C trampoline re-reads the
  property and forwards a `bool` to a `Fn(bool)+Send` closure, which sends `Msg::Signal(on)` to the
  detect worker. A companion `DefaultInputDeviceMonitor` (`:212`) watches
  `kAudioHardwarePropertyDefaultInputDevice` and fires `Msg::DeviceChanged` so the worker rebinds
  `MicMonitor` when the user switches input (AirPods, etc.).
- **Recording → poll-driven.** Once corti's aggregate device is running, it pins
  `IsRunningSomewhere` true, so the falling edge never arrives via the listener (LESSONS §2). While
  a recording is in flight the worker instead wakes every `POLL_INTERVAL` (1 s,
  `crates/corti-detect/src/lib.rs:39`) and re-derives the signal from process attribution:
  `other_app_holds_input(self_pid)` (`crates/corti-coreaudio/src/process.rs:53`) enumerates
  `kAudioHardwarePropertyProcessObjectList` (`:115`) and checks each object's
  `kAudioProcessPropertyIsRunningInput` (`:121`), excluding corti's own pid. Attribution of *who*
  is on the call is `mic_owner()` (`:28`), preferring a known conferencing app.

The worker's wait is `min(machine deadline, POLL_INTERVAL)` (`platform.rs:148`): it blocks on
`recv_timeout` when idle, polls at 1 Hz when recording.

## The debounce state machine

`Machine` (`crates/corti-detect/src/machine.rs`) is pure and time-injected (no HAL deps,
unit-tested). It coalesces noisy edges into confirmed `Action::Start` / `Action::Stop{keep,
duration}` (`:24`). States (`:34`): `Idle → Arming → Recording → Coasting`. Timing constants
(`lib.rs`):

| Const | Value | Role |
|-------|-------|------|
| `DEBOUNCE` | 1.5 s | rising-edge confirm before `Action::Start` |
| `COALESCE` | 2 s | falling-edge wait + gap-bridging before `Action::Stop` |
| `MIN_RECORDING` | 3 s | discard floor — shorter captures are dropped |
| `POLL_INTERVAL` | 1 s | during-recording attribution poll |

`Action::Start` → `Recorder::start(&owner.app, owner.pid)` (`platform.rs:189-193`); `Action::Stop`
→ `finish()` (or `discard()` under the floor).

## Capture: aggregate device + process tap

`CaptureSession::start_recording` (`crates/corti-coreaudio/src/capture.rs:287`) builds two
CoreAudio objects:

- a **process tap** via the directly-declared `AudioHardwareCreateProcessTap`
  (`capture.rs:44`) from a `CATapDescription` — per-PID (`initStereoMixdownOfProcesses:`) or global
  (`initStereoGlobalTapButExcludeProcesses:`);
- an **aggregate device** (`create_aggregate`, `:855`) whose clock-leading sub-device is the
  default **input** (mic + tap), or an input-less **output** device for tap-only so no orange mic
  dot appears (`tap_only_clock_device`, `listener.rs:50`). The tap is added with drift
  compensation; the aggregate is private with a fresh per-process UID.

Native format is f32 (4 bytes/sample assumed throughout); sample rate is read from the aggregate's
input stream format (`aggregate_input_format`, `:927`), defaulting to **48000** (`:376`).
CoreAudio, not corti, picks the callback buffer size.

## The `io_proc` contract (ADR 0005)

`io_proc` (`capture.rs:541`) is a real-time C callback and obeys one rule: **never accumulate,
never allocate, never block.** Each fire it interleaves the input buffers frame-major (mic
channel(s) first, then tap — the me/them contract), reserves a slot run on the SPSC ring via
`write_chunk_uninit` (`:594`), writes each f32, and `commit_all`s (`:618`). On ring-full it drops
the **whole** callback and bumps a `dropped` atomic (`:620-623`) rather than stalling the RT thread
(guardrail 9). Channel layout is discovered on the first callback and published via atomics on
`Shared` (`:573`).

## The writer thread & `OutputLayout`

A dedicated `corti-capture-writer` thread (`:416`) runs `run_writer` (`:634`): it blocks until the
first callback publishes channel counts, then per-frame downmixes to the requested `OutputLayout`
(`:150`) and writes incrementally with `hound`. The file is created lazily, so a permission-denied
(zero-callback) run leaves no file behind.

| `OutputLayout` | WAV | Contents |
|----------------|-----|----------|
| `TwoTrack` | 2-ch **32-bit float** (`:663`) | ch0 = mono mic mean, ch1 = mono tap mean — lossless, the AEC input |
| `TapOnlyMono` | 1-ch 16-bit (`:664`) | webinar / tap-only |
| `AllChannels` | 16-bit passthrough | debug spike |

`CaptureSession::stop` (`:444`) tears down the io_proc, drops the ring producer (the writer sees
end-of-stream), joins the writer, and returns a `RecordingHandle` (`:173`) with frame/callback/
dropped counts.

**In the app pipeline, PCM is not surfaced as in-memory chunks.** It exists as f32 only transiently
inside `io_proc` and inside `run_writer`'s per-frame `Vec`, and every downstream *app* stage reads
the WAV file — which is why the app transcribes batch (see [architecture.md](architecture.md)
§Corrections). The one in-memory PCM consumer is the optional `CaptureTee`: it attaches to the
`run_writer` ring drain and feeds a live stream — used today by `corti-tap --live`, see
[streaming.md](streaming.md).

## Offline AEC — file-to-file, post-capture

The AEC **kernel** is streaming (a frequency-domain block adaptive filter, ADR 0007), but today it
is **driven over a finished file, not the live ring.** After the detector emits `RecordingFinished`,
the pipeline calls `corti_capture::write_clean_wav(raw_2track_wav, &AecConfig)`
(`crates/corti-capture/src/lib.rs:66`): it opens the 2-track WAV with `hound`, deinterleaves into
`mic`/`tap` Vecs, runs `StreamingAec` (`:102`), and writes a `-clean.wav` sibling (`clean_path`,
`:45-52`). A 1-channel (tap-only/webinar) input has no far end to cancel → `Ok(None)`, skip
cleanly, no sibling written (`:76`).

`StreamingAec` (`crates/corti-aec/src/streaming.rs`) is overlap-save FDAF: block hop = `filter_len`
(default 8192 ≈ 170 ms @ 48 kHz, `lib.rs:35`), FFT size `2·hop`. A tunable lookahead window
(`CORTI_AEC_LOOKAHEAD_SECS`, default 5 s) warms the filter and locks the room delay
(`estimate_delay`, `lib.rs:126`) before emitting. Its API is chunk-in/chunk-out —
`push(&mic,&far) -> Vec<f32>` / `finish() -> Vec<f32>` — with the invariant that total-emitted ==
total-pushed (not per-call). `cancel(mic,far,sr,cfg)` (`lib.rs:229`) is a thin offline shim with
lookahead == whole input.

**Why post-capture today:** relocating `StreamingAec::push()` into the writer thread (so `me` is
cleaned live) is unbuilt — issue #74, the hard blocker for live transcription (ADR 0008). Until it
lands, AEC is a whole-file pass after `finish()`.

## corti-tap shares the same path

`crates/corti-tap/src/main.rs` is a thin CLI over the exact app capture path: it constructs
`corti_capture::Recorder` directly — `Recorder::start` (mic+tap) or `start_tap_only` under
`--no-mic` — the same `Recorder` the detector uses. It differs only in trigger (Ctrl-C vs the mic
state machine) and owner (a synthetic global-tap `OwningApp`, no PID attribution). Its optional
`--inbox` feature spins a current-thread tokio runtime purely to load AWS config and file to vagus
— the only tokio in the front end, and not in the capture path.
