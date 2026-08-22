# ADR 0011 — transcribe.cpp (GGML/Metal) as a selectable local ASR engine

- **Status:** Accepted as an accelerated opt-in engine (2026-08-04); sherpa remains the compatibility default
- **References:** ADR 0003 (local ASR via sherpa-onnx and the CoreML rejection), ADR 0009 (pull-based live
  core), ADR 0012 (bounded crash-safe live checkpoints), issues #91/#103, PRs #92/#104.

## Context

ADR 0003 chose NVIDIA Parakeet-TDT-0.6B-v3 int8 through sherpa-onnx / ONNX Runtime on CPU. ONNX
Runtime's CoreML execution provider measured 2.7–11× slower than CPU for this transducer, so Corti did not
ship it. That rejected one runtime/accelerator pairing, not Apple-Silicon GPU inference in general.

[transcribe.cpp](https://github.com/handy-computer/transcribe.cpp) is an MIT-licensed GGML ASR runtime from
the Handy author. It provides a Metal backend and an official CC-BY-4.0 GGUF conversion of the exact same
Parakeet model. This is not a change from Parakeet to another model: it is a second implementation of the
per-speech-region Parakeet decode. Silero VAD and optional pyannote/embedding diarization still use
sherpa-onnx.

The original spike used crates.io v0.1.3. Before integration, upstream had moved to an unreleased 0.2.0 and
added model families, structured diarization APIs, and Rust API fields. Corti therefore pins the then-current
upstream `main` revision exactly:

- repository: `handy-computer/transcribe.cpp`
- revision: `553f1099a2b3a5bc4421894be171f09960fc0f3a` (2026-08-03)
- GGUF repository revision: `85ac09ea12fc4b1112fa76810059364bc6adc9de`
- Q8_0 GGUF SHA-256: `5859f77944efcd8eafa23a6350731960b2b55b2203df51f319665c807d802cc7`

The exact Git revision in `Cargo.toml`/`Cargo.lock` keeps the native source build reproducible while 0.2.0 is
unreleased. Move back to crates.io once an equivalent release exists.

## Decision

1. **Swap only per-region ASR.** `Asr::{Sherpa,Ggml}` sits behind the one `asr_segment` contract. Both
   engines receive the same 16 kHz Silero-VAD regions and return call-relative `Word`s; resampling,
   VAD, optional far-end diarization, word-to-segment shaping, channel labels, and timeline merge remain
   common.
2. **Use one loaded transcribe.cpp session.** The GGUF model/session is loaded once per job or live session,
   shared across channels behind a mutex, and receives Corti's configured inference thread count. Corti
   explicitly requires the Metal backend (no silent CPU fallback), and model metadata must identify
   `parakeet/tdt-0.6b-v3`; an accidental different GGUF is a hard error.
3. **Preserve crash-safe live semantics.** `LiveTranscriber::checkpoint()` resets only bounded resampler/VAD
   state, not the GGML model. Its cumulative 16 kHz epoch base is applied before either ASR engine decodes,
   so ADR 0012's rolling durable writes and call-relative timestamps are engine-independent. Optional
   diarization still runs on each bounded far-end window before append.
4. **Ship the engine, select it explicitly.** Standard app/release builds include `local-ggml`; minimal
   `--no-default-features --features local` builds remain sherpa-only. Settings and
   `CORTI_LOCAL_ASR_ENGINE={sherpa|ggml}` select the runtime. Unknown or unavailable values error rather
   than silently falling back and corrupting benchmark labels.
5. **Keep `sherpa` as the persisted/default value for upgrade safety.** Existing installations already have
   the ONNX artifact and must not fail after an update because a new 740 MB GGUF is absent. Users can
   download the verified GGUF in Settings → Models and opt into Metal. A future default flip requires a
   real detector-call soak with rolling live checkpoints.
6. **Make model management engine-aware.** Settings shows/downloads only the selected ASR representation
   (ONNX or GGUF), shared Silero VAD, and configured diarization artifacts. Live eligibility validates the
   selected representation; GGML no longer spuriously requires the ONNX Parakeet files. An explicit
   `CORTI_LOCAL_GGML_MODEL` remains available for benchmarking.
7. **Contain native diagnostics.** transcribe.cpp's default native sink emits Metal pipeline and per-region
   decoder lines. Corti disables that process-global sink once and emits its own bounded structured model
   load / region-failure events instead.

## M1 Pro benchmark result

Hardware: 10-core Apple M1 Pro, 32 GB, macOS 26.6. Input: the same 300 s Planet Money
`nx-s1-5844617` excerpt, shipping Silero/VAD settings, no diarization, Q8_0 GGUF. Each row is the mean of
three alternating post-warm release-process runs and includes model load.

| engine | mean ASR wall | speedup | mean peak RSS | normalized WER |
|---|---:|---:|---:|---:|
| sherpa ONNX / CPU | 24.605 s | 1.00× | 1,543 MB | 0.304791 |
| transcribe.cpp GGML / Metal | 6.016 s | **4.09×** | **1,246 MB (−19.25%)** | **0.304791** |

The committed excerpt reference is imperfectly aligned, so the absolute WER is not a product-quality claim;
the meaningful result is equal WER on the same input/reference. The two hypotheses differ by 14 of 887
normalized words (1.58%). The loaded backend reports `MTL0` / Apple M1 Pro.

First-ever Metal shader compilation took 10.3 s; the next process loaded the cached Metal library in 11 ms.
Even that one-time cold GGML run took 17.45 s versus sherpa's 26.07 s. Power was not measured because the
speed criterion alone passed. Raw post-warm runs live in `bench/results/transcribe_cpp_round1.jsonl`.

This clears the spike gate (≥1.5× wall-clock improvement at no more than +0.5 percentage-point WER) by a
wide margin and also lowers peak RSS.

## Consequences

- A standard build contains two inference runtimes. sherpa-onnx remains necessary for Silero VAD and
  pyannote-grade diarization, so transcribe.cpp is not yet a wholesale replacement for the local stack.
- The release build needs CMake/C++; the resulting default static link is self-contained in the app. The
  GGUF remains an external cache artifact and is never bundled into the app or a notes vault.
- The GGUF is about 740 MB versus the compressed ONNX download's 487 MB, but the measured running RSS is
  lower. Engine-aware Settings avoids requiring both artifacts.
- The latest upstream revision is intentionally a pinned Git dependency while 0.2.0 is unreleased. Every
  update requires rerunning compile, mapping, WER/speed/RSS, and real-call checks rather than following
  upstream `main` implicitly.
- `Them N` identity still comes from sherpa's optional diarizer and remains window-local under ADR 0012.
  transcribe.cpp's newer model-specific diarization families do not replace that path.
- Remaining default-flip gate: run a real detector call with GGML live filing across at least one durability
  checkpoint, confirm no tee drops, inspect timestamps/note completion, and soak for crashes/truncation.

## Addendum (2026-08-21): whole-corpus batch result — accuracy gate cleared, soak gate still open

Issue #118 re-ran both engines over the full Planet Money corpus (three complete episodes, 5,568.7 s) rather
than the single 300 s excerpt above. Spec `bench/configs/engine_round2.json`; rows `engine_round2.jsonl`;
tables in `bench/results/RESULTS.md` §0b.

| metric (3 full episodes) | sherpa ONNX / CPU | transcribe.cpp GGML / Metal |
|---|---:|---:|
| mean normalized WER | 0.1961 | **0.1840** (−1.22 pp) |
| total ASR wall | 461.3 s (12.1× realtime) | **116.4 s (47.8× realtime)** — 3.96× |
| mean peak RSS | 1,381 MB | **1,238 MB** (−10.3%) |

GGML is slightly *more* accurate on all three episodes, not merely at parity. Absolute WER is inflated on
both arms because the untrimmed NPR reference does not contain the mp3's inserted ads; the delta is the
result. Hypothesis-vs-hypothesis divergence is 3.63% of words (657/18,114).

This clears the **accuracy** half of a default flip on a corpus 18× larger than the original excerpt. It does
**not** clear decision 5's gate, and the two are not substitutes: the open gate is a real detector-call soak
with live filing across durability checkpoints (tee drops, timestamp continuity, note completion,
truncation/crash behaviour). Those are streaming-path properties; this is a batch file-tier benchmark and
exercises none of them. It also needs the macOS audio-capture (TCC) grant, so it cannot be run unattended.

A second, separable blocker on *deleting* the sherpa recognizer: decision 4 defines
`--no-default-features --features local` as a sherpa-only minimal build, and CI checks that lane. Removing
the sherpa recognizer leaves that configuration with no ASR engine at all, so consolidation has to redefine
what the minimal build is before it can compile — a build-topology decision, not a measurement.

Status therefore unchanged: sherpa stays the persisted default, both engines stay selectable.
