# Architecture — the whole-pipeline graph

> Verified against v0.8.0 + feat/pipeline-docs-and-streaming + #85 (durable-jobs stack).

corti is a windowless macOS menu-bar agent that watches for a call, captures a 2-track WAV to
disk, then — after the call ends — cleans, transcribes, and files it. Everything runs in **one
process** on **OS threads + channels + atomics**; the design is deliberately sync-core with no
tokio in the capture/transcribe path (§Async islands). Data moves push-first: CoreAudio HAL
threads push events and PCM, which hop onto worker threads over an `rtrb` ring (audio) and two
`std::sync::mpsc` channels (control).

## The graph

```
                          ┌──────────────────────── one process ────────────────────────┐
  CoreAudio HAL threads   │                                                              │
  ─────────────────────   │   corti-detect worker thread          corti-pipeline thread │
  MicMonitor  ┐           │   ("the poller")                      (sole Queue owner)     │
  (IsRunning- ├─Msg──────►│   Machine (Idle/Arming/                                       │
   Somewhere) │  mpsc     │    Recording/Coasting)                 drains PipelineMsg     │
  DefaultIn-  ┘  <Msg>    │      │ Action::Start/Stop                    ▲                 │
   putDevice             │      ▼                                       │ mpsc            │
   Monitor               │   Recorder::start ──► CaptureSession         │ <PipelineMsg>   │
                          │                        │                    │                 │
                          │   Action::Stop ──► finish() ──► WAV path ────┘ Process{meta,   │
                          │      │ DetectorEvent (on detect thread)         audio_path}    │
                          │      ▼                                          │              │
                          │   handle_detector_event ─set flags,tray─►       ▼              │
                          │                                          enqueue → transcribe  │
                          │                                          → file → Done         │
                          │                                            │  │  │             │
   ── capture internals ──┤   ┌── CaptureSession ──────────────┐       │  │  └─► vagus CLI │
                          │   │ io_proc (RT C callback)          │       │  │    (subprocess│
   mic+tap PCM ──push────►│   │   push f32 ──► rtrb ring ──────► │       │  │     — filing) │
   (aggregate device)     │   │              (SPSC, drop-full)   │       │  └─► Backend     │
                          │   │   corti-capture-writer thread ◄──┘       │      .transcribe │
                          │   │   drains ring ──► hound ──► 2-track WAV   │      (blocking)  │
                          │   └──────────────────────────────────┘       │                 │
                          │                                    write_clean_wav (offline AEC)│
                          └──────────────────────────────────────────────────────────────┘
```

Read it as three long-lived worker threads (detector, pipeline, plus the per-recording capture
writer) fed by HAL callback threads, with the tray/main thread (not drawn) owning all AppKit
mutation. Two misconceptions this graph is meant to kill are corrected in §Corrections.

The `enqueue → transcribe → file → Done` line is the happy path; #85 wrapped it in a **tick loop**. The
pipeline thread `recv_timeout`s on the channel, so between recordings it also drains due `corti-jobs`:
a failed transcribe/file doesn't terminal-fail but schedules a durable retry job (backoff, ≤5 attempts,
looping back into `transcribe → file`), and an hourly periodic sweep enforces audio retention. Same one
thread, still serial — see [transcription.md](transcription.md).

## Threads

| Thread | Spawned | Owns / does |
|--------|---------|-------------|
| Tauri main | `app.run` (`app/src/main.rs`) | The event loop; every tray/menu/window mutation is marshalled here via `run_on_main_thread`. |
| `corti-detect` worker | `Detector::start` (`crates/corti-detect/src/platform.rs:41`, spawn `:60`) | `Machine` + `MicMonitor` + `DefaultInputDeviceMonitor` + the in-flight `Recorder`. The state machine + poll loop. |
| `corti-pipeline` | `app/src/main.rs:336-339` | **Sole** `Queue` owner; a tick loop (`run`, `app/src/pipeline.rs:87`) that drains `PipelineMsg` **and** due `corti-jobs` (retry/sweep) serially. |
| `corti-capture-writer` (one per recording) | `crates/corti-coreaudio/src/capture.rs:416` | Drains the `rtrb` ring and writes the 2-track WAV with `hound` (`run_writer`, `:634`). |
| `corti-blink`, `corti-stats` | `app/src/tray.rs`, `app/src/stats.rs` | Icon-swap animation; 1 Hz stats sampler. |
| CoreAudio HAL callback threads | OS/CoreAudio | `MicMonitor`/`DefaultInputDeviceMonitor` trampolines and `io_proc`. They only push (`tx.send` / ring write) — never touch capture state (guardrail 9). |

See [app.md](app.md) for the tray/window/command surface and the full thread inventory.

## Channels & rings

