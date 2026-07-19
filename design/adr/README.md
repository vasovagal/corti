# ADRs

Append-only decision log (Context / Decision / Consequences). Guardrails in
[`../guardrails.md`](../guardrails.md) each cite the ADR that binds them.

| ADR | Status | Decision |
|---|---|---|
| [0001](0001-separate-repo-and-vagus-boundary.md) | Accepted | corti is a separate project; vagus touched only through its CLI |
| [0002](0002-platform-and-capture.md) | Accepted | Apple Silicon + latest macOS only; all-CoreAudio process-tap capture; own the bindings |
| [0003](0003-local-asr-sherpa-onnx.md) | Accepted | local on-device ASR/diarization via sherpa-onnx (Parakeet-TDT) |
| [0004](0004-informational-webview.md) | Accepted | on-demand informational webview windows + a React/Vite frontend |
| [0005](0005-streaming-compressed-capture.md) | Partly superseded by 0007 | streaming compressed capture: bounded-RAM disk writer; keep AEC offline (compression + 2-pass-offline AEC superseded; ring/writer design shipped) |
| [0006](0006-distribution-unsigned-cask-tap.md) | Accepted | distribution: unsigned ad-hoc build via a private Homebrew cask tap |
| [0006](0006-far-end-recording-notice-spike.md) | Proposed (spike) | far-end "recording in progress" notice — feasibility spike |
| [0007](0007-streaming-aec-first.md) | Accepted (v0.8.0, #75) | streaming AEC-first pipeline; defer durability, calibration, compression — *durability partially reversed by #85 (durable jobs + retry + retention returned via `corti-jobs`)* |
| [0008](0008-live-streaming-transcript-in-process.md) | Proposed | live streaming transcript: first push-driven window, in-process transport |
| [0009](0009-chunked-transcription-api.md) | Accepted | chunked/live transcription: pull-based sync core, batch refactored onto it |
| [0010](0010-live-inbox-filing.md) | Accepted | live inbox filing: corti may append to / state-flip / delete notes it created — *amends 0001's boundary and 0008's file stance* |
| [0011](0011-spike-transcribe-cpp-ggml-asr.md) | Proposed (spike) | transcribe.cpp (GGML/Metal) as a runtime-selectable ASR engine — benchmark vs sherpa CPU, same Parakeet model |

**Two ADRs share number 0006** — `distribution-unsigned-cask-tap` (Accepted) and
`far-end-recording-notice-spike` (Proposed spike) — a numbering collision left as-is; disambiguate by
filename, not by `ADR 0006`.
