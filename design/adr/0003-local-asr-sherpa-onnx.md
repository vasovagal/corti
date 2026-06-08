# ADR 0003 — local on-device ASR/diarization via sherpa-onnx (ONNX); Parakeet-TDT; M1 Pro build target

- **Status:** Accepted (2026-06-06)

## Context

corti's only complete transcription backend is AWS Transcribe (`corti-transcribe-aws`): audio leaves the
device (PHI egress), it costs per minute, and it needs the network. We want a **fully-offline** backend so
recordings never leave the Mac. Guardrail #3 requires an ADR before taking on a far-reaching third-party
dependency — a local ASR runtime is exactly that.

A mid-2026 research sweep (and direct verification) settled the engine choice:

- The most accurate, license-clean, *Rust-native* offline path is **NVIDIA Parakeet-TDT-0.6B-v3** (a
  Token-and-Duration transducer; CC-BY-4.0) run as **ONNX** through the official **`sherpa-onnx` Rust
  crate** (k2-fsa, v1.13.2, Apache-2.0). Parakeet beats Whisper-large-v3 on meeting audio and, as a
  transducer, is far less prone to the free-form hallucination that makes Whisper risky for notes.
- The community `sherpa-rs` crate (thewh1teagle) — what older guides recommend — was **archived
  2026-06-06**; its bindings are now folded into the official repo at `rust/sherpa-onnx` and shipped as the
  `sherpa-onnx` crate. We depend on the official crate.
