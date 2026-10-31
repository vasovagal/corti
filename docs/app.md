# app — the Tauri tray surface

This guide maps the tray, windows, state, and commands in the `app/` crate (`corti-app`, binary
`corti`). Start with thread ownership, then follow the detector handoff, UI, and settings flow.
The source entry points are [`main.rs`](../app/src/main.rs), [`tray.rs`](../app/src/tray.rs), and
[`pipeline.rs`](../app/src/pipeline.rs); design decisions live in the [ADRs](../design/adr/).

The app starts without a window and uses Tauri's event loop. Its run callback vetoes implicit
`ExitRequested { code: None }` events: closing the last utility window or pressing Cmd-Q returns
to the tray; the tray's explicit `app.exit(0)` quits. `AppState` holds capture flags, status, and
history. Other managed holders own configuration, live text, hosted processing, and diagnostics.

## Threads

Every AppKit / tray / window mutation is marshalled to the Tauri main thread via
`AppHandle::run_on_main_thread`. The capture and pipeline threads:

| Thread | Spawned | Role |
|---|---|---|
| Tauri main | `app.run` (`main.rs`) | event loop; owns all AppKit/tray/window mutation |
| `corti-pipeline` | `main.rs` | sole owner of the writable `corti_queue::Queue` connection; a tick loop that drains `PipelineMsg` **and** due `corti-jobs` (retry/sweep) serially — see [transcription.md](transcription.md) |
| `corti-detect` | inside `Detector::start` (`crates/corti-detect/src/platform.rs`) | the poller — mic-in-use state machine, owns the in-flight `Recorder` |
| `corti-stats` | `main.rs` | 1 Hz sampler → `StatsBuffer` ring |
| `corti-blink` | `tray.rs` | 500 ms tray-icon swap while recording |

Retries and the hourly retention sweep share the `corti-pipeline` tick loop with recordings. While the explicit microphone test
owns the local model gate, due jobs remain unclaimed and the loop rechecks at 250 ms instead of overlapping a
retry ASR session.

Per-recording, test, and platform workers:
- **`corti-webinar-finish`** — one per manual webinar stop (`main.rs`); writes the WAV, sends
  `PipelineMsg::Process`, exits.
