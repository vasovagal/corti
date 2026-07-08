# app — the Tauri tray surface

> Verified against **v0.8.0 + feat/pipeline-docs-and-streaming** (2026-07-07). Current-state
> internals of the `app/` crate (`corti-app`, bin `corti`); `file.rs:line` anchors point into this
> worktree. Design rationale lives in `design/05-app-tauri.md` (partly stale) and the ADRs.

The app is a windowless macOS menu-bar agent. One OS process; no `#[tokio::main]` — it runs on
Tauri's own event loop (`app.run(|_,_| {})`, `app/src/main.rs:280`). All live state is a single
managed `AppState`; the tray is rebuilt from it on every change. Windows are on-demand and almost
entirely poll-based.

## Threads

Every AppKit / tray / window mutation is marshalled to the Tauri main thread via
`AppHandle::run_on_main_thread` (`tray.rs`). The long-lived threads:

| Thread | Spawned | Role |
|---|---|---|
| Tauri main | `app.run` (`main.rs:280`) | event loop; owns all AppKit/tray/window mutation |
| `corti-pipeline` | `main.rs:326` | **sole** `corti_queue::Queue` owner; drains `PipelineMsg`, runs `enqueue → transcribe → file → Done` serially |
| `corti-detect` | inside `Detector::start` (`crates/corti-detect/src/platform.rs`) | the poller — mic-in-use state machine, owns the in-flight `Recorder` |
| `corti-stats` | `main.rs:335` | 1 Hz sampler → `StatsBuffer` ring |
| `corti-blink` | `tray.rs:279` | 500 ms tray-icon swap while recording |

Transient / not app-owned:
- **`corti-webinar-finish`** — one per manual webinar stop (`main.rs:502`); writes the WAV, sends
  `PipelineMsg::Process`, exits.
- **`corti-capture-writer`** — one per recording, inside `CaptureSession`; streams the 2-track WAV.
- **CoreAudio HAL callback threads** — `MicMonitor` listeners; they only `tx.send(Msg::…)` and never
  touch capture (guardrail 9).