- **CPU is enough.** Pure ONNX-Runtime CPU is ~30–60× realtime on Apple Silicon (1 hr ≈ 1–2 min). The
  faster Apple-native options (FluidAudio/Swift, parakeet-mlx/Python) are **dead-end deps** for a Rust app
  (guardrail #3) for speed we do not need. ONNX Runtime's accelerator on Apple is the **CoreML execution
  provider** (there is no separate "Metal" EP); it is reported *unstable for the Parakeet transducer*, so
  CPU is the default and CoreML is an opt-in knob only.
- corti already records ch0=mic / ch1=system-tap, so **channel = speaker** solves me-vs-them with no
  diarizer. The far-end channel (ch1) is optionally split into `Them 1/2/…` with sherpa-onnx's
  **pyannote-segmentation-3.0 + 3D-Speaker embedding** ONNX models (community-1's embedding does not export
  to ONNX, so we use segmentation-3.0).

## Decision

- **Adopt `sherpa-onnx` (ONNX Runtime) as corti's local ASR + VAD + diarization runtime**, in a new
  `corti-transcribe-local` crate behind the `Transcriber` trait. Models: Parakeet-TDT-0.6B-v3 (int8),
  Silero VAD, pyannote-segmentation-3.0, a 3D-Speaker embedding. Default execution provider **`cpu`**;
  `coreml` is an opt-in config knob (not validated/hardened here).
- **Feature-gated and runtime-selectable.** The crate is the `local` cargo feature, independent of `aws`;
  both compile together and the active backend is chosen **at runtime** (`CORTI_TRANSCRIBE_BACKEND`), so
  the default `aws` build is unaffected and neither dep is forced on the other (extends guardrail #6).
- **Static linking, prebuilt lib.** Use `sherpa-onnx = { features = ["static"] }`; the build downloads a
  prebuilt `osx-arm64` static lib on first build (network once). For hermetic/air-gapped CI, set
  `SHERPA_ONNX_LIB_DIR` to a pre-extracted archive.
- **Make Apple Silicon explicit in the build and tune for the author's M1 Pro.** A repo `.cargo/config.toml`
  pins `target = "aarch64-apple-darwin"` with `-C target-cpu=apple-m1`, and `[profile.release]` uses
  `lto = "thin"` + `codegen-units = 1`. `apple-m1` is the forward-compatible baseline (M2/M3/M4 are
  supersets), so binaries still run on any Apple Silicon while getting native codegen.

## Consequences

- A large vendored C++/onnxruntime dependency enters the tree, but only when the `local` feature is on;
  `cargo build` (default `aws`) is unchanged. First `local` build needs network for the prebuilt lib.
- **Licensing duty:** Parakeet-TDT and the pyannote segmentation weights are CC-BY-4.0 — ship attribution
  in `NOTICE`. sherpa-onnx is Apache-2.0; no non-commercial blockers.
- Models (~0.5–0.7 GB total int8) are downloaded on first run and cached under `~/Library/Caches/corti/models/`
  (guardrail #5 — outside any vault), well within the 10 GB budget.
- **Relaxation point:** the `target-cpu=apple-m1` pin in `.cargo/config.toml` is the single knob to relax if
  non-M-series ever matters; the Apple-Silicon-only *policy* remains ADR 0002.
- Transcription reads a WAV and never captures audio, so it needs no new TCC identity (guardrail #10).

## Addendum (2026-06-08): CoreML measured — not viable for the int8 transducer

The `coreml` provider knob looked broken: on an Apple-Silicon Mac it printed
`CoreML is for Apple only since onnxruntime>=1.15. Fallback to cpu!` once per ONNX session (3 ASR + 2 VAD =
5 lines) and ran on CPU — reading like platform misdetection. It is not. Confirmed by `nm`/`strings` on the
exact prebuilt the build links:

- The crates.io `sherpa-onnx-sys` **static** path links `csukuangfj/onnxruntime-libs`'
  `onnxruntime-osx-arm64-static_lib`, which is **CPU-only**. sherpa's own
  `cmake/onnxruntime-osx-arm64-static.cmake` hard-codes `add_definitions(-DSHERPA_ONNX_DISABLE_COREML)`
  "when using static onnxruntime lib", so `session.cc`'s CoreML branch
  (`#if defined(__APPLE__) && (ORT_API_VERSION>=15) && !defined(SHERPA_ONNX_DISABLE_COREML)`) is compiled
  out and the `coreml` request hits the `#else` fallback. **CoreML is impossible with the static lib.**
- CoreML is only present on the **shared** path: the upstream `…-osx-arm64-shared-lib` ships Microsoft's
  `libonnxruntime.1.24.4.dylib` (real CoreML EP — defines `OrtSessionOptionsAppendExecutionProvider_CoreML`,
  links `CoreML.framework`) and `libsherpa-onnx-c-api.dylib` compiled with the CoreML branch in.

We measured the payoff against that shared lib (fresh process per run = one real corti job; output **identical**
to CPU in every case):

| Audio  | CPU (median) | CoreML (median) | Result          |
|--------|--------------|-----------------|-----------------|
| 11.1 s | 1.73 s       | 19.22 s         | ~11× **slower** |
| 44.5 s | 5.53 s       | 25.23 s         | ~4.6× **slower**|

The CoreML penalty is a near-constant ~18–20 s independent of audio length — the per-session CoreML
**model-compilation** cost (encoder/decoder/joiner → `.mlmodelc`), paid every job because corti builds
sessions per job. sherpa 1.13.2 calls the EP with flags `0` and wires no model cache, so it never amortizes,
and the int8 transducer ops largely fall back to CPU anyway. **Net: slower, with no accuracy gain.**

**Decision:** keep CPU the default and do **not** ship CoreML. Instead of letting sherpa emit its misleading
fallback, the backend now maps a `coreml` request to `cpu` itself and logs one honest line
(`crates/corti-transcribe-local/src/lib.rs::resolve_provider`), unless the crate is built with the
`coreml-lib` feature. That feature is a **documented escape hatch** for re-evaluation, not a shipping path:
it needs a CoreML-capable sherpa-onnx lib, which the static model cannot provide. Re-enabling would require
either (a) **shared** linking + bundling `libonnxruntime.dylib`/`libsherpa-onnx-c-api.dylib` into the `.app`
(+ rpath from the app build script, + ad-hoc signing the dylibs), or (b) a **custom static** `libonnxruntime.a`
built from ONNX Runtime source with `--use_coreml` (then point sherpa at it via `SHERPA_ONNXRUNTIME_LIB_DIR`,
which skips the `DISABLE_COREML` define). Both are real work to make corti *slower* today; revisit only if a
future ORT/model or a persistent CoreML model cache changes the math.

To re-measure: build a CoreML-capable lib, then
`SHERPA_ONNX_LIB_DIR=<lib> cargo run -p corti-transcribe-local --features coreml-lib --example transcribe_file -- <recording.wav>`
with `CORTI_LOCAL_PROVIDER=coreml` vs `=cpu`.
