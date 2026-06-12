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
- **Offline echo cancellation.** An FDAF adaptive filter strips speaker bleed from the mic
  track when you're on speakers, plus a residual echo suppressor that eliminates the faint
  ghosts the adaptive filter can't model — so the transcriber doesn't misattribute bleed as
  "Me." The raw tracks are always preserved. Tune with `corti --calibrate-aec`.
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

## AEC tuning

The echo canceller works out of the box for typical laptop-speaker setups. If your
room/speakers produce unusual echo, `corti --calibrate-aec` sweeps 144 parameter combinations
against your recent recordings and saves the best config:

```sh
corti --calibrate-aec                    # auto-picks 3 most recent 2-channel recordings
corti --calibrate-aec ~/path/to/*.wav    # explicit recordings
corti --calibrate-aec --dry-run          # print results without saving
```

Individual parameters can also be set via environment variables (`CORTI_AEC_FILTER_LEN`,
`CORTI_AEC_MU`, `CORTI_AEC_POWER_SMOOTHING`, `CORTI_AEC_DOUBLE_TALK_RATIO`,
`CORTI_AEC_SUPPRESS_RESIDUAL`) or persisted in `config.toml`. See
[`design/04a-aec-calibration.md`](./design/04a-aec-calibration.md) for the full analysis.

## Status

The full pipeline is built and tested end-to-end (detect → capture → AEC → transcribe →
vagus, crash-recoverable), with both transcription backends working. The live join-call → note
loop needs a signed `.app` to exercise the macOS audio-capture (TCC) grant. See
[`design/STATUS.md`](./design/STATUS.md).

## More

- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — build, test, workspace layout, dev setup.
- [`design/`](./design/) — ADRs, guardrails, and the rationale behind the stack.