Deliberately no tokio in the capture/pipeline path — `std::sync::mpsc` + std threads throughout.
`tauri::async_runtime` (Tauri's internal tokio) carries only the startup mic-permission check
(`permissions.rs:24`) and the async `#[tauri::command]`s (`verify_aws`).

## AppState — the single source of truth

`app.manage(AppState::new())` (`main.rs:294`) registers one managed singleton, read/written from
every background thread. The tray owns no state — it snapshots this on each rebuild
(`build_menu`, `tray.rs:47`):

- `detector_recording: AtomicBool`, `webinar_recording: AtomicBool` — independent capture-source
  flags; the icon blinks while *either* is true, and they never clobber each other.
- `status: Mutex<String>` — the top menu line.
- `stage: AtomicU8` — the current pipeline `Stage` (`main.rs:115`), read by the `get_pipeline_activity`
  command for the How-Corti-Works window. A single last-writer-wins global (like `status`), so with
  overlapping jobs an older worker can clobber a live capture's `Recording` — the UI treats the
  `recording` flag as authoritative for the Detect/Capture boxes.
- `history: Mutex<VecDeque<HistoryEntry>>` — capped at `HISTORY_LIMIT = 5` (`main.rs:168`); the
  `History ▸` submenu's source (in-flight, failed, and filed recordings).

The `Backend:` / `Bucket:` summary lines are **not** in `AppState` — `build_menu` derives them live
from `settings::ConfigState` so they track saved edits (`main.rs:171`).

Two more managed holders exist only to keep non-`Sync` objects alive so they can be `manage`d, and
are never locked for access:
- `DetectorHandle(Mutex<Detector>)` (`main.rs:192`, managed `main.rs:346`) — keeping it alive keeps
  the `corti-detect` worker alive; `Detector::drop` stops the worker and removes HAL listeners.
- `Webinar(Mutex<WebinarState>)` (`main.rs:198`) — the live tap-only recorder + a clone of the
  pipeline channel.

## Detector → pipeline handoff

```
mic-in-use (CoreAudio) ─► corti-detect worker ─► Recorder ─► DetectorEvent
                                                                  │  (on the detect thread)
                                                       handle_detector_event (main.rs:356)
                                                                  │
                              detector_recording flag + tray  ◄───┤
                                                                  │  PipelineMsg::Process
                                                                  ▼  (std::sync::mpsc, main.rs:309)
                                              corti-pipeline worker ─► transcribe ─► vagus ─► Done
```

`Detector::start` gets a callback that captures a cloned `AppHandle` and `pipe_tx`
(`main.rs:344`); on `RecordingFinished` the callback sends `PipelineMsg::Process { meta, audio_path }`
(`main.rs:382`). The manual **webinar** path (`imp::toggle`, `main.rs:455`) rejoins the *same*
pipeline: on stop the transient `corti-webinar-finish` thread sends an identical `Process`
(`main.rs:577`). Pipeline internals — the sole-`Queue`-owner rule and deferred durability — are in
[`transcription.md`](transcription.md); note only that crash recovery is **not** wired (ADR 0007, `pipeline.rs:9-13`):
the queue is a session-spanning record for tray history and `corti --list`, seeded newest-5 at
startup (`seed_history`, `pipeline.rs:167`).

## Tray

`build_tray` (`tray.rs:34`) builds `TrayIconBuilder::with_id("corti-tray")` (`tray.rs:23,36`) with
two compile-time template icons — `ICON_IDLE` / `ICON_REC` via `include_image!` (`tray.rs:27-28`),
`icon_as_template(true)`. The `TrayIcon` is owned by the Tauri runtime under `TRAY_ID`; every later
mutation re-fetches it via `app.tray_by_id(TRAY_ID)` (`tray.rs:265`).

**Rebuild-wholesale.** `build_menu` (`tray.rs:47`) reconstructs the *entire* menu from `AppState`
on every change and swaps it via `TrayIcon::set_menu`. Nothing mutates a live menu item. Because the
menu is swapped, clicks route through a **global** `Builder::on_menu_event` → `handle_menu_event`
(`main.rs:249`; `tray.rs:318`) rather than a per-menu handler, so dynamically-swapped menus still
fire. Items, in order:

| id | kind | notes |
|---|---|---|
| `status` | disabled line | `AppState.status` (`tray.rs:54`) |
| `webinar_toggle` | action | `▶ Start` / `■ Stop` from `webinar_recording` (`tray.rs:70`) |
| `History ▸` | submenu | up to 5; each id `note::<path>` when filed+clickable, else disabled `history::<id>` (`tray.rs:77`) |
| `backend` | disabled | live summary from `ConfigState` (`tray.rs:110`) |
| `bucket` | disabled | aws-only (`tray.rs:118`) |
| `open_settings` | action | Settings window (`tray.rs:126`) |
| `ethics_guide` | action | Ethics & Legality guide (`tray.rs:133`) |
| `open_how` | action | How Corti Works window (`tray.rs:140`) |
| `open_diagnostics` | action | Diagnostics console (`tray.rs:147`) |
| `open_privacy` | action | opens the Screen & System Audio pane (`tray.rs:154`) |
| `quit` | action | `app.exit(0)` (`tray.rs:162,321`) |

There is **no** dedicated Recording-Queue or stats window — the `History ▸` submenu is the recording
surface, and stats are polled inside the Diagnostics console. (The Recording Queue UI lives on the
unmerged `feat/opus-independent-stack`; see `design/STATUS.md`.)

### Blink

`spawn_blink` (`tray.rs:279`) runs a plain `corti-blink` std thread — deliberately independent of any
tokio runtime. It loops `sleep(500ms)`; reads `detector_recording || webinar_recording`; while
recording it toggles a local `phase` and swaps `ICON_REC`/`ICON_IDLE` via `tray.set_icon` marshalled
to the main thread; idle it rests on `ICON_IDLE`. A `shown: Option<bool>` dedups so `set_icon` fires
only on an actual change (`tray.rs:302`). The throb is a 500 ms two-frame template swap driven by the
two `AtomicBool`s — nothing else.

## Windows — the `?view=` + activation-policy dance

Four on-demand `WebviewWindow`s, all built from the *same* SPA `index.html`, differentiated by a
`?view=` query param parsed in `app/ui/src/main.tsx:11-14` (branched at `main.tsx:30`):

| Window | view | opener |
|---|---|---|
| Ethics & Legality guide | *(none, default)* | `open_ethics_window` (`tray.rs:340`) |
| Settings | `?view=settings` | `open_settings_window` (`tray.rs:383,395`) |
| Diagnostics / console | `?view=console` | `open_console_window` (`tray.rs:430,442`) |
| How Corti Works | `?view=how` | `open_how_window` (`tray.rs:478`) |

The **How Corti Works** window (`app/ui/src/How.tsx`) renders the pipeline as a row of boxes and
polls `get_pipeline_activity` (`app/src/activity.rs:23`) to pulse the active stage. The Rust `Stage`
ids are mirrored in `app/ui/src/lib/pipeline.ts`; because the shared `stage` is last-writer-wins, the
UI applies the `recording` flag as an override — Detect + Capture pulse together whenever a capture is
live, regardless of the reported stage (`activeBoxKeysForActivity`, `pipeline.ts:59`).

Each is a singleton (focus-if-exists). The app launches windowless with
`ActivationPolicy::Accessory` (no Dock icon, set at `setup`). Opening any window flips to
`ActivationPolicy::Regular` (`tray.rs:352,395,442,490`) so it can take focus; on
`WindowEvent::Destroyed`, `revert_activation_policy_if_no_windows` (`tray.rs:519`) drops back to
`Accessory` once the last window closes.

## Command surface — pull, plus one push

Windows read host state via `invoke` commands registered in `main.rs:252-269`:

```
get_config · set_config · get_backends · get_aws_status · verify_aws · get_paths ·
reveal_path · set_models_dir · get_models_status · get_embedding_models · download_model ·
get_console_logs · get_console_logs_text · save_console_logs · get_stats · get_pipeline_activity
```

The Diagnostics console polls `get_stats` on a 1 s `setInterval` (`Console.tsx:113,148`); the How
window polls `get_pipeline_activity` (`app/src/activity.rs:23`) the same way.

The **only** Rust→JS push event is **`model-download-progress`** — `app.emit(...)` in
`settings.rs:519,532,579` for the Settings model-downloader progress bar; JS `listen(...)` on the
other end. There is no push channel for recording/pipeline state today; a live indicator would poll
`get_stats` (whose `StatsSnapshot` already carries `detector_recording` / `webinar_recording`,
`stats.rs:161,163`) or add a new `emit`.

## Config / settings flow

`AppConfig` is loaded once and shared as `SharedConfig = Arc<Mutex<AppConfig>>`; the managed
`ConfigState { config, reload_tx }` (`main.rs:311`) holds it plus a clone of the pipeline sender.
When the Settings window saves, `set_config` writes the shared config and sends
`PipelineMsg::ReloadConfig` via `reload_tx` (`settings.rs`); the serial worker rebuilds its backend +
AEC toggle between jobs — or immediately if idle (`pipeline.rs:40,138`). Env knobs
(`CORTI_TRANSCRIBE_BACKEND`, `CORTI_AWS_BUCKET`, `CORTI_LANGUAGE`, `CORTI_LOCAL_*`) still seed the
initial config (`config.rs`).

## Diagnostics console + stats sampler

**Console** (`console.rs`): a `tracing` `ConsoleLayer` captures each event into a size-capped
`ConsoleBuffer` ring (`console.rs:71,134`) that the `get_console_logs*` / `save_console_logs`
commands read; a parallel **daily-rolling** file layer writes `<data_dir>/logs/corti.log`
(`console.rs:201,218`). The buffer is `manage`d so the layer and the Tauri command observe the same
entries.

**Stats** (`stats.rs`): `spawn_sampler` (`stats.rs:302`) runs the 1 Hz `corti-stats` thread — off the
pipeline thread (guardrail 9) — sampling process RSS/CPU (libc self-introspection) plus the recording
flags into a `StatsBuffer` ring. `get_stats` (`stats.rs:326`) returns a `StatsReport` (recent
samples + completed pipeline stage timings recorded by the worker via `StatsBuffer::record_stage`).
