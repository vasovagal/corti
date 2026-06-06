# corti

**Auditory addon for the [vagus](https://github.com/vasovagal/vagus) second brain.**

`corti` auto-records your meeting audio (Zoom, Slack huddles, Google Meet, Discord), transcribes it to
diarized + timestamped text, and files the result as a note in your vagus inbox — "Granola without the
SaaS." It's a menu-bar tray app: when another app grabs the microphone, corti starts recording (a blinking
record icon); when the mic is released, recording ends and a transcript note is generated.

Named after the **Organ of Corti** — the cochlear receptor that transduces sound into neural signals.
Exactly what this app does: audio → notes.

## Design principles

- **Apple Silicon only** (`aarch64-apple-darwin`), **latest macOS only**. No Intel, no old-OS support.
- **Own the stack.** Where no comprehensive upstream exists (CoreAudio HAL listeners, process objects,
  process taps), corti ships its own thin safe bindings rather than risk a dead-end dependency.
- **vagus stays lean.** corti is a separate project; it touches vagus *only* by shelling out to the `vagus`
  CLI (`vagus add-note … --print-path`). It never writes the vault or the index directly.
- **Pluggable, runtime-selectable backends** — `aws` (AWS Transcribe) and `local` (offline, on-device
  Parakeet-TDT via ONNX) compile together; pick one at runtime with `CORTI_TRANSCRIBE_BACKEND`.

## Workspace layout

| Crate | Role |
|-------|------|
| `corti-core` | Shared domain types (no platform deps) |
| `corti-coreaudio` | Owned CoreAudio bindings: mic-in-use listener, process attribution, process taps |
| `corti-detect` | Start/stop trigger + attribution state machine |
| `corti-capture` | Aggregate-device capture graph → multitrack WAV |
| `corti-transcribe` | `Transcriber` trait + diarized-transcript Markdown renderer |
| `corti-transcribe-aws` | AWS Transcribe batch backend (`aws` feature) |
| `corti-transcribe-local` | Local offline backend — Parakeet-TDT via ONNX/sherpa-onnx (`local` feature) |
| `corti-queue` | Durable job store + crash recovery |
| `corti-vagus` | The `vagus` CLI shell-out (the only vagus touchpoint) |
| `app/` | Tauri 2 tray binary |

See [`design/`](./design/) for ADRs and guardrails.

## Status

The full pipeline is built: the CoreAudio capture spike validated (mic + per-process tap → synchronized
2-track WAV), every library crate is done + tested, and the `corti-app` Tauri menu-bar tray wires it all
together (detect → capture → transcribe → vagus, crash-recoverable). Both transcription backends are built:
AWS Transcribe and a fully-offline local backend (Parakeet-TDT-0.6B-v3 via ONNX/sherpa-onnx, with far-end
speaker diarization). The live join-call → note loop needs a signed `.app` to exercise the macOS
audio-capture (TCC) grant. Remaining: offline AEC (`corti-aec`). See [`design/STATUS.md`](./design/STATUS.md).
