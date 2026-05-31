# corti — status & roadmap (resume here)

Snapshot of what's built, what's stubbed, and the order to tackle the rest. Pair with
[`LESSONS.md`](LESSONS.md) (hard-won audio-capture knowledge) and the per-component design docs.

## Done & verified ✅
- **corti-core** — domain types (`OwningApp`, `RecordingMeta`, `JobStatus`, `DiarizedTranscript` with
  Markdown rendering). Fully unit-tested.
- **corti-vagus** — shells out to `vagus add-note … --print-path`; proven end-to-end (a canned transcript
  became a real, searchable note in a temp vault).
- **corti-coreaudio** — owned CoreAudio bindings: mic-in-use listener (`MicMonitor`),
  default-input-device-change listener (`DefaultInputDeviceMonitor`, added for corti-detect's mid-session
  rebind), attribution (`mic_owner` → `MicOwner{app, pid}`), and the **proven** capture engine
  (`CaptureSession` + `TapTarget::{Global, Process(pid)}`). Spike validated a synchronized 3-channel WAV.
- **corti-capture** — `Recorder` orchestration → 2-track WAV (ch0 = me, ch1 = them) under
  `~/Library/Caches/corti/recordings/`.
- **corti-detect** — debounce/coalesce state machine (`Detector` + `DetectorEvent`) turning mic on/off
  transitions into recordings via the `Recorder`. Pure timing logic in `machine.rs` is unit-tested; the
  macOS worker hops off the HAL thread and rebinds on a default-device change. Live-checkable via
  `cargo run -p corti-detect --example watch`.
- **corti-transcribe** — the `Transcriber` trait (sync `transcribe(audio, meta) -> DiarizedTranscript`);
  the diarized-Markdown renderer already lives in `corti-core`.
- **corti-transcribe-aws** — AWS Transcribe batch backend (the default). Uses **channel identification**
  (ch0→`Speaker::Me`, ch1→`Speaker::Other("Them")` — deterministic, no heuristics): re-encode the float
  WAV → 16-bit PCM, S3 upload, `StartTranscriptionJob`, poll, fetch + parse JSON. The caller injects an
  `SdkConfig` (`AwsTranscriber::new(&sdk_config, AwsOptions)`); the crate never reads creds. Pure parser +
  WAV re-encode are unit-tested; live check: `cargo run -p corti-transcribe-aws --example transcribe_file`.
- **corti-queue** — durable SQLite (WAL, bundled) job store at `~/.local/share/corti/queue.db` (outside any
  vault). `Queue` + `Job` + `JobUpdate`: `enqueue` (idempotent — job id = recording filename stem), `get`,
  `set_status`, `update`/`fail`, `resumable` (non-terminal rows to resume on startup), `all`, and
  `prune_done(cutoff)` (returns pruned WAV paths). `Job::meta()` rebuilds a `RecordingMeta` for resume.
  Unit-tested incl. reopen-persistence; inspect via `cargo run -p corti-queue --example inspect`.
- **app/** (`corti-app`, bin `corti`) — the Tauri 2 menu-bar tray that wires the whole pipeline together:
  `ActivationPolicy::Accessory` + `LSUIElement` (no Dock icon), a blinking template tray icon while
  recording, and a dynamically-rebuilt menu (status line, recent notes → open, settings, quit). Threading
  per design 05: the detector callback flips the blink/status and forwards each finished recording over a
  channel to a single **pipeline worker** that owns the `Queue` (enqueue → store stable `transcribe_job` →
  transcribe on a blocking thread → persist a transcript sidecar → file via `corti-vagus` → `Done`), with
  startup `resumable()` crash recovery + `prune_done` retention. Backends are feature-flavored
  (`default = ["aws"]`, `whisper` opt-in). `cargo build`/`clippy -D warnings`/`fmt`/`test` are clean (incl.
  the whisper-only build); the live join-call → note loop still needs a **signed bundle** to exercise the
  TCC audio-capture grant (LESSONS §1) — `cargo tauri build` produces the `.app` (merged `Info.plist` +
  `Entitlements.plist`).

## Stubbed (compiling shells, ready to implement) 🚧
Each has a design doc and a `lib.rs` that compiles with the intended public API + `todo!`/`bail!` bodies.
- **corti-transcribe-whisper** → [`02-corti-transcribe.md`](02-corti-transcribe.md) — local whisper.
- **corti-aec** → [`04-corti-aec.md`](04-corti-aec.md) — offline NLMS echo canceller (speaker-bleed removal).

## Recommended build order when you resume
1. ~~**corti-detect**~~ — **done** (pure logic; first real "join huddle → 2-track WAV appears" loop).
2. ~~**corti-transcribe + corti-transcribe-aws**~~ — **done** (WAV → diarized Markdown; channel-id Me/Them,
   caller-injected `SdkConfig`). Pairs with corti-vagus to file the note. Headphones path, no AEC yet.
3. ~~**corti-queue**~~ — **done** (durable SQLite WAL store; `resumable()` drives crash recovery on
   startup). Idempotent by recording-stem id. The aws backend now takes a stable `AwsOptions.job_name`, and
   the app stores it as `transcribe_job` + reuses it on resume so a re-poll re-attaches to the existing job
   instead of re-submitting (the parse/poll path was already idempotent).
4. ~~**app/**~~ — **done** (the Tauri tray; `enqueue` → transcribe → corti-vagus → `Done`, resuming
   `resumable()` rows on startup). Live capture loop pending a signed bundle (TCC; LESSONS §1).
5. **corti-aec** — quality polish for speaker users.
6. **corti-transcribe-whisper** — offline transcription flavor.

## Publishing (cargo publish)
All library crates carry a `description` + workspace `license`/`repository`, and internal deps are
`{ path, version }`, so they're publish-ready. `cargo publish --dry-run -p corti-core --allow-dirty` passes.
Publish in **dependency order** (each crate's deps must be live on crates.io first):
1. `corti-core`
2. `corti-transcribe` (needs core)
3. `corti-vagus`, `corti-coreaudio`, `corti-aec` (need core)
4. `corti-capture` (needs core, coreaudio)
5. `corti-detect` (needs core, coreaudio, capture)
6. `corti-transcribe-aws`, `corti-transcribe-whisper` (need core, transcribe), `corti-queue` (needs core)

`--dry-run` on a dependent crate fails until its deps are actually published (cargo resolves the version
dep from the registry) — that's expected, not a metadata problem. The `app/` binary will be `publish = false`.

## Guardrails & ADRs
See [`guardrails.md`](guardrails.md) and [`adr/`](adr/). Key invariants: vagus touched only via its CLI;
Apple-Silicon + latest-macOS only; own the platform bindings; capture is CoreAudio process-tap; audio caches
live outside any vault; transcription backends are feature-flavored; pipeline is crash-recoverable; audio
binaries need a TCC identity (guardrail 10).
