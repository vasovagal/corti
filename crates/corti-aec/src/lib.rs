//! Offline acoustic echo cancellation for corti.
//!
//! Removes **speaker bleed** from the mic track when the user is on speakers: the mic hears the far-end
//! after a room delay + coloring, so this is NOT `mic − far_end` but an adaptive filter that *learns* the
//! echo path. Because corti transcribes after the call, this runs **offline** — it can use a long filter,
//! double-talk gating, and a **2-pass** sweep (pass 1 converges the filter, pass 2 re-filters from sample 0
//! so the call opening is cleaned too).
//!
//! Implemented as an **overlap-save frequency-domain block adaptive filter** (FDAF) via `rustfft`: a
//! single-partition, bin-wise normalized LMS update. Frequency-domain block processing is what keeps it fast
//! even with long filters (`filter_len` of several thousand taps). Full algorithm + invariants live in
//! `design/04-corti-aec.md`.
//!
//! Invariant: the raw mic + far-end tracks are always preserved upstream; this produces an *additional*
//! cleaned track. Headphones remain the cleanest path; AEC is the speaker-user safety net.

use rustfft::Fft;
use rustfft::num_complex::Complex;

type Cf = Complex<f32>;

/// NLMS configuration.
#[derive(Debug, Clone)]
pub struct AecConfig {
    /// Adaptive-filter length in taps (e.g. 4096 ≈ 85 ms of echo at 48 kHz). Also the block hop; the FFT
    /// size is `2 · filter_len`.
    pub filter_len: usize,
    /// Step size (≈ 0.1–0.5).
    pub mu: f32,
    /// Regularization to avoid divide-by-zero on quiet far-end (≈ 1e-6).
    pub eps: f32,
}

impl Default for AecConfig {
    fn default() -> Self {
        Self {
            filter_len: 4096,
            mu: 0.3,
            eps: 1e-6,
        }
    }
}

/// Far-end power-spectrum smoothing (γ) for the per-bin NLMS normalization.
const POWER_SMOOTHING: f32 = 0.9;
/// Double-talk gate: freeze adaptation when a block's mic energy exceeds this × the far-end energy (the
/// near-end is then dominating, and adapting would diverge the filter).
const DOUBLE_TALK_RATIO: f32 = 2.0;
/// Offline passes: 1 to converge `W`/`P`, 2 to re-filter from the start with the converged filter so the
/// call opening is cleaned rather than left under-cancelled.
const PASSES: usize = 2;

/// Remove far-end echo from `mic`, returning the cleaned near-end signal (same length as `mic`).
///
/// Uses `far_end` (the time-aligned tap of what the speakers played) as the echo reference. When `far_end`
/// is silent or empty the output equals `mic` (nothing to cancel). The raw inputs are never mutated.
///
/// `sample_rate` is currently unused — the filter works purely in samples/blocks/bins and is rate-agnostic.
/// It's kept in the signature for API stability and for a future cross-correlation delay pre-alignment pass
/// (design/04-corti-aec.md §9), which would need it.
pub fn cancel(mic: &[f32], far_end: &[f32], _sample_rate: u32, cfg: &AecConfig) -> Vec<f32> {
    let n = mic.len();
    if n == 0 {
        return Vec::new();
    }
    let b = cfg.filter_len.max(1);
    let m = 2 * b;
    let num_blocks = n.div_ceil(b);
    let padded = num_blocks * b;

    // Padded, time-aligned mic (`d`) and far-end (`x`). Tap shorter than mic ⇒ zero-filled (no echo there).
    let mut d = vec![0.0f32; padded];
    d[..n].copy_from_slice(mic);
    let mut x = vec![0.0f32; padded];
    let take = far_end.len().min(padded);
    x[..take].copy_from_slice(&far_end[..take]);

    let mut planner = rustfft::FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(m);
    let ifft = planner.plan_fft_inverse(m);

    let mut w = vec![Cf::new(0.0, 0.0); m]; // frequency-domain filter weights
    let mut p = vec![0.0f32; m]; // smoothed far-end power per bin
    let mut out = vec![0.0f32; padded];

    for _ in 0..PASSES {
        run_pass(
            &mut w,
            &mut p,
            fft.as_ref(),
            ifft.as_ref(),
            &d,
            &x,
            b,
            m,
            cfg.mu,
            cfg.eps,
            &mut out,
        );
    }

    out.truncate(n);
    out
}

