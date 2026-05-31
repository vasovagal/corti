# 05 — app (Tauri 2 tray binary)

The menu-bar app that ties everything together: a blinking record icon while capturing, and the
detect → capture → (aec) → transcribe → vagus pipeline. **Built** ✅ — the crate is `app/` (`corti-app`,
bin `corti`); `cargo build`/`clippy -D warnings`/`fmt`/`test` are clean. See the "Built" section at the
bottom for what shipped, how to run, and how to bundle. The live join-call → note loop still needs a
**signed bundle** to exercise the TCC audio-capture grant (§1 of `LESSONS.md`).

## Shell / UX
- **Accessory activation policy** (`tauri::ActivationPolicy::Accessory`) → menu-bar only, no Dock icon.
- **Tray icon** via `TrayIconBuilder` in `setup`; **blink** by toggling between two template `Image`s on a
  ~500 ms `tauri::async_runtime::spawn` interval while recording; steady idle icon otherwise.
- Tray menu: status line, recent notes (open in vagus), settings, quit.

## Packaging & permissions (the part that bites — see LESSONS.md §1)
- Ship a real `.app` bundle (Tauri does this). `Contents/Info.plist` (merged by Tauri) must include
  **`NSAudioCaptureUsageDescription`** and **`NSMicrophoneUsageDescription`**. The audio-capture grant is
  what lets the process tap deliver audio.
- **Entitlements:** sign with BOTH `com.apple.security.device.audio-input` and
  `com.apple.security.device.microphone`, or the mic is silently denied in a signed build. Verify with
  `codesign -d --entitlements - -vvv corti.app`.
- Launching matters: a bundled, LaunchServices-launched app gets its own TCC identity (unlike a loose
  binary). The tray app is launched normally, so this is satisfied — but **test capture in a signed,
  notarized build**, not just `tauri dev`.
- Use `tauri-plugin-macos-permissions` to check/request mic + audio-capture status and route the user to
  System Settings on first run.

## Crate
Binary crate at `app/` (new workspace member; add to root `Cargo.toml` `members`, set `publish = false`).
Heavy deps (tauri, the aws/whisper backends) live here, keeping the library crates lean. Apple-Silicon +
latest-macOS only. Backend chosen by Cargo features: `default = ["aws"]`, `whisper` opt-in.

## Depends on
All library crates: corti-core, corti-coreaudio, corti-capture, corti-detect, corti-transcribe(+backend),
corti-queue, corti-vagus, corti-aec.

---

# Resume: concrete implementation plan (self-contained — start here)

Everything the app wires is implemented, unit-tested, and clippy/fmt-clean. Below are the **exact public
APIs** so this can be built without re-deriving them. Verify against the crates if anything drifts.

## The crate APIs the app calls

**corti-detect** (`crates/corti-detect/src/lib.rs`) — the trigger:
```rust
pub const DEBOUNCE: Duration;       // 1.5s rising-edge confirm
pub const COALESCE: Duration;       // 2s falling-edge / gap window
pub const MIN_RECORDING: Duration;  // 3s floor (shorter = discarded)
pub const POLL_INTERVAL: Duration;  // 1s process-attribution poll while recording

pub enum DetectorEvent {
    RecordingStarted  { meta: RecordingMeta },
    RecordingFinished { meta: RecordingMeta, audio_path: PathBuf },
    Error(String),
}
pub struct Detector; // macOS only
impl Detector {
    // on_event is invoked off the HAL thread (a worker thread). Keep the Detector alive;
    // Drop stops the worker + removes HAL listeners.
    pub fn start(on_event: impl Fn(DetectorEvent) + Send + 'static) -> anyhow::Result<Self>;
}
```

**corti-queue** (`crates/corti-queue/src/lib.rs`) — durability:
```rust
pub struct Queue;            // holds a rusqlite Connection — Send but NOT Sync (see threading note)
pub struct Job { pub id, started_at, ended_at, owning_app, bundle_id, audio_path,
                 status: JobStatus, s3_uri, transcribe_job, note_path, error, updated_at }
impl Job { pub fn meta(&self) -> RecordingMeta; }   // rebuild meta to resume
pub struct JobUpdate { pub status, s3_uri, transcribe_job, note_path, error } // all Option; partial update
impl Queue {
    pub fn open() -> Result<Self>;                                   // ~/.local/share/corti/queue.db (WAL)
    pub fn enqueue(&self, meta: &RecordingMeta) -> Result<String>;   // id = recording stem; IDEMPOTENT
    pub fn get(&self, id: &str) -> Result<Option<Job>>;
    pub fn set_status(&self, id: &str, status: JobStatus) -> Result<()>;
    pub fn update(&self, id: &str, fields: JobUpdate) -> Result<()>;
    pub fn fail(&self, id: &str, error: &str) -> Result<()>;
    pub fn resumable(&self) -> Result<Vec<Job>>;                     // non-terminal rows
    pub fn prune_done(&self, older_than: DateTime<Local>) -> Result<Vec<PathBuf>>; // returns WAVs to unlink
}
```

