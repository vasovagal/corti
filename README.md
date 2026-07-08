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
- **Echo cancellation on lossless audio.** An FDAF adaptive filter strips speaker bleed from
  the mic track when you're on speakers, plus a residual echo suppressor that eliminates the
  faint ghosts the adaptive filter can't model — so the transcriber doesn't misattribute bleed
  as "Me." The mic and far-end are captured as separate lossless tracks (the far-end as a mono
  echo reference) so the filter sees bit-exact audio.
- **Diarized, timestamped transcripts.** Clean Markdown with word-level timestamps and
  **Me** / **Them** speaker labels, dropped right into your vagus vault.
- **Two transcription backends, picked at runtime.** **AWS Transcribe** for cloud accuracy,
  or a **fully offline on-device** backend (NVIDIA Parakeet-TDT-0.6B-v3 via ONNX) when nothing
  may leave the machine. Both compile in; choose with `CORTI_TRANSCRIBE_BACKEND`.
- **Standalone `corti-tap` CLI.** Force-tap system audio to a WAV on demand
  (`corti-tap --inbox` to transcribe + file, `--no-mic` for listen-only webinars,
  `--label` to name it).
- **Live inbox filing.** With the local backend, the app transcribes the call **as it happens**
  and appends finalized segments to the vagus note mid-call — `tail -f` the note and watch the
  conversation arrive. The note's first body line is `State: transcribing` while streaming and
  flips in place to `State: transcribed ` (same byte width) when final — the contract inbox
  agents key off. Also available standalone as `corti-tap --live`. See
  [`docs/streaming.md`](./docs/streaming.md) and
  [`docs/transcription.md`](./docs/transcription.md).

## How it works

```text
  mic-in-use (CoreAudio HAL DeviceIsRunningSomewhere)
       │
       ▼
  detector state machine        debounce 1.5s / coalesce 2s / discard < 3s
       │  Action::Start
       ▼
  in-process capture            one CoreAudio aggregate device, no subprocess
     process tap ─┐
     mic ─────────┼─▶ ring ─▶ 2-track WAV   (ch0 Me · ch1 Them)
       │      └─ bounded tee ─▶ live: streaming AEC ─▶ Parakeet ─▶ append segments
       │                              to the vagus note as the call happens (local backend)
       │  Action::Stop                └─ finish flips `State: transcribing` → `transcribed`
       ▼
  batch fallback (live off/ineligible/failed):
  AEC   FDAF adaptive filter + residual suppressor — strips speaker bleed off the mic track
       │
       ▼
  transcribe   Parakeet-TDT (offline ONNX)  |  AWS Transcribe
       │       → diarized, word-timestamped Markdown
       ▼
  vagus CLI    files the note (corti's other note writes are confined to notes it created — ADR 0010)
```

Internals — the same data flow with `file:line` anchors — live in
[`docs/architecture.md`](./docs/architecture.md).

## In the app

Past the tray menu, corti ships a **Settings** window (backend, S3 bucket, model download), an
**Ethics & Legality** guide (recording-consent norms by jurisdiction), and a **Diagnostics**
console with a live stats panel (CPU/RSS, per-stage timings, rolling logs). From the shell,
`corti --list` prints every tracked recording with its pipeline status and filed note.

## Install

```sh
brew tap vasovagal/tap
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

The echo canceller works out of the box for typical laptop-speaker setups. Individual
parameters can be set via environment variables (`CORTI_AEC_FILTER_LEN`, `CORTI_AEC_MU`,
`CORTI_AEC_POWER_SMOOTHING`, `CORTI_AEC_DOUBLE_TALK_RATIO`, `CORTI_AEC_SUPPRESS_RESIDUAL`) or
persisted in `config.toml`.

> The automated `corti --calibrate-aec` parameter sweep was dropped per
> [ADR 0007](./design/adr/0007-streaming-aec-first.md) (streaming AEC-first): it depended on
> persisted raw 2-track recordings, which the no-durability/no-raw-retention stance removes.
> See [`design/04a-aec-calibration.md`](./design/04a-aec-calibration.md) for the original
> tuning analysis.

## Status

Pre-1.0. The full pipeline runs end-to-end (detect → capture → AEC → transcribe → vagus) with
both backends working; the live join-call → note loop still needs a signed `.app` to exercise
the macOS audio-capture (TCC) grant.

## More

- [`docs/`](./docs/) — current-state internals: pipeline data flow, streaming, the crate map.
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) — build, test, workspace layout, dev setup.
- [`design/`](./design/) — ADRs, guardrails, and the rationale behind the stack.