/// One forward sweep over every block: estimate the echo, subtract it (writing the cleaned output), then —
/// unless double-talk is detected — run the constrained bin-wise NLMS weight update. `w`/`p` carry over
/// between passes; the overlap history is implicit in `x` (consecutive blocks), so each pass naturally
/// restarts from sample 0 with the converged filter.
#[allow(clippy::too_many_arguments)]
fn run_pass(
    w: &mut [Cf],
    p: &mut [f32],
    fft: &dyn Fft<f32>,
    ifft: &dyn Fft<f32>,
    d: &[f32],
    x: &[f32],
    b: usize,
    m: usize,
    mu: f32,
    eps: f32,
    out: &mut [f32],
) {
    let num_blocks = d.len() / b;
    let inv_m = 1.0 / m as f32;

    let mut xspec = vec![Cf::new(0.0, 0.0); m]; // reused: far frame → X
    let mut yspec = vec![Cf::new(0.0, 0.0); m]; // reused: X⊙W → y
    let mut espec = vec![Cf::new(0.0, 0.0); m]; // reused: error frame → E
    let mut grad = vec![Cf::new(0.0, 0.0); m]; // reused: gradient

    for k in 0..num_blocks {
        let base = k * b;
        let cur = &x[base..base + b];

        // xframe = [previous b far samples ; current b far samples] (overlap-save), then FFT in place → X.
        for s in xspec.iter_mut() {
            *s = Cf::new(0.0, 0.0);
        }
        if k > 0 {
            let prev = &x[base - b..base];
            for i in 0..b {
                xspec[i] = Cf::new(prev[i], 0.0);
            }
        }
        for i in 0..b {
            xspec[b + i] = Cf::new(cur[i], 0.0);
        }
        fft.process(&mut xspec);

        // Y = X ⊙ W ; y = IFFT(Y)/M. The valid linear-convolution samples are the last B (overlap-save
        // discards the first B as circular-wrap aliasing).
        for j in 0..m {
            yspec[j] = xspec[j] * w[j];
        }
        ifft.process(&mut yspec);

        // eb = d_k − yb → cleaned output block; build the error frame [0_B ; eb] for the update. Also
        // accumulate the block energies the double-talk gate compares.
        let dk = &d[base..base + b];
        let mut d_energy = 0.0f32;
        let mut x_energy = 0.0f32;
        for i in 0..b {
            let y_i = yspec[b + i].re * inv_m;
            let e_i = dk[i] - y_i;
            out[base + i] = e_i;
            espec[i] = Cf::new(0.0, 0.0); // first half zero
            espec[b + i] = Cf::new(e_i, 0.0); // error in the valid second half
            d_energy += dk[i] * dk[i];
            x_energy += cur[i] * cur[i];
        }

        // Double-talk: near-end dominates ⇒ freeze the filter for this block. This is a coarse whole-block
        // energy-ratio gate; a coherence-based detector and explicit delay pre-alignment are documented
        // future refinements (design/04-corti-aec.md §9) that would make adaptation more robust on real,
        // correlated speech.
        if d_energy > DOUBLE_TALK_RATIO * x_energy {
            continue;
        }

        // P = γ·P + (1−γ)·|X|² (running far-end power per bin).
        for j in 0..m {
            p[j] = POWER_SMOOTHING * p[j] + (1.0 - POWER_SMOOTHING) * xspec[j].norm_sqr();
        }
        // E = FFT(error frame).
        fft.process(&mut espec);
        // G = conj(X) ⊙ E / (P + eps).
        for j in 0..m {
            grad[j] = (xspec[j].conj() * espec[j]).unscale(p[j] + eps);
        }
        // Gradient constraint: keep only the first B time-domain taps (normalized), zero the wrap-around
        // tail, so the filter stays a genuine length-B impulse response.
        ifft.process(&mut grad);
        for g in grad[..b].iter_mut() {
            *g = g.scale(inv_m);
        }
        for g in grad[b..].iter_mut() {
            *g = Cf::new(0.0, 0.0);
        }
        fft.process(&mut grad);
        // W += μ·G.
        for j in 0..m {
            w[j] += grad[j].scale(mu);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic white-ish noise in `[-amp, amp]` (LCG; no `rand` dep, reproducible across runs).
    fn noise(n: usize, amp: f32, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let u = (s >> 32) as f32 / u32::MAX as f32; // [0, 1]
                (u * 2.0 - 1.0) * amp
            })
            .collect()
    }

    /// Causal FIR convolution: `y[n] = Σ_k h[k]·x[n-k]` — a synthetic room/echo path.
    fn convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        for n in 0..x.len() {
            let mut acc = 0.0f32;
            for (k, &hk) in h.iter().enumerate() {
                if n >= k {
                    acc += hk * x[n - k];
                }
            }
            y[n] = acc;
        }
        y
    }

    fn energy(s: &[f32]) -> f32 {
        s.iter().map(|v| v * v).sum()
    }

    /// Pearson correlation (near 1.0 ⇒ same shape up to scale/offset).
    fn correlation(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len() as f32;
        let ma = a.iter().sum::<f32>() / n;
        let mb = b.iter().sum::<f32>() / n;
        let (mut num, mut da, mut db) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..a.len() {
            let (xa, xb) = (a[i] - ma, b[i] - mb);
            num += xa * xb;
            da += xa * xa;
            db += xb * xb;
        }
        num / (da.sqrt() * db.sqrt()).max(1e-12)
    }

    #[test]
    fn cancels_synthetic_echo() {
        let sr = 48_000u32;
        let secs = 3usize;
        let n = sr as usize * secs;

        // Far-end: white-ish noise (fast adaptive-filter convergence).
        let far = noise(n, 0.5, 1);

        // Room impulse: ~1 ms delay then exponential decay. Gain kept modest so echo energy stays well
        // below the far-end energy (otherwise the energy-ratio double-talk gate would freeze adaptation in
        // the echo-only regions too).
        let mut h = vec![0.0f32; 256];
        for (k, slot) in h.iter_mut().enumerate().skip(50) {
            *slot = 0.15 * (-((k - 50) as f32) / 40.0).exp();
        }
        let echo = convolve(&far, &h);

        // Near-end: a 440 Hz tone, loud, active ONLY in [1s, 2s). The surrounding regions are echo-only
        // ("near-silent-near") so the filter can converge and ERLE is measurable there.
        let mut near = vec![0.0f32; n];
        for (i, slot) in near.iter_mut().enumerate() {
            if i >= sr as usize && i < 2 * sr as usize {
                *slot = 0.7 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sr as f32).sin();
            }
        }

        let mic: Vec<f32> = (0..n).map(|i| echo[i] + near[i]).collect();

        let started = std::time::Instant::now();
        let clean = cancel(&mic, &far, sr, &AecConfig::default());
        let elapsed = started.elapsed();

        // Length invariant.
        assert_eq!(clean.len(), mic.len(), "output length must match mic");

        // ERLE in the converged near-silent tail [2s, 3s): residual echo vs raw echo.
        let lo = 2 * sr as usize;
        let raw_e = energy(&mic[lo..]);
        let res_e = energy(&clean[lo..]).max(1e-12);
        let erle_db = 10.0 * (raw_e / res_e).log10();
        assert!(erle_db >= 12.0, "ERLE {erle_db:.1} dB < 12 dB target");

        // Near-end preserved through double-talk [1s, 2s): the cleaned output must track the pure tone
        // *better than the raw mic does*. Asserting only an absolute correlation would be non-load-bearing
        // here — the raw mic already correlates ~0.92 with the tone (the echo barely perturbs it), so a
        // no-op passthrough would pass. We instead require the cleaned output to both clear a high floor and
        // strictly beat the raw baseline, which a passthrough (or an over-subtracting filter) cannot.
        let dt = sr as usize..2 * sr as usize;
        let raw_corr = correlation(&mic[dt.clone()], &near[dt.clone()]);
        let clean_corr = correlation(&clean[dt.clone()], &near[dt]);
        assert!(
            clean_corr > 0.99,
            "near-end correlation {clean_corr:.4} ≤ 0.99 (raw baseline {raw_corr:.4})"
        );
        assert!(
            clean_corr > raw_corr + 0.03,
            "AEC did not improve near-end isolation: clean {clean_corr:.4} vs raw {raw_corr:.4}"
        );

        // Speed goal: a few seconds of audio must cancel in well under its real-time duration.
        assert!(
            elapsed.as_secs_f32() < secs as f32 / 2.0,
            "AEC took {:.3}s for {secs}s of audio (expected ≪ real-time)",
            elapsed.as_secs_f32()
        );
    }

    #[test]
    fn silent_far_end_is_passthrough() {
        // No far-end ⇒ no echo to model ⇒ the filter never moves ⇒ output is the mic, bit-for-bit.
        let mic = noise(10_000, 0.4, 7);
        let far = vec![0.0f32; mic.len()];
        let clean = cancel(&mic, &far, 48_000, &AecConfig::default());
        assert_eq!(clean, mic);
    }

    #[test]
    fn dominant_near_end_is_preserved_exactly() {
        // Headphone case, the one that matters: the mic is pure near-end speech (no echo) and clearly
        // dominates a present-but-uncorrelated far-end tap. The double-talk gate must then freeze the
        // filter on every block, so AEC — which runs unconditionally — leaves the clean recording
        // untouched bit-for-bit (it never spuriously cancels the user's voice).
        //
        // Note: this guards the dominant-near-end case (active speaker). When a bleed-free near-end is only
        // *comparable* in energy to an uncorrelated reference, the long adaptive filter can overfit and
        // mildly distort it (~3% on a pessimal always-on synthetic); driving `aec_enabled` from a
        // headphones-vs-speakers hint instead of always-on is the scoped follow-up (design/04-corti-aec.md §9).
        let sr = 48_000u32;
        let n = sr as usize * 2;
        let mic: Vec<f32> = (0..n)
            .map(|i| 0.8 * (2.0 * std::f32::consts::PI * 330.0 * i as f32 / sr as f32).sin())
            .collect();
        let far = noise(n, 0.3, 4242); // present, uncorrelated, and quieter than the near-end

        let clean = cancel(&mic, &far, sr, &AecConfig::default());
        assert_eq!(
            clean, mic,
            "dominant near-end must be preserved bit-for-bit"
        );
    }

    #[test]
    fn length_matches_mic_for_non_block_multiples() {
        // Lengths that are not a multiple of filter_len must still round-trip exactly in size.
        let cfg = AecConfig {
            filter_len: 64,
            ..AecConfig::default()
        };
        for &n in &[0usize, 1, 63, 64, 65, 200] {
            let mic = noise(n, 0.3, n as u64 + 1);
            let far = noise(n, 0.3, n as u64 + 99);
            assert_eq!(cancel(&mic, &far, 48_000, &cfg).len(), n);
        }
    }
}
