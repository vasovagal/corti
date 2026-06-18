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

pub mod score;
pub mod tune;

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
    /// Far-end power-spectrum smoothing (γ) for the per-bin NLMS normalization (≈ 0.8–0.95).
    pub power_smoothing: f32,
    /// Double-talk gate: freeze adaptation when a block's mic energy exceeds this × the far-end energy (the
    /// near-end is then dominating, and adapting would diverge the filter). Lower values freeze more
    /// aggressively (≈ 1.0–3.0).
    pub double_talk_ratio: f32,
    /// Offline passes: 1 to converge `W`/`P`, 2 to re-filter from the start with the converged filter so
    /// the call opening is cleaned rather than left under-cancelled.
    pub passes: usize,
    /// Residual echo suppression overestimation factor. After the FDAF removes the bulk echo, the post-filter
    /// suppresses per-bin residual using a spectral-subtraction gain: G = max(1 − α·|Y|²/|E|², floor). Set 0
    /// to disable. Typical range 1.5–4.0; higher → more aggressive (risks near-end distortion).
    pub suppress_residual: f32,
}

impl Default for AecConfig {
    fn default() -> Self {
        Self {
            filter_len: 4096,
            mu: 0.3,
            eps: 1e-6,
            power_smoothing: 0.9,
            double_talk_ratio: 2.0,
            passes: 2,
            suppress_residual: 2.5,
        }
    }
}

/// Length of each fixed analysis segment for the bounded delay estimator (samples). 32768 ≈ 0.68 s at
/// 48 kHz — comfortably longer than the ≤10 ms (≤480-sample) delay we search, so each segment's
/// `[0, max_lag)` correlation is dominated by genuine (non-circular-wrap) overlap.
const DELAY_SEG_LEN: usize = 1 << 15;
/// Max segments folded into the accumulated cross-spectrum. Caps work — and memory — at O(1) in `n`:
/// segments are spread evenly across the whole recording and reuse the same fixed buffers.
const DELAY_MAX_SEGMENTS: usize = 64;
/// A sampled segment's far-end energy must reach this fraction of the loudest sampled segment to be
/// folded in, so silent / near-end-only stretches don't dilute the cross-correlation average.
const DELAY_ENERGY_GATE: f32 = 0.05;

/// FFT length used by [`estimate_delay`] for a recording of `n` samples. Factored out so the
/// memory-bound test can assert O(1)-in-`n` arithmetically, without allocating. Bounded by the
/// constant `(DELAY_SEG_LEN + max_lag).next_power_of_two()` for any `n >= DELAY_SEG_LEN`.
fn delay_fft_len(n: usize, max_lag: usize) -> usize {
    (DELAY_SEG_LEN.min(n) + max_lag).next_power_of_two()
}