| Carrier | Type | From → To | Payload |
|---------|------|-----------|---------|
| HAL → detector | `std::sync::mpsc::Sender<Msg>` (`platform.rs`) | `MicMonitor`/`DefaultInputDeviceMonitor` callback → detect worker | `Msg::Signal(bool)` / `Msg::DeviceChanged` / `Msg::Shutdown` (`platform.rs:24-28`) |
| capture data plane | `rtrb::Producer<f32>` / `Consumer<f32>` (SPSC, wait-free) | `io_proc` → writer thread | interleaved f32 frames (mic channels first, then tap — the me/them contract). Ring sized `sample_rate · 8ch · RING_SECONDS` (`ring_capacity`, `capture.rs:134`, default 30 s, `:60`). |
| detector → app | closure callback (`DetectorEvent`) | detect worker → `handle_detector_event` (`main.rs:368`) | `RecordingStarted` / `RecordingFinished{meta, audio_path}` / `RecordingDiscarded` / `Error` (`crates/corti-detect/src/lib.rs:43`) |
| app → pipeline | `std::sync::mpsc::Sender<PipelineMsg>` (`main.rs:317`) | detector callback + webinar-finish thread + Queue-window Retry → pipeline worker | `Process{meta, audio_path}` / `Retry{id}` / `ReloadConfig` (`pipeline.rs:47`) |

The two `mpsc` channels are control-plane (coarse start/stop events carrying file paths); the
`rtrb` ring is the only data-plane carrier, and PCM lives in memory nowhere else — see
[audio-pipeline.md](audio-pipeline.md).

## Ownership chains

The detector is a managed Tauri singleton whose whole point is to keep the worker thread alive:

```
Tauri managed-state registry
  └─ DetectorHandle(Mutex<Detector>)          main.rs:196 (managed :358) — Mutex is NEVER locked;
       └─ Detector{ ctrl: Sender<Msg>,          it only makes the !Sync Detector Send+Sync so it
                    worker: JoinHandle }         can be managed. Drop → Msg::Shutdown + join
            └─ Worker (on the spawned thread)    (platform.rs:85).
                 ├─ Machine                      the debounce/coalesce state machine
                 ├─ MicMonitor (rebindable)      HAL listener on the default input device
                 ├─ DefaultInputDeviceMonitor    rebinds MicMonitor on input-device switch
                 └─ current: Option<(Recorder, RecordingMeta)>
                        └─ Recorder              corti-capture
                             └─ CaptureSession   owns tap + aggregate device + io_proc,
                                  ├─ Cap          the *mut ring producer (touched only by io_proc)
                                  └─ JoinHandle   the writer thread
```

The pipeline worker is the mirror image: it is the **only** owner of the rusqlite `Queue`
(`Connection` is `Send`, not `Sync`, so it is never shared — `pipeline.rs:1-7`). The tray owns no
state; it rebuilds its menu from the managed `AppState` snapshot (`build_menu`, `tray.rs:47`). Full app-side
ownership is in [app.md](app.md).

## Corrections

Two things the diagram makes explicit because they are routinely misremembered:

1. **The recorder is in-process, not a subprocess.** `Recorder::start`
   (`crates/corti-capture/src/lib.rs:175`) builds a `CaptureSession` that installs a CoreAudio
   `io_proc` and spawns an in-process writer thread. No child process is forked to record. The
   **only** subprocess in the whole flow is the external `vagus` CLI at filing time
   (`crates/corti-vagus/src/lib.rs:100`).
2. **The app pipeline transcribes batch, post-hoc — PCM goes to disk first.** In the app nothing
   is transcribed live: the capture path streams f32 frames to a WAV on disk, and only after
   `Recorder::finish` yields a finished file does the pipeline call `Backend::transcribe(path)`.
   The `Transcriber` trait itself takes a `&Path` to a complete 2-track WAV
   (`crates/corti-transcribe/src/lib.rs:23`), so the batch shape is baked into that contract. The
   codebase *does* have a live path — an optional `CaptureTee` off the writer thread feeding
   `LiveTranscriber` — but it is not yet wired into the app pipeline (used today by
   `corti-tap --live`); see [streaming.md](streaming.md) and ADR 0008/0009.

## Async islands

corti is **sync-core: no tokio** in `corti-detect`, `corti-capture`, `corti-coreaudio`, or
`corti-aec` (their `Cargo.toml`s carry no tokio/futures). All pipelining is std threads + `mpsc` +
`rtrb`. Async exists in exactly two walled-off islands, neither in the capture or transcribe hot
path:

- **`tauri::async_runtime`** — Tauri's internal tokio, used for the startup microphone-permission
  check (`app/src/permissions.rs`) and the async `#[tauri::command]`s (e.g. `verify_aws` in
  `app/src/settings.rs`).
- **Private current-thread runtimes for AWS** — `build_sdk_config` builds a throwaway
  `new_current_thread` runtime and `block_on`s the credential load once at worker startup
  (`app/src/transcribe.rs:229`); the AWS transcriber spins its own current-thread runtime *inside*
  its blocking `transcribe()` (`crates/corti-transcribe-aws/src/lib.rs:339`). From the pipeline
  thread's view both are ordinary synchronous blocking calls behind the sync `Transcriber` trait.
