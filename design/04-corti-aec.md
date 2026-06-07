# 04 — corti-aec (offline acoustic echo cancellation)

Removes **speaker bleed** from the mic track: when the user is on speakers (not headphones), the mic
physically hears the far-end audio (see [`LESSONS.md`](LESSONS.md) §4). corti has both the mic track and the
*clean* far-end (tap) track, perfectly time-aligned, so it can cancel the echo.

## Critical: this is NOT `mic − far_end`
Naive subtraction fails. The mic hears the speakers after a **room delay**, colored by the speaker/room
**transfer function**, plus nonlinear distortion. The real solution is an **adaptive filter** that *learns*
the echo path `h` such that `echo ≈ h * far_end`, then outputs `clean = mic − (h * far_end)`.

## Why offline makes this tractable
corti transcribes *after* the call, so AEC runs **offline / non-real-time** — far more forgiving than
real-time AEC:
- No latency budget; can use a long filter (cover the full room impulse response).
- Can do a **2-pass / bidirectional** filter (run forward and backward, average) for better convergence.
- Can estimate and compensate a global delay first (cross-correlate mic vs far_end) so the adaptive filter
  starts aligned.

## Algorithm: NLMS (normalized least mean squares)
Per sample `n`, with filter length `L` (e.g. 2048–8192 taps at 48 kHz ≈ 40–170 ms of echo):
```
y[n]   = Σ_{k=0..L-1} w[k] · x[n-k]          # estimated echo from far-end x
e[n]   = d[n] − y[n]                          # d = mic; e = cleaned output
w[k]  += μ · e[n] · x[n-k] / (‖x‖² + ε)       # normalized update
```
Add **double-talk handling** (freeze adaptation when the near-end speaks, else the filter diverges) — a
simple energy-ratio detector (near vs far) gates the `w` update. Tune `μ` (~0.1–0.5) and `ε`.

## Invariants
- **Always preserve the raw mic + far-end tracks** — AEC produces an *additional* cleaned mic track and
  never overwrites the originals. If AEC underperforms, the pipeline falls back to the raw mic.
- **Tap-only recordings skip AEC cleanly** — a 1-channel ("webinar" / listen-only) capture has no mic
  track, so there is no speaker bleed to cancel. `write_clean_wav` returns `Ok(None)` and the pipeline
  transcribes the raw tap directly. This is an expected outcome, **not** an error/fallback.
- **Headphones remain the cleanest path** — AEC is the speaker-user safety net, documented as such, not a
  substitute for headphones.
- Hand-rolled (own the stack); NLMS is well-understood DSP with no dead-end dependency. Optional: pull in a
  small FFT crate (`rustfft`) if a frequency-domain block-NLMS (faster for long filters) is wanted later;
  start time-domain.

## API (target)
```rust
pub struct AecConfig { pub filter_len: usize, pub mu: f32, pub eps: f32 }
/// Returns the cleaned (echo-removed) near-end signal, same length as `mic`.
pub fn cancel(mic: &[f32], far_end: &[f32], sample_rate: u32, cfg: &AecConfig) -> Vec<f32>;
```

## Where it slots in the pipeline
After `corti-capture` writes the raw 2-track WAV, before transcription: produce `…-clean.wav` (or just feed
the cleaned mic + raw far-end to the transcriber). Transcribe the cleaned mic as "me", the raw tap as "them".

## Validation
Reuse the `splitwav` example + `afplay`: on a speaker recording, the cleaned mic channel should have the
music/far-end audibly suppressed while the user's voice remains. Keep a before/after for A/B.

## Depends on
Just `corti-core` (+ maybe `rustfft` later). Pure DSP, no platform deps — unit-testable with synthetic
signals (generate `far_end`, convolve with a known `h` to make a fake echo, add near-end speech, assert the
canceller recovers the near-end).