**corti-transcribe / -aws** (`crates/corti-transcribe*`):
```rust
pub trait Transcriber {  // synchronous/blocking by design
    fn transcribe(&self, audio: &Path, meta: &RecordingMeta) -> Result<DiarizedTranscript>;
}
// AwsTranscriber drives the async SDK on its OWN private tokio runtime inside transcribe(),
// so call it from a blocking task (spawn_blocking / std thread), NEVER the async UI thread.
impl AwsTranscriber {
    pub fn new(sdk_config: &aws_config::SdkConfig, opts: AwsOptions) -> Self;
}
pub struct AwsOptions { bucket, key_prefix, language, delete_after, poll_interval, max_wait }
impl AwsOptions { pub fn new(bucket: impl Into<String>) -> Self; } // Default: corti/, en-US, delete_after=true, 5s, 30min
```

**corti-vagus** (`crates/corti-vagus/src/lib.rs`) — the only vagus touchpoint:
```rust
impl Vagus {
    pub fn discover() -> Result<Self>;  // finds `vagus` on PATH ($VAGUS_BIN override), checks --version
    pub fn file_recording(&self, meta: &RecordingMeta, transcript: &DiarizedTranscript) -> Result<PathBuf>; // note path
}
```

**corti-core**: `RecordingMeta { started_at, ended_at, owning_app, audio_path }` (`note_title()`,
`source()`); `JobStatus::{Recording, PendingTranscription, Transcribing, PendingNote, Done, Failed}`
(`is_terminal()`); `DiarizedTranscript::to_markdown()`.

## Pipeline (maps 1:1 to JobStatus)
On `DetectorEvent::RecordingFinished { meta, audio_path }`:
1. `let id = queue.enqueue(&meta)?;`                          // → PendingTranscription
2. store the transcribe job name + `set_status(id, Transcribing)` (see idempotency tweak below), then run
   `transcriber.transcribe(&audio_path, &meta)?` on a **blocking task**.
3. `queue.set_status(id, PendingNote)?;` then `let note = vagus.file_recording(&meta, &transcript)?;`
   then `queue.update(id, JobUpdate { note_path: Some(note), ..Default::default() })?;`
4. `queue.set_status(id, Done)?;`
- Any step errs → `queue.fail(id, &format!("{e:#}"))?;` and surface in the tray.
- `DetectorEvent::Error(e)` / `RecordingStarted` → drive tray blink + status line; log errors.

## Startup crash recovery
`for job in queue.resumable()?` resume by `job.status`, reusing `job.meta()`:
- `PendingTranscription` → (re)transcribe.
- `Transcribing` → re-attach to the existing AWS job via `job.transcribe_job` (needs the tweak below) and
  continue polling — this is why idempotency matters (don't re-pay to re-submit).
- `PendingNote` → re-file via vagus **only if `job.note_path` is None** (else already filed; just mark Done).
- `Recording` → a capture orphaned by the crash; no recorder survives, so `queue.fail(id, "interrupted")`.

## Required small tweak to corti-transcribe-aws (idempotent re-poll)
The AWS backend currently mints a **unique-per-attempt** Transcribe job name internally, so a resumed
`Transcribing` job can't re-attach. Add `job_name: Option<String>` to `AwsOptions` (or a
`transcribe_with_job_name` method); when set, use it as the `transcription_job_name` and let the existing
`ConflictException`→poll path re-attach. The app: generate the name once (the recording id works), store it
via `queue.update(id, { transcribe_job: Some(name) })` **before** calling transcribe, and reuse it on resume.

