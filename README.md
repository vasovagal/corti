# corti

**Auditory addon for the [vagus](https://github.com/vasovagal/vagus) second brain — Granola
without the SaaS.**

corti is a macOS menu-bar app that turns your meetings into searchable notes, automatically.
When another app grabs the microphone — Zoom, Slack huddles, Google Meet, Discord — corti
starts recording (a blinking tray icon); when the mic is released, it transcribes the call to
diarized, timestamped Markdown and files it straight into your vagus inbox. No "join meeting"
bot, no upload, no account.

Named after the **Organ of Corti** — the cochlear receptor that transduces sound into neural
signals. Exactly what this app does: audio → notes.

## Features

- **Zero-touch capture.** Detects calls automatically from the macOS mic-in-use signal — no
  buttons, no scheduling, no per-app integrations.
- **Both sides, cleanly separated.** Records your mic and the far-end audio as a synchronized
  2-track WAV through a single CoreAudio aggregate device — no virtual audio drivers, no
  ScreenCaptureKit.
- **Knows who you were talking to.** Attributes each recording to the app that owned the call.
- **Offline echo cancellation.** An NLMS/FDAF canceller strips speaker bleed from the mic
  track when you're on speakers; the raw tracks are always preserved.
- **Diarized, timestamped transcripts.** Clean Markdown with word-level timestamps and
  **Me** / **Them** speaker labels, dropped right into your vagus vault.
- **Crash-recoverable.** A durable job queue resumes interrupted transcriptions after a crash
  or restart — you never lose a recording.
- **Two transcription backends, picked at runtime.** **AWS Transcribe** for cloud accuracy,
  or a **fully offline on-device** backend (NVIDIA Parakeet-TDT-0.6B-v3 via ONNX) when nothing
  may leave the machine. Both compile in; choose with `CORTI_TRANSCRIBE_BACKEND`.
- **Standalone `corti-tap` CLI.** Force-tap system audio to a WAV on demand
  (`corti-tap --inbox` to transcribe + file, `--no-mic` for listen-only webinars,
  `--label` to name it).

**How it works:** detect → capture (mic + system tap) → echo-cancel → transcribe → file to
vagus (via the `vagus` CLI; corti never writes your vault directly).

## Install

```sh
brew tap vasovagal/corti
brew install --cask corti
```

corti launches into the menu bar. On first run macOS prompts once for **Microphone** and
**System Audio Recording** — grant both and you're done. (Distributed via a private cask;
pre-1.0.)

**Requirements:** Apple Silicon, macOS 15+.

## Speed & privacy

The offline backend runs **~30–60× realtime** on Apple Silicon — a 1-hour call transcribes
in roughly **1–2 minutes**, CPU-only, with no GPU dependency. Models are a ~0.7 GB one-time
download cached under `~/Library/Caches/corti/models/`; after that the local path makes **zero
network calls** — audio and transcript never leave the device. Prefer cloud accuracy? Point
`CORTI_TRANSCRIBE_BACKEND=aws` at AWS Transcribe instead. Either way, capture and
transcription run in the background; the UI never blocks.

## Status

The full pipeline is built and tested end-to-end (detect → capture → AEC → transcribe →
vagus, crash-recoverable), with both transcription backends working. The live join-call → note
loop needs a signed `.app` to exercise the macOS audio-capture (TCC) grant. See
[`design/STATUS.md`](./design/STATUS.md).

## More

- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — build, test, workspace layout, dev setup.
- [`design/`](./design/) — ADRs, guardrails, and the rationale behind the stack.
