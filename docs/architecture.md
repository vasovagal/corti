# Architecture — the whole-pipeline graph

Corti's detector captures calls to a WAV and files a Vagus note. Eligible local recordings file
bounded windows during the call; AWS, manual captures and live failures use the batch pipeline.
This page maps capture and filing ownership. See [app.md](app.md) for the UI and hosted-service
threads, and [quality.md](quality.md) for loss accounting.

Capture and local decoding run in-process on OS threads. HAL callbacks feed fixed PCM and
descriptor rings; control events use `std::sync::mpsc`. The graph omits quality descriptors,
format observation and the UI/hosted workers; the tables below identify those boundaries.

## The graph

```
                          ┌──────────────────────── one process ────────────────────────┐
  CoreAudio HAL threads   │                                                              │
  ─────────────────────   │   corti-detect worker thread          corti-pipeline thread │
  MicMonitor  ┐           │   ("the poller")                      (writable Queue owner)     │
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
                          │   │   drains ring ──► AEC ──► hound ──► WAV   │      (blocking)  │
                          │   └───────│──────────────────────────┘       │                 │
                          │           │ try_send (bounded lossy CaptureTee, #87)            │
                          │           ▼                                                     │
                          │   corti-live thread (one per recording, when live filing is on) │
                          │     StreamingAec → 2× LiveTranscriber (CPU or Metal) → checkpoint│
                          │     → optional bounded diarize → append + sync_all; final flip   │
                          └──────────────────────────────────────────────────────────────┘
```

The detector and pipeline are long-lived workers. Each recording adds a capture writer and,
when eligible, a live consumer. AppKit mutations run on the Tauri main thread.

