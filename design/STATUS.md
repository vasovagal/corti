# corti — status & roadmap (resume here)

Snapshot of what's built, what's stubbed, and the order to tackle the rest. Pair with
[`LESSONS.md`](LESSONS.md) (hard-won audio-capture knowledge) and the per-component design docs.

## Done & verified ✅
- **corti-core** — domain types (`OwningApp`, `RecordingMeta`, `JobStatus`, `DiarizedTranscript` with
  Markdown rendering). Fully unit-tested.
- **corti-vagus** — shells out to `vagus add-note … --print-path`; proven end-to-end (a canned transcript
  became a real, searchable note in a temp vault).
- **corti-coreaudio** — owned CoreAudio bindings: mic-in-use listener (`MicMonitor`), attribution
  (`mic_owner` → `MicOwner{app, pid}`), and the **proven** capture engine
  (`CaptureSession` + `TapTarget::{Global, Process(pid)}`). Spike validated a synchronized 3-channel WAV.
- **corti-capture** — `Recorder` orchestration → 2-track WAV (ch0 = me, ch1 = them) under
  `~/Library/Caches/corti/recordings/`.

## Stubbed (compiling shells, ready to implement) 🚧
Each has a design doc and a `lib.rs` that compiles with the intended public API + `todo!`/`bail!` bodies.
- **corti-detect** → [`01-corti-detect.md`](01-corti-detect.md) — mic on/off state machine → drives Recorder.
- **corti-transcribe** → [`02-corti-transcribe.md`](02-corti-transcribe.md) — `Transcriber` trait (the
  integration contract; the diarized-Markdown renderer already lives in `corti-core`).
- **corti-transcribe-aws** → [`02-corti-transcribe.md`](02-corti-transcribe.md) — AWS Transcribe batch.
- **corti-transcribe-whisper** → [`02-corti-transcribe.md`](02-corti-transcribe.md) — local whisper.
- **corti-queue** → [`03-corti-queue.md`](03-corti-queue.md) — durable job store + crash recovery.
- **corti-aec** → [`04-corti-aec.md`](04-corti-aec.md) — offline NLMS echo canceller (speaker-bleed removal).
- **app/** → [`05-app-tauri.md`](05-app-tauri.md) — Tauri 2 tray binary; wires the pipeline.

## Recommended build order when you resume
1. **corti-detect** — pure logic; gives the first real "join huddle → 2-track WAV appears" loop.
2. **corti-transcribe + corti-transcribe-aws** — turns WAV → diarized Markdown → note via corti-vagus.
   (Full pipeline working, headphones path, no AEC yet.)
3. **corti-queue** — make it crash-safe.
4. **app/** — the tray, once there's a pipeline to drive.
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
