# Architecture — the whole-pipeline graph

> Verified against main + transcribe.cpp integration PR #92 and crash-safe rolling live commits (#85, #87, #93, #103).

corti is a windowless macOS menu-bar agent that watches for a call, captures a 2-track WAV to
disk, and files it as a vagus note — **live while the call runs** when the local backend + models
are available (#87), else batch after the call ends. Everything runs in **one process** on
**OS threads + channels + atomics**; the design is deliberately sync-core with no
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
                          │   └───────│──────────────────────────┘       │                 │
                          │           │ try_send (bounded lossy CaptureTee, #87)            │
                          │           ▼                                                     │
                          │   corti-live thread (one per recording, when live filing is on) │
                          │     StreamingAec → 2× LiveTranscriber (CPU or Metal) → checkpoint│
                          │     → optional bounded diarize → append + sync_all; final flip   │
                          │                                    write_clean_wav (offline AEC)│
                          └──────────────────────────────────────────────────────────────┘
```

Read it as three long-lived worker threads (detector, pipeline, plus the per-recording capture
writer and — when live filing is eligible — a per-recording `corti-live` consumer, #87) fed by HAL
callback threads, with the tray/main thread (not drawn) owning all AppKit mutation. Two
misconceptions this graph is meant to kill are corrected in §Corrections.

The `enqueue → transcribe → file → Done` line is the batch path; #87 short-circuits it when the
`corti-live` thread already filed the note during the call (`Process` goes straight to `Done` —
[transcription.md](transcription.md#live-inbox-filing-87)), and #85 wrapped it in a **tick loop**. The
pipeline thread `recv_timeout`s on the channel, so between recordings it also drains due `corti-jobs`:
a failed transcribe/file doesn't terminal-fail but schedules a durable retry job (backoff, ≤5 attempts,
looping back into `transcribe → file`), and an hourly periodic sweep enforces audio retention. Same one
thread, still serial — see [transcription.md](transcription.md).

## Threads

| Thread | Spawned | Owns / does |
|--------|---------|-------------|
| Tauri main | `app.run` (`app/src/main.rs`) | The event loop; every tray/menu/window mutation is marshalled here via `run_on_main_thread`. |
| `corti-detect` worker | `Detector::start` (`crates/corti-detect/src/platform.rs:64`, spawn `:90`) | `Machine` + `MicMonitor` + `DefaultInputDeviceMonitor` + the in-flight `Recorder`. The state machine + poll loop. |
| `corti-pipeline` | `app/src/main.rs:343` | **Sole** `Queue` owner; a tick loop (`run`, `app/src/pipeline.rs:101`) that drains `PipelineMsg` **and** due `corti-jobs` (retry/sweep) serially. |
| `corti-capture-writer` (one per recording) | `crates/corti-coreaudio/src/capture.rs:416` | Drains the `rtrb` ring and writes the 2-track WAV with `hound` (`run_writer`, `:634`). |
| `corti-live` (one per eligible detector recording, #87/#103/#105) | `LiveManager::spawn` via the detector's `LiveHook` | Drains the bounded tee → streaming AEC + two channels sharing the selected resident Parakeet ASR (`sherpa`/CPU or `transcribe.cpp`/Metal); publishes closed regions to the bounded reader, checkpoints on the configured interval, optionally diarizes only that bounded far-end window, appends + `sync_all`s once, then syncs the final `State:` flip. Panic-contained; any failure preserves prior chunks and falls back to the same note. |
| `corti-live-test` + `corti-mic-test-capture` (#105) | contextual idle tray action | One selected local ASR/VAD channel plus direct default-input SPSC-ring consumer. No tap, WAV, note, or queue row; detector edges/background jobs are deferred until cleanup. |
| `corti-blink`, `corti-stats` | `app/src/tray.rs`, `app/src/stats.rs` | Icon-swap animation; 1 Hz stats sampler. |
| CoreAudio HAL callback threads | OS/CoreAudio | `MicMonitor`/`DefaultInputDeviceMonitor` trampolines and `io_proc`. They only push (`tx.send` / ring write) — never touch capture state (guardrail 9). |

See [app.md](app.md) for the tray/window/command surface and the full thread inventory.

## Channels & rings

| Carrier | Type | From → To | Payload |
|---------|------|-----------|---------|
| HAL/app → detector | `std::sync::mpsc::Sender<Msg>` (`platform.rs`) | mic/device callbacks + test controller → detect worker | `Signal` / `DeviceChanged` / acknowledged `Pause` / `Resume` / `Shutdown` |
| capture data plane | `rtrb::Producer<f32>` / `Consumer<f32>` (SPSC, wait-free) | `io_proc` → writer thread | interleaved f32 frames (mic channels first, then tap — the me/them contract). Ring sized `sample_rate · 8ch · RING_SECONDS` (`ring_capacity`, `capture.rs:134`, default 30 s, `:60`). |
| detector → app | closure callback (`DetectorEvent`) | detect worker → `handle_detector_event` (`main.rs:381`) | `RecordingStarted` / `RecordingFinished{meta, audio_path}` / `RecordingDiscarded` / `Error` (`crates/corti-detect/src/lib.rs:43`) |
| app → pipeline | `std::sync::mpsc::Sender<PipelineMsg>` (`main.rs:319`) | detector callback + webinar-finish thread + Queue-window Retry + corti-live thread → pipeline worker | `Process{meta, audio_path}` / `Retry{id}` / `ReloadConfig` / #87's `LiveNoteCreated{meta, note_path}` + `LiveDiscarded{id}` (`pipeline.rs:48`) |
| capture tee (#87/#103) | bounded `SyncSender<CaptureChunk>` in a `CaptureTee` (`TEE_BACKLOG = 2048`, fixed ≤~64 MiB at 48 kHz) | writer thread → `corti-live` thread | ~4096-frame downmixed mono `(mic, tap)` chunks; `try_send` only — full ⇒ dropped + counted, the WAV is untouched |
| mic-test ring + tee (#105) | fixed `rtrb` + `CaptureTee` (128 chunks) | direct default-input IO proc → mic worker → test ASR | mono `mic`, empty `tap`; no disk writer |
| live reader event | managed bounded store + `live-transcript-changed` | call/test worker → Tauri webview | monotonic timestamped row/state deltas; open-late snapshot repairs/re-hydrates |

The `mpsc` control channels carry coarse events with file paths; the `rtrb` ring is the primary
data-plane carrier, plus (#87) the bounded lossy tee that hands the live consumer its downmixed
copy — see [audio-pipeline.md](audio-pipeline.md) and [streaming.md](streaming.md).

## Ownership chains

The detector is a managed Tauri singleton whose whole point is to keep the worker thread alive:

```
Tauri managed-state registry
  └─ DetectorHandle(Mutex<Detector>)          main.rs:198 (managed :371) — Mutex is NEVER locked;
       └─ Detector{ ctrl: Sender<Msg>,          it only makes the !Sync Detector Send+Sync so it
                    worker: JoinHandle }         can be managed. Drop → Msg::Shutdown + join
            └─ Worker (on the spawned thread)    (platform.rs:118).
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
   (`crates/corti-vagus/src/lib.rs:102`).
2. **The detector recording WAV is still the source of truth — live transcription is a tee, not a replacement.**
   The detector capture path always streams f32 frames to the 2-track WAV; the #87 live path consumes a
   bounded lossy *copy* and can never stall or corrupt the recording. When live filing is eligible
   (local backend + selected ASR/shared configured models + `live_filing`), bounded transcript windows are diarized and
   OS-synced during the call and batch transcription is skipped; on any live-path failure — or for webinar/manual captures, which have
   no live hook — the pipeline falls back to the batch `Backend::transcribe(path)` over the finished
   file. The `Transcriber` trait still takes a `&Path` to a complete 2-track WAV
   (`crates/corti-transcribe/src/lib.rs:23`); the chunked surface is `LiveTranscriber`
   (ADR 0009) — see [streaming.md](streaming.md) and [transcription.md](transcription.md).
3. **The Live Transcript window is a bounded observer, not a new authority.** Closed VAD regions are copied
   into a 2,000-row/~1 MiB transient store and pushed to the webview with call-relative timestamps; durable
   filing/fallback remains unchanged. The explicit idle microphone test is the one capture exception to point
   2: it opens the input device directly and intentionally creates no tap, aggregate, WAV, queue row, or note
   (ADR 0013).

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