The batch path is `enqueue → transcribe → file → Done`. A successfully filed live note skips
batch decoding. The pipeline's `recv_timeout` loop also drains durable retry jobs and the hourly
retention sweep, serially. A fresh batch or first live fallback has six backend attempts;
automatic recovery retains the original backend. See
[transcription.md](transcription.md#live-inbox-filing-87) and [quality.md](quality.md#capture-162-138).

## Threads

| Thread | Spawned | Owns / does |
|--------|---------|-------------|
| Tauri main | `app.run` (`app/src/main.rs`) | The event loop; every tray/menu/window mutation is marshalled here via `run_on_main_thread`. |
| `corti-detect` worker | `Detector::start` (`crates/corti-detect/src/platform.rs`) | `Machine` + `MicMonitor` + `DefaultInputDeviceMonitor` + the in-flight `Recorder`. The state machine + poll loop. |
| `corti-pipeline` | `app/src/main.rs` | Owns the writable `Queue`; a tick loop (`run`, `app/src/pipeline.rs`) that drains `PipelineMsg` **and** due `corti-jobs` (retry/sweep) serially. |
| `corti-capture-writer` (one per recording) | `crates/corti-coreaudio/src/capture.rs` | Drains PCM and quality descriptors, normalizes source epochs to the retained rate, and writes the WAV with `hound` (`capture_writer::run_writer_with_intent`). |
| `corti-live` (one per eligible detector recording, #87/#103/#105) | `LiveManager::spawn` via the detector's `LiveHook` | Drains the bounded tee → streaming AEC + two channels sharing the selected resident Parakeet ASR (`sherpa`/CPU or `transcribe.cpp`/Metal); publishes closed regions to the bounded reader, files at settled natural or forced boundaries, optionally diarizes only that bounded far-end window, appends + `sync_all`s once, then syncs the final `State:` flip. Panic-contained; any failure preserves prior chunks and falls back to the same note. |
| `corti-live-test` + `corti-mic-test-capture` (#105) | contextual idle tray action | One selected local ASR/VAD channel plus direct default-input SPSC-ring consumer. No tap, WAV, note, or queue row; detector edges/background jobs are deferred until cleanup. |
| `corti-blink`, `corti-stats` | `app/src/tray.rs`, `app/src/stats.rs` | Icon-swap animation; 1 Hz stats sampler. |
| CoreAudio HAL callback threads | OS/CoreAudio | `MicMonitor`/`DefaultInputDeviceMonitor` trampolines and `io_proc`. Mic/device listeners send control signals; the capture callback admits PCM plus descriptors without blocking or allocating. Capture-format callbacks invalidate an atomic mailbox for an off-HAL worker. |

See [app.md](app.md) for the tray/window/command surface and the full thread inventory.

## Channels & rings

| Carrier | Type | From → To | Payload |
|---------|------|-----------|---------|
| HAL/app → detector | `std::sync::mpsc::Sender<Msg>` (`platform.rs`) | mic/device callbacks + test controller → detect worker | `Signal` / `DeviceChanged` / acknowledged `Pause` / `Resume` / `Shutdown` |
| capture data plane | `rtrb::Producer<f32>` / `Consumer<f32>` (SPSC, wait-free) | `io_proc` → writer thread | interleaved f32 frames (mic channels first, then tap — the me/them contract). Ring sized `sample_rate · 8ch · RING_SECONDS` (`ring_capacity`, `capture.rs`, default 30 s). |
| capture accounting | fixed `rtrb` descriptor ring | `io_proc` → writer thread | Ordered epochs, source losses and PCM admission descriptors; loss must not become silent completeness. |
| detector → app | closure callback (`DetectorEvent`) | detect worker → `handle_detector_event` (`main.rs`) | `RecordingStarted` / `RecordingFinished{meta, audio_path}` / `RecordingDiscarded` / `Error` (`crates/corti-detect/src/lib.rs`) |
| app → pipeline | `std::sync::mpsc::Sender<PipelineMsg>` (`main.rs`) | detector callback + webinar-finish thread + Queue-window Retry + corti-live thread → pipeline worker | `Process{meta, audio_path}` / `Retry{id}` / `ReloadConfig` / #87's `LiveNoteCreated{meta, note_path}` + `LiveDiscarded{id}` (`pipeline.rs`) |
| capture tee (#87/#103) | bounded `SyncSender<CaptureChunk>` in a `CaptureTee` (`TEE_BACKLOG = 2048`, ≤~72 MiB PCM/availability plus <1 MiB chunk metadata) | writer thread → `corti-live` thread | At most 4096 retained frames with mono `mic`/`tap`, epoch, retained offset and availability; `try_send` only — full ⇒ dropped + counted, the WAV is untouched |
| mic-test ring + tee (#105) | fixed `rtrb` + `CaptureTee` (128 chunks) | direct default-input IO proc → mic worker → test ASR | mono `mic`, empty `tap`; no disk writer |
| live reader event | managed bounded store + `live-transcript-changed` | call/test worker → Tauri webview | monotonic timestamped row/state deltas; open-late snapshot repairs/re-hydrates |

The `mpsc` control channels carry coarse events with file paths; the `rtrb` ring is the primary
data-plane carrier, plus (#87) the bounded lossy tee that hands the live consumer its downmixed
copy — see [audio-pipeline.md](audio-pipeline.md) and [streaming.md](streaming.md).

## Ownership chains

The managed detector owns its worker until shutdown:

```
Tauri managed-state registry
  └─ DetectorHandle(Mutex<Detector>)          live-test controller locks to pause/resume
       └─ Detector{ ctrl: Sender<Msg>,          the mutex also makes the managed holder Sync
                    worker: JoinHandle }         Drop → Msg::Shutdown + join
            └─ Worker (on the spawned thread)    (platform.rs).
                 ├─ Machine                      the debounce/coalesce state machine
                 ├─ MicMonitor (rebindable)      HAL listener on the default input device
                 ├─ DefaultInputDeviceMonitor    rebinds MicMonitor on input-device switch
                 └─ current: Option<(Recorder, RecordingMeta)>
                        └─ Recorder              corti-capture
                             └─ CaptureSession   owns tap + aggregate device + io_proc,
                                  ├─ Cap          PCM/descriptor producers; HAL owns them until stop
                                  ├─ FormatObserver   off-HAL format worker
                                  └─ JoinHandle   the writer thread
```

The pipeline owns its writable rusqlite `Queue` connection. Queue-window reads open separate
read-only connections (`app/src/queue_ui.rs`); they do not share the pipeline connection.
The tray rebuilds its menu from managed state (`build_menu`, `tray.rs`). See [app.md](app.md)
for app-side ownership.

## Corrections

Keep these boundaries when changing the pipeline:

1. **The recorder is in-process, not a subprocess.** `Recorder::start`
   (`crates/corti-capture/src/lib.rs`) builds a `CaptureSession` that installs a CoreAudio
   `io_proc` and spawns an in-process writer thread. No child process is forked to record. Note creation crosses a subprocess boundary through the external `vagus` CLI
   (`crates/corti-vagus/src/lib.rs`). Other app actions can also launch processes, such as `open` or model extraction.
2. **The detector recording WAV is still the source of truth — live transcription is a tee, not a replacement.**
   The detector capture path always streams f32 frames to the 2-track WAV; the #87 live path consumes a
   bounded lossy *copy* and can never stall or corrupt the recording. When live filing is eligible
   (local backend + selected ASR/shared configured models + `live_filing`), bounded transcript windows are diarized and
   OS-synced during the call and batch transcription is skipped; on any live-path failure — or for webinar/manual captures, which have
   no live hook — the pipeline falls back to batch decoding over the finished file, retaining recorded quality limits. The `Transcriber` trait still takes a `&Path` to a complete 2-track WAV
   (`crates/corti-transcribe/src/lib.rs`); the chunked surface is `LiveTranscriber`
   (ADR 0009) — see [streaming.md](streaming.md) and [transcription.md](transcription.md).
3. **The Live Transcript window is a bounded observer, not a new authority.** Closed VAD regions are copied
   into a 2,000-row/~1 MiB transient store and pushed to the webview with call-relative timestamps; durable
   filing/fallback remains unchanged. The explicit idle microphone test is the one capture exception to point
   2: it opens the input device directly and intentionally creates no tap, aggregate, WAV, queue row, or note
   (ADR 0013).

## Async islands

corti is **sync-core: no tokio** in `corti-detect`, `corti-capture`, `corti-coreaudio`, or
`corti-aec` (their `Cargo.toml`s carry no tokio/futures). All pipelining is std threads + `mpsc` +
`rtrb`. The blocking capture/local-decoder path does not require an async runtime. Its async boundaries include:

- **`tauri::async_runtime`** — Tauri's internal tokio, used for the startup microphone-permission
  check (`app/src/permissions.rs`) and the async `#[tauri::command]`s (e.g. `verify_aws` in
  `app/src/settings.rs`).
- **Private current-thread runtimes for AWS** — `build_sdk_config` builds a throwaway
  `new_current_thread` runtime and `block_on`s the credential load once at worker startup
  (`app/src/transcribe.rs`); the AWS transcriber spins its own current-thread runtime *inside*
  its blocking `transcribe()` (`crates/corti-transcribe-aws/src/lib.rs`). From the pipeline
  thread's view both are ordinary synchronous blocking calls behind the sync `Transcriber` trait.

The optional local [`stream` adapter](streaming.md#the-async-edge--stream-feature) bridges a worker
thread to a tokio channel; it does not move the synchronous decoder onto the capture thread.