/// Estimate the bulk acoustic speaker→mic delay (in samples) via a segmented accumulated cross-spectrum.
///
/// Finds the lag in `[0, max_lag)` that maximizes the cross-correlation between `mic` and `far_end` —
/// the room's acoustic propagation delay (typically 1–10 ms, 48–480 samples at 48 kHz). Pre-shifting
/// `far_end` by this amount lets the adaptive filter start aligned and converge faster.
///
/// The delay is a single room constant for the whole call, so rather than transforming the entire
/// recording (which would size the FFT as `O(n)` and allocate gigabytes for a long call) this samples up
/// to `DELAY_MAX_SEGMENTS` fixed-size `DELAY_SEG_LEN` windows spread across the call, FFTs each, and
/// **sums** their cross-spectra `conj(Far_s) ⊙ Mic_s` before one IFFT. By linearity this equals summing
/// the per-segment linear cross-correlations: the true lag reinforces coherently while uncorrelated
/// near-end energy averages down (Welch/GCC-style). Every buffer is sized by
/// `(DELAY_SEG_LEN + max_lag).next_power_of_two()` — a constant independent of `n` — so peak transient
/// memory and the largest single allocation are bounded regardless of recording length.
///
/// Returns 0 when either signal is empty, when `max_lag` is 0, when the far-end is silent over every
/// sampled segment, or when the best in-window lag is negative (far_end leads mic — shouldn't happen in a
/// speaker→mic path but can occur with misaligned captures; only lags `[0, max_lag)` are searched).
///
/// Total on degenerate input: mismatched `mic`/`far_end` lengths (the shorter bounds the analysis),
/// `max_lag` larger than the signal, and non-finite samples all return a bounded lag in `[0, max_lag)`
/// without panicking (the lag is unspecified — but still bounded — when samples are non-finite).
fn estimate_delay(mic: &[f32], far_end: &[f32], max_lag: usize) -> usize {
    let n = mic.len().min(far_end.len());
    if n == 0 || max_lag == 0 {
        return 0;
    }

    let seg_len = DELAY_SEG_LEN.min(n);
    // Zero-pad so lags in [0, max_lag) are a wrap-free LINEAR cross-correlation (matches the original
    // full-length transform's semantics, restricted to each segment).
    let fft_len = delay_fft_len(n, max_lag);

    let mut planner = rustfft::FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(fft_len);
    let ifft = planner.plan_fft_inverse(fft_len);

    // Five fixed-size buffers — the entire footprint, all O(1) in n.
    let mut mic_spec = vec![Cf::new(0.0, 0.0); fft_len];
    let mut far_spec = vec![Cf::new(0.0, 0.0); fft_len];
    let mut xspec = vec![Cf::new(0.0, 0.0); fft_len]; // accumulated cross-spectrum
    let mut xcorr = vec![Cf::new(0.0, 0.0); fft_len]; // IFFT buffer
    let scratch_len = fft
        .get_inplace_scratch_len()
        .max(ifft.get_inplace_scratch_len());
    let mut scratch = vec![Cf::new(0.0, 0.0); scratch_len];

    // Spread up to DELAY_MAX_SEGMENTS evenly across the non-overlapping seg_len windows of the recording.
    let total_segments = (n / seg_len).max(1);
    let used = total_segments.min(DELAY_MAX_SEGMENTS);
    let stride = total_segments / used; // >= 1 since used <= total_segments

    // Pass 1: per-segment far-end energy + the loudest, to set the gate (skips silence / near-end-only).
    let mut seg_energy = [0.0f32; DELAY_MAX_SEGMENTS];
    let mut max_far_energy = 0.0f32;
    for (s, slot) in seg_energy[..used].iter_mut().enumerate() {
        let base = (s * stride) * seg_len; // base + seg_len <= n: s*stride < total_segments, total*seg_len <= n
        let e: f32 = far_end[base..base + seg_len].iter().map(|&v| v * v).sum();
        *slot = e;
        if e > max_far_energy {
            max_far_energy = e;
        }
    }
    if max_far_energy <= 0.0 {
        return 0; // far-end silent over every sampled segment → nothing to align
    }
    let gate = DELAY_ENERGY_GATE * max_far_energy;

    // Pass 2: fold each loud-enough segment's cross-spectrum into the accumulator.
    let mut folded = 0usize;
    for (s, &e) in seg_energy[..used].iter().enumerate() {
        if e < gate {
            continue;
        }
        let base = (s * stride) * seg_len;
        for v in mic_spec.iter_mut() {
            *v = Cf::new(0.0, 0.0);
        }
        for v in far_spec.iter_mut() {
            *v = Cf::new(0.0, 0.0);
        }
        for i in 0..seg_len {
            mic_spec[i] = Cf::new(mic[base + i], 0.0);
            far_spec[i] = Cf::new(far_end[base + i], 0.0);
        }
        fft.process_with_scratch(&mut mic_spec, &mut scratch);
        fft.process_with_scratch(&mut far_spec, &mut scratch);
        for j in 0..fft_len {
            xspec[j] += far_spec[j].conj() * mic_spec[j];
        }
        folded += 1;
    }
    if folded == 0 {
        return 0;
    }

    // Single IFFT of the averaged cross-power spectrum → linear cross-correlation for lags [0, max_lag).
    xcorr.copy_from_slice(&xspec);
    ifft.process_with_scratch(&mut xcorr, &mut scratch);

    // Search positive lags [0, max_lag) for the peak. fft_len > max_lag by construction, so in-bounds.
    let search = max_lag.min(fft_len);
    let (best_lag, _) = xcorr[..search]
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.re.total_cmp(&b.re))
        .unwrap_or((0, &Cf::new(0.0, 0.0)));
    best_lag
}

