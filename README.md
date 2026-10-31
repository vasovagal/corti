# corti

corti records calls on Apple Silicon Macs and files timestamped Markdown in your
[vagus](https://github.com/vasovagal/vagus) inbox. It detects another app's microphone use,
records your microphone and system audio, and transcribes locally or through AWS. No meeting bot
is needed. Start with [Install](#install), then choose the [privacy mode](#speed--privacy).

The name comes from the Organ of Corti, the cochlear structure that converts sound into neural signals.

## Features

- **Automatic call capture.** Microphone use starts recording; release ends it after the detector's
  debounce/coalescing interval. A blinking tray icon shows capture. App attribution is best-effort.
- **Separate audio tracks.** One CoreAudio aggregate device captures microphone and system audio into
  a two-track WAV, without a virtual driver. In-flight echo cancellation reduces speaker bleed into
  the mic; it does not guarantee that every echo is removed.
- **Local or AWS transcription.** Local NVIDIA Parakeet-TDT-0.6B-v3 runs through sherpa/ONNX on CPU
  or transcribe.cpp/GGML on Metal. Choose the backend and engine in Preferences, or with
  `CORTI_TRANSCRIBE_BACKEND` and `CORTI_LOCAL_ASR_ENGINE`.
- **Timestamped speaker labels.** Notes distinguish **Me** from **Them**. Optional local far-end
  diarization adds **Them N** labels; those identities are local to each filing window.
- **Live reading and durable filing.** Local calls publish speech regions to the Live Transcript
  window and sync bounded windows to a Vagus note. The nominal filing interval is one minute,
  configurable to 1–10, with bounded deferral to a speech boundary. Synced windows survive a crash;
  unfinished work can fall back to batch. See [streaming](docs/streaming.md) and
  [quality limits](docs/quality.md).
- **Microphone test.** The idle tray action opens a microphone-only ASR test. It creates no audio
  file, queue row, or note. Closing the window stops the microphone and resumes call detection.
- **Optional hosted text tools.** Separately enabled modes clean finalized text, answer questions,
  and refresh subscribed questions through direct APIs, Vertex, Bedrock, or a ChatGPT subscription.
  Raw ASR remains the fallback. Connecting a provider does not enable these modes; see [Speed & privacy](#speed--privacy).
- **Learned corrections.** `corti --review <note.md>` proposes lexicon rules for approval. Accepted
  rules apply to later transcript cleanup and hosted prompts; raw Live Transcript rows stay verbatim.
- **Generation and quality metadata.** Each note's `corti` frontmatter identifies the release,
  live/batch path, backend, selected model artifacts, and relevant settings without credentials,
  buckets, or private model paths. Current Vagus indexes this metadata, so
  `vagus search parakeet` can find Parakeet transcripts.
- **Standalone capture.** `corti-tap` records system audio to WAV. Use `--no-mic` for listen-only
  webinars, `--label` to name the recording, `--inbox` to transcribe and file, or `--live` for live
  terminal transcription. `--inbox` uses local ASR unless `CORTI_TRANSCRIBE_BACKEND=aws` selects AWS.

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
     mic ─────────┼─▶ ring ─▶ AEC ─▶ 2-track WAV   (ch0 Me, cleaned · ch1 Them)
       │             writer thread runs the FDAF adaptive filter + residual suppressor in flight,
       │             with raw fallback if filtering cannot start
       │      └─ bounded tee ─▶ live: streaming AEC ─▶ Parakeet (CPU or Metal) ─▶ rolling checkpoint
       │                              └─ optional diarize ─▶ append + sync_all (local backend)
       │  Action::Stop                   └─ sync tail, then flip `State:` durably
       ▼
  batch fallback (live off/ineligible/failed):
  transcribe   Parakeet-TDT (sherpa/CPU or transcribe.cpp/Metal)  |  AWS Transcribe
       │       → diarized, word-timestamped Markdown
       ▼
  vagus CLI    files the note (corti's other note writes are confined to notes it created — ADR 0010)
```

See [architecture](docs/architecture.md) for component ownership and source links, and
[the filing contract](docs/transcription.md#live-inbox-filing-87) for exact note state lines.

## In the app

The tray opens Live Transcript, Preferences, the recording-consent guide, Recording Queue,
and Diagnostics. Preferences controls transcription, hosted text modes, storage, and model downloads.
Diagnostics shows CPU/RSS, stage timings, and logs. Tray-opened windows come to the foreground.
`corti --list` lists tracked recordings, pipeline status, and filed notes.

## Install

**Requirements:** Apple Silicon and macOS 15+ for the app bundle. Install and configure
[Vagus](https://github.com/vasovagal/vagus#install) to file notes in its inbox.

```sh
brew tap vasovagal/tap
brew install --cask corti
```

Open Corti from Applications and grant **Microphone** and **System Audio Recording** access when
macOS asks. In Preferences, download the selected local models or configure AWS. The app is
ad-hoc signed and not notarized; macOS may require **Open Anyway** in Privacy & Security and renewed
recording grants after an update. See the [distribution contract](design/adr/0006-distribution-unsigned-cask-tap.md).

## Speed & privacy

With local ASR models downloaded and hosted text modes disabled, transcription stays on the Mac.
**AWS Transcribe uploads audio** to the configured S3 bucket. **Hosted text processing sends selected
transcript text, word-bank entries, steering, and questions** after disclosure acknowledgement; it
never sends audio. ChatGPT subscription mode uses a Corti-owned device credential and the fixed
Responses endpoint directly, without a Codex server or a separate API-key charge for that transport.

Models live under `~/Library/Caches/corti/models/`. The selected ASR download is about 487 MB for
compressed ONNX or 740 MB for GGUF, plus VAD and any selected diarization models. Hosted credentials
stay in private files under `~/.local/share/corti/hosted-secrets/`, outside the vault and webview.

On a five-minute M1 Pro benchmark excerpt, Metal averaged **6.016 s** including model load:
**4.09× faster** and **19% lower peak RSS** than sherpa/CPU at the same normalized excerpt WER.
This is a measurement of that input, not a general accuracy or speed guarantee. Sherpa remains the
default; download the Q8_0 GGUF and select Metal to use it. See [the benchmark and its limits](design/adr/0011-spike-transcribe-cpp-ggml-asr.md#m1-pro-benchmark-result).

## AEC tuning

Tune echo cancellation through `CORTI_AEC_FILTER_LEN`, `CORTI_AEC_MU`,
`CORTI_AEC_POWER_SMOOTHING`, `CORTI_AEC_DOUBLE_TALK_RATIO`, and `CORTI_AEC_SUPPRESS_RESIDUAL`,
or persist the settings in `config.toml`. See [audio processing](docs/audio-pipeline.md).

There is no `corti --calibrate-aec` command. Its proposed sweep required retained raw mic tracks;
see [ADR 0007](design/adr/0007-streaming-aec-first.md) and the
[tuning analysis](design/04a-aec-calibration.md).

## Status

Pre-1.0. This README describes the current source tree; use the
[release notes](https://github.com/vasovagal/corti/releases) for the installed release.
Quality accounting and known live-boundary limits are documented in [docs/quality.md](docs/quality.md).

## More

- [Documentation](docs/) — pipeline, streaming, app surfaces, and quality contracts.
- [Contributing](CONTRIBUTING.md) — prerequisites, build, tests, and development setup.
- [Design](design/) — ADRs, guardrails, and rationale.