- **`corti-capture-writer`** — one per recording, inside `CaptureSession`; streams the 2-track WAV.
- **`corti-live`** — one per live-eligible detector recording (`app/src/live.rs`); drains the
  fixed capture tee, runs streaming AEC/ASR, publishes closed regions to the bounded timestamped reader,
  optionally diarizes one bounded rolling window, then appends and OS-syncs that chunk. No transcript/audio
  collection grows with call duration. The detector's `LiveHook` delivers its recording-specific
  finish/discard verdict before the later pipeline event; the pipeline only collects the finished ID — see
  [transcription.md](transcription.md#live-inbox-filing-87).
- **`corti-live-test` + `corti-mic-test-capture`** — only during the explicit idle microphone test.
  The first owns one local ASR/VAD channel; the second drains a direct-input SPSC ring into bounded mono tee
  chunks. Detector edges are paused, and neither thread writes a WAV/note/queue row.
- **CoreAudio HAL callback threads** — capture IO procs transfer bounded PCM and descriptors; format
  callbacks invalidate a mailbox. `MicMonitor` reads the running property before signaling its worker.
  These callbacks never decode audio or touch a webview.

Capture and pipeline scheduling use `std::sync::mpsc` and standard threads. Tauri's async runtime
handles permission checks and async commands such as `verify_aws`; the AWS backend drives its own runtime.

<a id="appstate--the-single-source-of-truth"></a>

## Shared state

`app.manage(AppState::new())` registers shared capture status and history. The tray snapshots
this state in `build_menu` on each rebuild:

- `detector_recording`, `webinar_recording`, `live_test_active`: independent atomic ownership flags; the icon
  blinks while any capture/test is live, and tray actions use them to prevent overlap.
- `status: Mutex<String>` — the top menu line.
- `stage: AtomicU8` — the current pipeline `Stage` (`main.rs`), read by the `get_pipeline_activity`
  command for the How-Corti-Works window. A single last-writer-wins global (like `status`), so with
  overlapping jobs an older worker can clobber a live capture's `Recording` — the UI treats the
  `recording` flag as authoritative for the Detect/Capture boxes.
- `history: Mutex<VecDeque<HistoryEntry>>` — capped at `HISTORY_LIMIT = 5` (`main.rs`); the
  `History ▸` submenu's source (in-flight, failed, and filed recordings).

The `Backend:` / `Bucket:` summary lines are **not** in `AppState` — `build_menu` derives them live
from `settings::ConfigState` so they track saved edits (`main.rs`).

Three managed holders connect capture and queue ownership to commands:
- `DetectorHandle(Mutex<Detector>)` — keeping it alive keeps
  the `corti-detect` worker alive; the microphone test locks it to pause/resume detection.
  `Detector::drop` stops the worker and removes HAL listeners.
- `Webinar(Mutex<WebinarState>)` (`main.rs`) — the live tap-only recorder + a clone of the
  pipeline channel.
- `queue_ui::PipelineTx(Mutex<Sender<PipelineMsg>>)` — a clone of the
  pipeline channel so the Recording Queue window's Retry command messages the queue-owning thread
  rather than writing the DB directly.

## Detector → pipeline handoff

```
mic-in-use (CoreAudio) ─► corti-detect worker ─► Recorder ─► DetectorEvent
                                                                  │  (on the detect thread)
                                                       handle_detector_event (main.rs)
                                                                  │
                              detector_recording flag + tray  ◄───┤
                                                                  │  PipelineMsg::Process
                                                                  ▼  (std::sync::mpsc, main.rs)
                                              corti-pipeline worker ─► transcribe ─► vagus ─► Done
```

`Detector::start` receives a callback with a cloned `AppHandle` and `pipe_tx`.
On `RecordingFinished`, it sends `PipelineMsg::Process`. The manual webinar path sends the same
message from `corti-webinar-finish` after capture stops. Queue rows back tray history and
`corti --list`; startup seeds recent history and recovers orphaned jobs.
See [pipeline and retry ownership](transcription.md#the-pipeline-worker).

**Live inbox filing** uses the same handoff: `Detector::start_with_live_hook` carries an
`AppLiveHook` that spawns a per-recording `corti-live` thread when eligible. On capture termination the
detector delivers `finished(meta)` or `discarded(meta)` **before** emitting its downstream event.
`LiveManager` keeps finish-delivered sessions by recording ID; at `Process` time the worker calls
`collect(id)`, and only a `Filed` result with no dropped tee chunks and complete settled quality skips batch. A lossy/error result carries
its partial path into batch rewrite; a discard stays in the single-model gate under a manager-owned reaper
and reports persistent unlink failure back to the pipeline. Live telemetry adds no new stage — it remains
`Recording` during the call.
Errors the recording survives (for example a mic-monitor rebind) never deliver a terminal verdict. See
[transcription.md](transcription.md#live-inbox-filing-87).

## Tray

`build_tray` creates `corti-tray` with compile-time `ICON_IDLE` / `ICON_REC` template icons.
Tauri owns the icon under `TRAY_ID`; updates retrieve it through `app.tray_by_id(TRAY_ID)`.

`build_menu` rebuilds and swaps the whole menu from current state. The global
`Builder::on_menu_event` handler routes clicks after a menu swap. Items appear in this order:

| id | kind | notes |
|---|---|---|
| `status` | disabled line | `AppState.status` |
| `live_transcript` | contextual action | call: **Read live transcript…**; idle: **Test microphone & live transcription…**; active test: read it; webinar: disabled |
| `webinar_toggle` | action | `▶ Start` / `■ Stop` from `webinar_recording` |
| `History ▸` | submenu | up to 5; each id `note::<path>` when filed+clickable, else disabled `history::<id>` |
| `backend` | disabled | live summary from `ConfigState` |
| `bucket` | disabled | aws-only |
| `open_queue` | action | Recording Queue window |
| `open_settings` | action | Settings window |
| `ethics_guide` | action | Ethics & Legality guide |
| `open_how` | action | How Corti Works window |
| `open_diagnostics` | action | Diagnostics console |
| `open_privacy` | action | opens the Screen & System Audio pane |
| `quit` | action | `app.exit(0)` |

Recording Queue shows the durable queue; `History ▸` shows the five newest recordings.
Diagnostics owns the stats display; there is no separate stats window.

<a id="blink"></a>

**Blink.**

`corti-blink` wakes every 500 ms. While any detector, webinar, or microphone-test flag is active,
it alternates the two template icons on the main thread; otherwise it shows `ICON_IDLE`.
A `shown: Option<bool>` suppresses unchanged icon updates.

<a id="windows--the-view--activation-policy-dance"></a>

## Windows

Six on-demand `WebviewWindow`s, all built from the *same* SPA `index.html`, differentiated by a
`?view=` query param parsed in `app/ui/src/main.tsx` (including `?view=live`). All six use the
shared `open_app_window` singleton/focus/activation-policy lifecycle:

| Window | view | opener |
|---|---|---|
| Ethics & Legality guide | *(none, default)* | `open_ethics_window` |
| Preferences | `?view=settings` | `open_settings_window` |
| Recording Queue | `?view=queue` | `open_queue_window` |
| Live Transcript / mic test | `?view=live` | contextual `live_transcript` tray action |
| Diagnostics / console | `?view=console` | `open_console_window` |
| How Corti Works | `?view=how` | `open_how_window` |

The **How Corti Works** window (`app/ui/src/How.tsx`) renders the pipeline as a row of boxes and
polls `get_pipeline_activity` (`app/src/activity.rs`) to pulse the active stage. The Rust `Stage`
ids are mirrored in `app/ui/src/lib/pipeline.ts`; because the shared `stage` is last-writer-wins, the
UI applies the `recording` flag as an override — Detect + Capture pulse together whenever a capture is
live, regardless of the reported stage (`activeBoxKeysForActivity`, `pipeline.ts`).

The **Recording Queue** window (`app/ui/src/Queue.tsx`) offers Retry, Reveal audio, and Open note.
Its [command and event flow](#commands-and-events) keeps reads off the queue writer.

Each is a singleton (focus-if-exists). The app launches windowless with
`ActivationPolicy::Accessory` (no Dock icon, set at `setup`). The shared `open_app_window` flips to
`ActivationPolicy::Regular`, explicitly activates `NSApplication` on the AppKit main thread, then
unminimizes/shows/focuses the centered window; this prevents tray-opened windows hiding behind the current
app. Opening `live` reserves a manager-owned window generation before any test can start. On
`WindowEvent::Destroyed`, that exact generation is invalidated and requests `LiveTestManager::stop()` (a no-op
while reading a real call). A queued command from the destroyed generation is rejected, and a canceled
startup keeps its slot until detector/model cleanup completes, so stale cleanup cannot clobber a fast reopen.
`revert_activation_policy_if_no_windows` then drops back to `Accessory` once the last window closes.

<a id="command-surface--pull-plus-one-push"></a>

## Commands and events

Windows invoke commands registered in [`main.rs`](../app/src/main.rs). This subset groups the
main surfaces; the registration list and [`capabilities/`](../app/capabilities/) define the full API:

```
get_config · set_config · get_backends · get_aws_status · verify_aws · get_paths ·
reveal_path · set_models_dir · get_models_status · get_embedding_models · download_model ·
get_console_logs · get_console_logs_text · save_console_logs · get_stats · get_pipeline_activity ·
list_recordings · retry_recording · open_note · reveal_audio          ← Recording Queue (#85)
get_live_transcript · get_live_test_window_generation · start_live_test · stop_live_test
                                                                        ← Live Transcript (#105/#129)
get_hosted_settings · patch_hosted_settings · refresh_hosted_provider ·
start_chatgpt_device_login · cancel_chatgpt_device_login ·
sign_out_chatgpt_subscription · open_chatgpt_device_login             ← Hosted Preferences (#130)
submit_hosted_question · cancel_hosted_question · get_hosted_assistant ·
set_hosted_subscriptions · run_hosted_subscription_now                  ← Assistant / subscriptions (#158)
```

The hosted coordinator lives in the platform-independent `corti-chat` crate (ADR 0017); the app's
`postprocess_app.rs` service batches finalized rows into Live requests, builds the request
for each subscription the coordinator reports due, copies accepted clean text into the session ledger, and
re-reads the word bank and the learned lexicon at every session begin. The headless CLI exposes
`corti --review <note.md>…` and `corti --lexicon list|add|remove|test` for that lexicon.

The Diagnostics console polls `get_stats` on a 1 s `setInterval` (`Console.tsx`); the How
window polls `get_pipeline_activity` (`app/src/activity.rs`) the same way. The Recording Queue
window (`queue_ui.rs`) is the exception to pure polling — it pulls `list_recordings` once and then
refetches on the pushed **`queue-changed`** event, whose reads go through a per-call **read-only** SQLite
connection so the pipeline thread stays the DB's only writer.

The Live Transcript window subscribes before its initial `get_live_transcript` snapshot, applies only
increasing revisions/row sequences, and reconciles slowly in case WKWebView suspended an event. The managed
store retains at most 2,000 rows/~1 MiB; the Vagus note remains durable authority.

ChatGPT subscription login is app-owned rather than server-owned: a bounded `corti-chatgpt-login` thread
polls OpenAI's fixed device endpoint, while React sees only the verification URL, user code, and secret-free
credential state. The rotating credential is one private document in the app-owned
`hosted-secrets/` store: directory mode `0700`, file mode `0600`, atomic writes, and symlink/owner
checks ([`secret_store.rs`](../app/src/secret_store.rs)). It never enters the webview. Catalog and inference
requests go directly to fixed `chatgpt.com/backend-api/codex/{models,responses}` endpoints; no Codex process is
spawned and no credential from Codex, Pi, or Dekopon is imported. Each poller is bound to one opaque login id,
so cancel/restart cannot let a stale worker mutate its replacement. Account identity derives an opaque live
request/cache scope rather than a separately persisted value. A credential-store save failure projects not-saved
state and prevents new dispatch, while preserving the just-rotated token for an already in-flight request.

Rust→JS push events: **`live-transcript-changed`** — tiny state/row deltas from the bounded store;
**`model-download-progress`** — `app.emit(...)` in `settings.rs` for
the Settings model-downloader progress bar — and **`queue-changed`** (`tray::emit_queue_changed`),
emitted whenever any tray-history change moves so the Queue window refreshes.
`settings-navigation-requested` wakes Preferences to read a backend-owned section request.
`StatsSnapshot` also carries the recording flags for polling clients.

## Config / settings flow

`AppConfig` is loaded once and shared as `SharedConfig = Arc<Mutex<AppConfig>>`; the managed
`ConfigState { config, reload_tx }` (`main.rs`) holds it plus a clone of the pipeline sender.
When the Settings window saves, `set_config` writes the shared config and sends
`PipelineMsg::ReloadConfig` via `reload_tx`; the serial worker rebuilds its backend + AEC toggle between
jobs — or immediately if idle. Local settings include `asr_engine = sherpa | ggml`: standard builds compile
both, while a minimal local build disables GGML in the UI. The model table follows the draft selector and
shows only that engine's Parakeet artifact plus shared VAD/configured diarization artifacts. The
retention sweep reads `retention_days` live from the same `SharedConfig`, so a saved change applies
to the next sweep with no reload; the live-filing hook snapshots the config at each recording start,
so `live_filing` and its rolling interval need no reload either. Env knobs (`CORTI_TRANSCRIBE_BACKEND`,
`CORTI_AWS_BUCKET`, `CORTI_LANGUAGE`, `CORTI_LOCAL_*`, `CORTI_RETENTION_DAYS`, `CORTI_LIVE_FILING`,
`CORTI_LIVE_BUFFER_MINUTES`) still seed the initial config (`config.rs`), including
`CORTI_LOCAL_ASR_ENGINE` and the advanced `CORTI_LOCAL_GGML_MODEL` path override. `live_filing` defaults on; the
Settings screen also exposes `live_buffer_minutes` (default 1, range 1–10), the nominal interval
between diarized, synced transcript commits. A speech boundary may defer a commit by up to 22 seconds;
see [live boundaries](quality.md#live-boundaries-155). Both apply to the next recording.

## Diagnostics console + stats sampler

**Console** ([`console.rs`](../app/src/console.rs)): `ConsoleLayer` captures events into a bounded
`ConsoleBuffer` shared with `get_console_logs*` and `save_console_logs`. A separate daily-rolling
file layer writes under `<data_dir>/logs/` with the base name `corti.log`.

**Stats** ([`stats.rs`](../app/src/stats.rs)): the 1 Hz `corti-stats` thread samples process RSS/CPU
and recording flags into a `StatsBuffer`. `get_stats` returns recent samples and the completed
stage timings recorded by the pipeline worker. Sampling stays off the capture and pipeline threads.
