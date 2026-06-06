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
