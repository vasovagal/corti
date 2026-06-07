# ADR 0005 — Streaming compressed capture: bounded-RAM disk writer; keep AEC offline

- **Status:** Proposed (2026-06-07)

## Context

Capture accumulates the **entire recording in RAM**. The IO proc pushes every sample into one
`Mutex<Vec<f32>>` (`corti-coreaudio/src/capture.rs` — `Cap.samples`, written per-callback, drained only at
`stop()`); nothing is written to disk until the call ends. For a mic(1ch)+stereo-tap(2ch) 32-bit-float
stream at 48 kHz that is ~576 KB/s ≈ **2 GB/hour** of live memory. An 8-hour session (an all-day class or
training) tries to hold ~16 GB and OOMs — the real blocker for long calls is RAM, not disk. Separately, the
on-disk format is **32-bit IEEE float**, twice the bits speech recognition needs (the AWS path already
re-encodes to 16-bit PCM; the local Parakeet path resamples to 16 kHz mono), so half of every recording is
waste — the 3.6 GB recordings cache is mostly headroom no consumer reads.

Two more facts constrain the fix:

- **AEC is deliberately offline and 2-pass** (`corti-aec/src/lib.rs` — `PASSES = 2`): pass 2 re-filters from
  sample 0 with the converged adaptive filter so the *call opening* is cleaned. It reads the WAV **from
  disk**, never the in-memory buffer, and its binding invariant is that the raw mic + far-end tracks are
  always preserved (AEC produces an *additional* cleaned track). Bit-exact passthrough tests lock this in.
- **Guardrail 9** requires HAL callbacks to "not block and only hand work to the async runtime via a
  channel." Today's IO proc takes a `Mutex` and calls `Vec::push` (which can reallocate) — technically a
  violation; both can stall the real-time audio thread.

## Decision

- **Stream capture to disk incrementally.** The IO proc pushes interleaved frames into a **pre-allocated,
  bounded, wait-free SPSC ring** (no lock, no allocation on the audio thread — this *properly* satisfies
  guardrail 9). A **dedicated writer thread** drains the ring and encodes to disk. Peak RAM becomes
  `O(ring capacity)`, independent of call length.
- **Give `CaptureSession` its output path + layout up front**, and have `stop()` return a lightweight
  `RecordingHandle { path, frames, sample_rate, mic_channels, tap_channels, callbacks, dropped_samples }`
  instead of `CapturedAudio { samples }`. The output layout (2-track me/them, or tap-only mono) is fixed at
  `start`, since the file is opened and written *during* capture.
- **Store 16-bit PCM (Phase 0).** Lossless for ASR (quantization ~96 dB down), exactly half the size of
  32-bit float, and `hound` already streams it (`write_sample` + `finalize()` patches the header). Phased
  onward: **FLAC** (Phase 1, ~3–4× smaller than 32f *and* no RIFF size ceiling) and **opt-in Opus ~32 kbps**
  archival (Phase 2, ~24–31× smaller).
- **Keep AEC offline and post-call — do *not* move it in-flight.** A causal single-pass online filter loses
  the 2-pass opening cleanup, threatens the raw-preserved invariant, and forfeits re-tunability (re-running
  AEC on the saved raw). It buys nothing for RAM/disk, since AEC reads from disk. The on-disk **AEC/ASR
  input must stay lossless** (FLAC or 16-bit) — never feed it lossy Opus.

## Consequences

- **Reinforces guardrail 7 (crash-recoverable).** A finalized partial file now survives a crash mid-call;
  today a crash loses the entire in-RAM recording. The pipeline's `JobStatus::Recording` recovery
  (`app/src/pipeline.rs` — "no recorder survives a crash, so fail it") can be upgraded to salvage a partial.
- **Reinforces guardrail 9.** The IO proc becomes genuinely non-blocking and allocation-free.
- **API change** in `corti-coreaudio` (`CaptureSession::stop` → `Result<RecordingHandle>`; output layout at
  `start`) and `corti-capture` (`Recorder`). The spike (`corti-coreaudio/src/tap.rs`) and the capture unit
  tests adapt; `CapturedAudio` is retained as the me/them channel-split contract type used by the writer's
  per-frame downmix and its tests.
- **New dependency:** `rtrb` (a tiny pure-Rust wait-free SPSC ring — not a macOS binding, so no guardrail-3
  ADR needed beyond this note). Phases 1–2 add `flac-bound`/`opus` (portable C codec libs, not macOS
  bindings; `cmake` + `clang` are present on the build host).
- **Known limit (Phase 0):** a single 16-bit WAV hits the RIFF 32-bit (4 GiB) size ceiling at ≈ 6 h for
  2-channel audio. Removed by FLAC (Phase 1, no container cap) or segment roll-over; documented until then.
- **New failure surfaces:** ring overflow on a stalled disk must be **counted and surfaced, never silently
  dropped** (a dropped sample corrupts both the transcript and the AEC reference); partial files on
  discard must be deleted (`Recorder::discard`) and on crash recovered.
- **16-bit quantization changes stored sample values**, so the bit-exact AEC passthrough tests
  (`silent_far_end_is_passthrough`, `dominant_near_end_is_preserved_exactly`) must continue to receive
  lossless input (FLAC/16-bit is bit-exact integer PCM) or have their tolerances set deliberately — never
  let quantization silently break them.
- **User-facing guidance becomes honest:** with FLAC the default, "keep ~0.35 GB free per hour of recording
  (~3 GB for an all-day session)"; with Phase 0's 16-bit, ~0.7 GB/hr.