/// Remove far-end echo from `mic`, returning the cleaned near-end signal (same length as `mic`).
///
/// Uses `far_end` (the time-aligned tap of what the speakers played) as the echo reference. When `far_end`
/// is silent or empty the output equals `mic` (nothing to cancel). The raw inputs are never mutated.
///
/// Pre-aligns `far_end` by estimating the bulk acoustic delay via cross-correlation (design/04-corti-aec.md
/// §9) before running the adaptive filter. `sample_rate` gates the max-lag search window (10 ms).
///
/// Memory: [`estimate_delay`] is bounded by a constant independent of `n` (segmented accumulated
/// cross-spectrum). The remaining buffers here — `d`, `x`, `out` — are intentionally `O(n)` length-`padded`
/// `f32` (≈ `n` each, 4 bytes/sample): a whole-file offline FDAF that returns a full-length cleaned track
/// must hold the inputs resident. These are reclaimed normally and were never the crash trigger (that was
/// the GB-scale `Complex<f32>` allocation in `estimate_delay`, now bounded). A fully streaming /
/// block-windowed FDAF that bounds `cancel()` itself in `n` is the documented follow-up if multi-hour
/// offline calls ever pressure RAM (tracked GH "Bug Fix" issue) — it changes the 2-pass "restart from
/// sample 0 with the converged filter" structure and is out of scope for this delay-estimator fix.
pub fn cancel(mic: &[f32], far_end: &[f32], sample_rate: u32, cfg: &AecConfig) -> Vec<f32> {
    let n = mic.len();
    if n == 0 {
        return Vec::new();
    }

    // Pre-align: shift far_end forward by the estimated acoustic delay so the adaptive filter starts
    // converged. Max search window = 10 ms (a generous room propagation bound).
    let max_lag = (sample_rate as usize * 10) / 1000; // 10 ms
    let delay = estimate_delay(mic, far_end, max_lag);

    let b = cfg.filter_len.max(1);
    let m = 2 * b;
    let num_blocks = n.div_ceil(b);
    let padded = num_blocks * b;

    let mut d = vec![0.0f32; padded];
    d[..n].copy_from_slice(mic);
    // Pre-align far_end by `delay` samples directly into the zero-padded reference buffer `x` (the first
    // `delay` entries stay zero). Equivalent to prepending `delay` zeros to far_end then copying, but
    // without materializing that intermediate Vec — `x`'s contents (hence the filter numerics) are
    // bit-identical to the previous code.
    let mut x = vec![0.0f32; padded];
    if delay < padded {
        let copy_len = far_end.len().min(padded - delay);
        x[delay..delay + copy_len].copy_from_slice(&far_end[..copy_len]);
    }

    let mut planner = rustfft::FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(m);
    let ifft = planner.plan_fft_inverse(m);

    let mut w = vec![Cf::new(0.0, 0.0); m]; // frequency-domain filter weights
    let mut p = vec![0.0f32; m]; // smoothed far-end power per bin
    let mut out = vec![0.0f32; padded];

    for _ in 0..cfg.passes {
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
            cfg.power_smoothing,
            cfg.double_talk_ratio,
            &mut out,
        );
    }

    if cfg.suppress_residual > 0.0 {
        suppression_pass(
            &w,
            fft.as_ref(),
            ifft.as_ref(),
            &d,
            &x,
            b,
            m,
            cfg.suppress_residual,
            cfg.double_talk_ratio,
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
    power_smoothing: f32,
    double_talk_ratio: f32,
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
        if d_energy > double_talk_ratio * x_energy {
            continue;
        }

        // P tracks far-end power per bin. Uses a one-sided smoother: power *increases* are tracked
        // immediately (preventing P ≪ |X|² which would inflate the gradient and diverge the filter),
        // while *decreases* are smoothed (conservative — P too large just shrinks the step).
        for j in 0..m {
            let inst = xspec[j].norm_sqr();
            if inst > p[j] {
                p[j] = inst.max(eps);
            } else {
                p[j] = power_smoothing * p[j] + (1.0 - power_smoothing) * inst;
                p[j] = p[j].max(eps);
            }
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
        // W = (1−λ)·W + μ·G. The small leakage λ prevents unbounded weight growth over long
        // recordings (leaky NLMS).
        let leak = 1e-5f32;
        for j in 0..m {
            w[j] = w[j].scale(1.0 - leak) + grad[j].scale(mu);
        }
    }
}

/// Residual echo suppressor: a forward-only pass with the frozen filter W that applies per-bin
/// spectral subtraction to remove echo the FDAF couldn't model (nonlinearities, late reverb).
///
/// For each block: Y = X⊙W is the echo estimate, E = D−Y is the FDAF error. The gain
/// G(k) = max(1 − α·|Y(k)|²/|E(k)|², floor) attenuates bins where the echo estimate is a
/// large fraction of the residual, leaving near-end-dominant bins untouched.
#[allow(clippy::too_many_arguments)]
fn suppression_pass(
    w: &[Cf],
    fft: &dyn Fft<f32>,
    ifft: &dyn Fft<f32>,
    d: &[f32],
    x: &[f32],
    b: usize,
    m: usize,
    alpha: f32,
    double_talk_ratio: f32,
    eps: f32,
    out: &mut [f32],
) {
    let num_blocks = d.len() / b;
    let inv_m = 1.0 / m as f32;
    let floor = 0.01f32; // −40 dB spectral floor

    let mut xspec = vec![Cf::new(0.0, 0.0); m];
    let mut yspec = vec![Cf::new(0.0, 0.0); m];
    let mut espec = vec![Cf::new(0.0, 0.0); m];

    for k in 0..num_blocks {
        let base = k * b;
        let cur = &x[base..base + b];

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

        // Y = X ⊙ W (echo estimate in freq domain — kept for post-filter gain)
        for j in 0..m {
            yspec[j] = xspec[j] * w[j];
        }

        // IFFT(Y) → time-domain echo estimate; compute error e = d − y
        let mut yspec_td = yspec.clone();
        ifft.process(&mut yspec_td);

        let dk = &d[base..base + b];
        let mut d_energy = 0.0f32;
        let mut x_energy = 0.0f32;
        for i in 0..b {
            let y_i = yspec_td[b + i].re * inv_m;
            let e_i = dk[i] - y_i;
            espec[i] = Cf::new(0.0, 0.0);
            espec[b + i] = Cf::new(e_i, 0.0);
            d_energy += dk[i] * dk[i];
            x_energy += cur[i] * cur[i];
        }

        // During double-talk the user is speaking — pass through unsuppressed.
        if d_energy > double_talk_ratio * x_energy {
            for i in 0..b {
                out[base + i] = espec[b + i].re;
            }
            continue;
        }

        // Spectral subtraction: suppress bins where echo estimate dominates the error.
        fft.process(&mut espec);
        for j in 0..m {
            let echo_pow = yspec[j].norm_sqr();
            let error_pow = espec[j].norm_sqr();
            let gain = (1.0 - alpha * echo_pow / (error_pow + eps)).max(floor);
            espec[j] = espec[j].scale(gain);
        }
        ifft.process(&mut espec);
        for i in 0..b {
            out[base + i] = espec[b + i].re * inv_m;
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

        // Speed goal: a few seconds of audio must cancel faster than real time. The half-real-time
        // target only holds for optimized builds; `cargo test` runs unoptimized, where a slow CI
        // runner can legitimately exceed it (observed ~1.9s for 3s). Debug builds just need to clear
        // real time, which still catches an algorithmic blowup; release builds hold the tighter bound.
        let budget = if cfg!(debug_assertions) {
            secs as f32
        } else {
            secs as f32 / 2.0
        };
        assert!(
            elapsed.as_secs_f32() < budget,
            "AEC took {:.3}s for {secs}s of audio (expected < {budget:.1}s)",
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

    /// Build a far-end signal of `n` samples with a delayed echo into `mic`, plus optional leading silence.
    /// Returns `(mic, far)` where mic = far shifted right by `delay` (pure echo, no near-end).
    fn delayed_echo(
        n: usize,
        delay: usize,
        lead_silence: usize,
        seed: u64,
    ) -> (Vec<f32>, Vec<f32>) {
        let far = {
            let mut f = vec![0.0f32; lead_silence];
            f.extend(noise(n.saturating_sub(lead_silence), 0.5, seed));
            f.truncate(n);
            f.resize(n, 0.0);
            f
        };
        let mut mic = vec![0.0f32; n];
        if delay < n {
            mic[delay..n].copy_from_slice(&far[..(n - delay)]);
        }
        (mic, far)
    }

    #[test]
    fn estimate_delay_recovers_known_lag() {
        // A clean delayed echo: the cross-correlation peak must land exactly on the planted delay.
        let max_lag = 480; // 10 ms at 48 kHz, as cancel() computes it
        for &delay in &[0usize, 1, 37, 200, 479] {
            let (mic, far) = delayed_echo(200_000, delay, 0, 11);
            assert_eq!(
                estimate_delay(&mic, &far, max_lag),
                delay,
                "planted delay {delay} not recovered"
            );
        }
    }

    #[test]
    fn estimate_delay_matches_full_fft_reference() {
        // The bounded-window estimate must agree with a brute-force full-length cross-correlation
        // (the semantics the original O(n)-memory implementation computed) on a synthetic signal.
        fn full_xcorr_argmax(mic: &[f32], far: &[f32], max_lag: usize) -> usize {
            let n = mic.len().min(far.len());
            let mut best = (0usize, f32::NEG_INFINITY);
            for lag in 0..max_lag.min(n) {
                let mut acc = 0.0f32;
                for i in lag..n {
                    acc += mic[i] * far[i - lag];
                }
                if acc > best.1 {
                    best = (lag, acc);
                }
            }
            best.0
        }
        let max_lag = 480;
        // Use a recording shorter than DELAY_SEG_LEN so the segmented estimator collapses to a single
        // full-signal window (seg_len = n, total_segments = 1), making the two methods compute over
        // identical samples — an exact-match check of the windowed cross-correlation semantics against the
        // brute-force linear cross-correlation the original O(n)-memory implementation computed. Sweep
        // delays at 0, mid-range, and near max_lag.
        let n = DELAY_SEG_LEN - 4_000; // < DELAY_SEG_LEN (32768) → one window
        for &delay in &[0usize, 240, 479] {
            let (mic, far) = delayed_echo(n, delay, 0, 99 + delay as u64);
            assert_eq!(
                estimate_delay(&mic, &far, max_lag),
                full_xcorr_argmax(&mic, &far, max_lag),
                "windowed estimate disagrees with full cross-correlation reference at delay {delay}"
            );
        }
    }

    #[test]
    fn estimate_delay_skips_leading_silence() {
        // Leading silence carries no far-end signal. The energy gate must drop the silent sampled segments
        // (their far-end energy is well below the loudest segment's) and still recover the delay from the
        // loud region — a whole-recording correlation would also work here, but this asserts the gated
        // segmented accumulation lands on signal, not on the silent prefix.
        let max_lag = 480;
        let lead = DELAY_SEG_LEN * 4 + 50_000; // silence spanning several analysis segments
        let (mic, far) = delayed_echo(lead + 300_000, 88, lead, 7);
        assert_eq!(estimate_delay(&mic, &far, max_lag), 88);
    }

    #[test]
    fn estimate_delay_memory_bound_is_constant_in_n() {
        // Lock in the HARD requirement: the largest single allocation and the analysis FFT size are bounded
        // by a constant independent of the recording length. We assert this *arithmetically* via the same
        // `delay_fft_len` the implementation uses, for absurdly long (3-hour-plus) recordings, WITHOUT
        // allocating those samples. fft_len must equal (DELAY_SEG_LEN + max_lag).next_power_of_two() for any
        // n >= DELAY_SEG_LEN (65536 for max_lag=480), never (2*n).next_power_of_two().
        let max_lag = 480usize;
        let bound = delay_fft_len(DELAY_SEG_LEN, max_lag);
        assert_eq!(bound, 65536, "fft_len bound for max_lag=480 must be 65536");
        for &n in &[
            DELAY_SEG_LEN,
            10 * DELAY_SEG_LEN,
            142_700_000,            // ~50 min at 48 kHz (the crashing case)
            48_000usize * 60 * 200, // ~3.3 hours
        ] {
            assert_eq!(
                delay_fft_len(n, max_lag),
                bound,
                "fft_len must be constant for n={n}"
            );
            // The old bug: fft sized over the whole recording would dwarf the bound. Once the recording
            // exceeds one analysis segment the segmented size stays flat while the O(n) size keeps growing.
            if n > DELAY_SEG_LEN {
                assert!(
                    delay_fft_len(n, max_lag) < (2 * n).next_power_of_two(),
                    "fft_len not bounded below the O(n) size for n={n}"
                );
            }
        }
        // Concrete byte budget: largest single buffer is `bound` complex f32 (8 bytes each) = 512 KiB.
        // Peak transient is the five fixed fft_len buffers + a small stack array, ~2.5 MiB — all O(1) in n.
        let largest_alloc_bytes = bound * std::mem::size_of::<Cf>();
        assert_eq!(
            largest_alloc_bytes,
            512 * 1024,
            "largest alloc must be exactly 512 KiB"
        );
        assert!(
            largest_alloc_bytes <= 1024 * 1024,
            "largest single allocation {largest_alloc_bytes} bytes exceeds 1 MiB"
        );
    }

    #[test]
    fn estimate_delay_invariant_to_recording_length() {
        // Segmented sampling across a long timeline must return the same constant delay as a short clip with
        // the SAME planted delay. Tile a base delayed-echo pair to ~5x length (preserving the delay) and
        // assert the estimate is identical at both lengths.
        let max_lag = 480;
        let delay = 53;
        let base_n = 120_000;
        let (mic_base, far_base) = delayed_echo(base_n, delay, 0, 2024);
        let short = estimate_delay(&mic_base, &far_base, max_lag);
        assert_eq!(short, delay, "base estimate must recover the planted delay");

        // Tile 5x. Each tile independently carries the same `delay`; concatenation preserves it within tiles.
        let mut mic_long = Vec::with_capacity(base_n * 5);
        let mut far_long = Vec::with_capacity(base_n * 5);
        for _ in 0..5 {
            mic_long.extend_from_slice(&mic_base);
            far_long.extend_from_slice(&far_base);
        }
        let long = estimate_delay(&mic_long, &far_long, max_lag);
        assert_eq!(
            long, delay,
            "delay estimate changed with recording length: {long} vs {delay}"
        );
    }

    #[test]
    fn estimate_delay_robust_to_localized_double_talk() {
        // The case Lens A's single-loudest-window picker would get wrong: mic = far delayed by D everywhere,
        // PLUS a loud uncorrelated near-end burst confined to the first segment (so the single highest-energy
        // mic region is near-end-only). Segmented accumulation + energy gating must still recover D.
        let max_lag = 480;
        let delay = 23;
        let n = DELAY_SEG_LEN * 8; // several segments
        let (mut mic, far) = delayed_echo(n, delay, 0, 555);
        // Loud uncorrelated near-end in the first segment, far louder than the echo amplitude.
        let burst = noise(DELAY_SEG_LEN, 3.0, 9090);
        for i in 0..DELAY_SEG_LEN {
            mic[i] += burst[i];
        }
        assert_eq!(
            estimate_delay(&mic, &far, max_lag),
            delay,
            "localized double-talk fooled the delay estimate"
        );
    }

    #[test]
    fn estimate_delay_zero_on_empty_and_zero_maxlag() {
        let mic = noise(10_000, 0.4, 1);
        let far = noise(10_000, 0.4, 2);
        assert_eq!(estimate_delay(&[], &far, 480), 0, "empty mic → 0");
        assert_eq!(estimate_delay(&mic, &[], 480), 0, "empty far → 0");
        assert_eq!(estimate_delay(&mic, &far, 0), 0, "zero max_lag → 0");
    }

    #[test]
    fn estimate_delay_robust_to_strong_near_end() {
        // Regression guard that averaging didn't weaken peak SNR: a known delay with uncorrelated near-end at
        // the SAME amplitude as the echo, spanning many segments, must still recover the delay.
        let max_lag = 480;
        let delay = 71;
        let n = 250_000; // > many DELAY_SEG_LEN segments
        let (mut mic, far) = delayed_echo(n, delay, 0, 313);
        // Echo amplitude is 0.5 (delayed_echo's far amp); add uncorrelated near-end at the same amplitude.
        let near = noise(n, 0.5, 4747);
        for i in 0..n {
            mic[i] += near[i];
        }
        assert_eq!(
            estimate_delay(&mic, &far, max_lag),
            delay,
            "strong uncorrelated near-end weakened the delay peak"
        );
    }

    #[test]
    fn estimate_delay_degenerate_inputs_never_panic() {
        // Hardening for the "never revisit stability" guarantee: pathological inputs must return a bounded
        // lag in [0, max_lag) without panicking (no slice OOB, usize overflow, or unwrap-on-empty). Each
        // case below was proven safe by hand in review; these lock that in against future edits.
        let max_lag = 480;

        // Mismatched lengths (n = min of the two), both orders.
        let short = noise(1_000, 0.4, 1);
        let long = noise(200_000, 0.4, 2);
        assert!(estimate_delay(&short, &long, max_lag) < max_lag);
        assert!(estimate_delay(&long, &short, max_lag) < max_lag);

        // max_lag larger than the whole signal (tiny n): seg_len shrinks to n, the lag search clamps in-bounds.
        let tiny = noise(10, 0.4, 3);
        assert!(estimate_delay(&tiny, &tiny, max_lag) < max_lag);
        assert!(estimate_delay(&tiny, &tiny, 100_000) < 100_000);

        // Non-finite samples: total_cmp is a total order, so max_by never panics; result is an
        // unspecified-but-bounded lag (documented on estimate_delay), never a crash.
        let mut bad = noise(100_000, 0.5, 4);
        bad[5_000] = f32::INFINITY;
        bad[60_000] = f32::NAN;
        let clean = noise(100_000, 0.5, 5);
        assert!(estimate_delay(&bad, &clean, max_lag) < max_lag);
        assert!(estimate_delay(&clean, &bad, max_lag) < max_lag);

        // Single sample, and silent (all-zero) far-end → 0, no panic.
        assert_eq!(estimate_delay(&[0.25f32], &[0.0f32], max_lag), 0);
        assert_eq!(
            estimate_delay(&noise(50_000, 0.4, 6), &vec![0.0f32; 50_000], max_lag),
            0
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