## Threading model
- `Detector::start` callback already runs off the HAL thread → forward each `DetectorEvent` over a channel
  (e.g. `tauri::async_runtime` mpsc) to one **pipeline worker** that OWNS the `Queue` (rusqlite `Connection`
  is `Send` but not `Sync` — don't share it across tasks; one owner, or a `Mutex`, or open-per-thread).
- `transcribe()` blocks (its own runtime) → run on `spawn_blocking` / a dedicated thread, not the UI loop.
- Tray blink state flips on `RecordingStarted` / `RecordingFinished`.

## Config the app supplies
- **AWS:** build `SdkConfig` via `aws_config::defaults(BehaviorVersion::latest()).load().await` (standard
  credential chain) and pass to `AwsTranscriber::new`; log clearly if it fails. Bucket from app
  config/`CORTI_AWS_BUCKET`. Min IAM policy is in `02-corti-transcribe.md` (principal creds, no
  DataAccessRoleArn).
- **vagus:** `Vagus::discover()` at startup; surface a clear error if `vagus` isn't installed.
- **AEC:** `corti-aec` is a passthrough stub today — wire it as a no-op between capture and transcribe, or
  skip for v1 (note it; real NLMS is design 04).

## Verification
- `cargo build && cargo clippy --all-targets -- -D warnings && cargo fmt --check && cargo test` clean.
- `tauri dev` for UI/tray, but **capture only works in a signed `.app`** (TCC; LESSONS §1) — test the full
  join-call → note loop in a signed build.
- End-to-end is already provable from the CLI today (no GUI): `corti-detect --example watch` captures a WAV,
  `corti-transcribe-aws --example transcribe_file -- <wav>` transcribes it, and `corti-vagus` files it.

---

# Built — what shipped, how to run & bundle

Crate `app/` = package `corti-app`, binary `corti` (`publish = false`). Layout:
- `src/main.rs` — lifecycle + wiring: `Builder` → `setup` (Accessory policy, managed `AppState`, tray, blink,
  pipeline worker, detector, permission check) → `App::run` (windowless event loop). Holds the `Detector`
  alive in a managed `Mutex<Detector>` holder (`Detector` is `Send`, not `Sync`).
- `src/tray.rs` — template tray icons (embedded via `include_image!`, `icon_as_template(true)`), a
  ~500 ms **blink thread**, and the menu rebuilt from `AppState` on every change (status / recent notes /
  settings / quit). All AppKit mutations marshalled via `run_on_main_thread`. Menu clicks handled by a
  **global** `Builder::on_menu_event` (so dynamically-set menus still route).
- `src/pipeline.rs` — the single `Queue`-owning worker: crash recovery (`resumable()`) + `prune_done`, then
  serial `enqueue → store stable transcribe_job + Transcribing → transcribe (blocking) → write transcript
  sidecar → set PendingNote → file via vagus → note_path → Done`. Errors → `queue.fail` + tray status.
- `src/transcribe.rs` — feature-flavored backend (`Backend::transcribe`); AWS builds `SdkConfig` once and
  passes the recording id as the stable `AwsOptions.job_name`.
- `src/config.rs` — env config (`CORTI_AWS_BUCKET`, `CORTI_LANGUAGE`, `CORTI_WHISPER_MODEL`).
- `src/permissions.rs` — startup mic check via `tauri-plugin-macos-permissions` (host-side, no JS); only
  *requests* when running bundled (calling `request_microphone_permission` unbundled crashes — no plist).
- `Info.plist` (auto-merged: `LSUIElement`, `NSAudioCaptureUsageDescription`, `NSMicrophoneUsageDescription`),
  `Entitlements.plist` (`device.audio-input` + `device.microphone`), `icons/` (tray + bundle set,
  regenerable via `icons/generate-icons.py` + `cargo tauri icon`), `tauri.conf.json` (windowless: no
  `frontendDist`, `app.windows: []`).

Deliberate v1 choices:
- **AEC skipped** (the §"AEC" note's "skip for v1" branch): the raw 2-track WAV goes straight to the backend.
  Wiring `corti-aec` means sample-level decode/cancel/re-encode — a later pass once real NLMS lands (design 04).
- **Transcript sidecar** (`<recording>.transcript.json`): persisted the instant a transcript is ready so a
  crash at `PendingNote` re-files **without** re-transcribing (the AWS-staged output may already be deleted).
  This is corti's own durability detail beyond the queue's columns.
- **System-audio capture permission** has no Apple preflight, so it isn't checked up front; a capture
  `Error` (no IO callbacks) surfaces in the tray and the Settings ▸ "Open Privacy Settings…" item routes to
  the Screen & System Audio Recording pane.

Run / bundle:
- Dev tray (no capture): `cargo tauri dev` (from `app/`). Detection/attribution work; capture needs a bundle.
- Backend config: `export CORTI_AWS_BUCKET=<bucket>` (+ AWS creds/region via the standard chain).
- Bundle: `cd app && cargo tauri build --bundles app` → `target/release/bundle/macos/Corti.app`. For repeatable
  TCC grants sign with a **stable** identity (`APPLE_SIGNING_IDENTITY="<cert>"`), then `open Corti.app` via
  LaunchServices so the bundle's own TCC identity is used (LESSONS §1) — approve the prompts, join a call.
  - Verified the bundle assembles correctly: the merged `Info.plist` carries `NSAudioCaptureUsageDescription`
    + `NSMicrophoneUsageDescription` + `LSUIElement`, and `Entitlements.plist` (`device.audio-input` +
    `device.microphone`) produces a valid signature. (If `cargo tauri build` aborts at *"failed to run
    xattr"* — a sandboxed/PATH-less `/usr/bin/xattr` — the `.app` is already assembled; just
    `codesign --force --sign <id> --entitlements app/Entitlements.plist Corti.app` it by hand.)
