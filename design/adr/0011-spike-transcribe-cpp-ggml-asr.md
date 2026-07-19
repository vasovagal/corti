# ADR 0011 — Spike: transcribe.cpp (GGML/Metal) as a runtime-selectable ASR engine

- **Status:** Proposed (spike; 2026-07-19)
- **References:** ADR 0003 (local ASR via sherpa-onnx — including the CoreML rejection this revisits from a
  different angle), ADR 0009 (pull-based sync core; the `LiveTranscriber` seam this plugs into),
  guardrail 3 (far-reaching dependency ⇒ ADR), guardrail 6 (pluggable `Transcriber`).

## Context

ADR 0003 chose sherpa-onnx (ONNX Runtime) running NVIDIA Parakeet-TDT-0.6B-v3 int8, **CPU-only**: the
measured CoreML execution provider was 2.7–11× *slower* than CPU on the int8 transducer, so Apple-Silicon
GPU acceleration was rejected — as an ONNX Runtime EP problem, not as a verdict on the hardware.

[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp) (MIT, v0.1.3, by the author of Handy)
is a GGML-based ASR inference library — the whisper.cpp/llama.cpp lineage — with a fundamentally
different GPU path: **Metal, enabled automatically on Apple Silicon**. It ships an official GGUF port of
the *exact model corti runs* (`handy-computer/parakeet-tdt-0.6b-v3-gguf`, Q8_0 at WER 1.94% vs the 1.95%
F32 reference on LibriSpeech test-clean), maintainer-supported Rust bindings on crates.io
(`transcribe-cpp` — the community `sherpa-rs` situation ADR 0003 lamented, solved), and a single-file
model artifact instead of sherpa's multi-file ONNX directories. The author's headline claim is
faster-than-realtime SOTA ASR at lower watts than ONNX-CPU — exactly the axis corti is pinned on.

Risks are real: the project is ~3 months old (0.1.x, "rough edges" per the author), it has **no
Silero-VAD equivalent surfaced and no pyannote diarization** (its only diarization is a different model
family, MOSS), and its `-sys` crate compiles the vendored C++/ggml tree with CMake at build time.

## Decision

Run a **benchmark spike**, not a migration. The wager is narrow: *is GGML/Metal faster and cheaper (watts,
wall-clock) than ONNX/CPU on the same Parakeet-TDT-0.6B-v3, at equal-or-better WER?* Everything else is
deliberately held constant so the comparison isolates the ASR runtime:

1. **Swap only the per-region decode.** A new `Asr` enum in `corti-transcribe-local` sits where the
   `Arc<OfflineRecognizer>` used to flow (batch + live paths, ADR 0009's `LiveTranscriber`): `Sherpa`
   (shipping path, byte-identical) or `Ggml` (`ggml.rs` — `transcribe-cpp` `Model`/`Session` behind a
   `Mutex`, word-level timestamps mapped onto `segment::Word`). Resampling, Silero VAD chunking, pyannote
   far-end diarization and word→segment shaping all stay on sherpa-onnx in both configurations.
2. **Runtime-selected, compile-time gated.** `LocalConfig::asr_engine = "sherpa" | "ggml"`
   (`CORTI_LOCAL_ASR_ENGINE`, default `sherpa`), GGUF path override via `CORTI_LOCAL_GGML_MODEL`
   (default `<model dir>/parakeet-tdt-0.6b-v3-Q8_0.gguf`). The engine is compiled in only by the new
   `ggml` feature (`local-ggml` on the app, `ggml` on `corti-bench`) — **off by default**, so the default
   build neither changes behavior nor compiles the C++ tree. An unknown engine token errors; a `ggml`
   request against a non-`ggml` build errors (no silent fallback — that would mislabel bench results).
3. **Bench it with the existing harness.** `corti-bench process --asr-engine ggml [--ggml-model …]`
   emits the same JSON envelope (engine + GGUF recorded in `config`), so the Planet Money fixtures and
   scorers (design/06) compare WER/speed/RSS against the sherpa CPU baseline unchanged. Wall-power via
   `powermetrics` alongside, as in ADR 0003's methodology.

### Go / no-go criteria (evaluate on the M1 Pro daily driver)

- **Go** looks like: ≥1.5× faster wall-clock on the batch fixture at ≤ +0.5 pp WER, or comparable speed
  at clearly lower energy; live-path decode latency no worse than CPU sherpa.
- **No-go** looks like: Metal parity-or-worse with CPU (the CoreML story again), WER regressions from the
  GGUF port on real 2-track call audio (LibriSpeech parity does not guarantee far-end/AEC'd audio parity),
  or instability (crashes, truncated decodes) attributable to the 0.1.x runtime.

## Consequences

- The default build is byte-for-byte unchanged; the spike code is dark until the feature + env var are
  both set. CI's `--all-features` lane compiles the C++ tree (CMake on the macOS runner) and runs the
  pure mapping tests, so the spike can't rot silently.
- Two runtimes ship in a `ggml` build (ONNX Runtime for VAD/diarization + GGML for ASR). Acceptable for
  a spike; a real migration would need transcribe.cpp answers for VAD + diarization (or a corti-side
  Silero port) before sherpa could be dropped — tracked as an open question, not assumed.
- The GGUF is fetched manually for now (the resolve error prints the one-line `curl`);
  `fetch-models.sh` / the in-app downloader (#24) deliberately don't learn about it unless the spike goes.
- If no-go: delete `ggml.rs`, the `Asr::Ggml` arm and the features; the `Asr` seam itself is a
  zero-cost refactor worth keeping either way.
