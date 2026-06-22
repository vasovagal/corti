//! Streaming (in-flight) acoustic echo cancellation — ADR 0007.
//!
//! Where the offline path in [`crate::cancel`] held the whole call resident and ran a 2-pass sweep, this
//! runs a **single forward FDAF pass** as audio arrives, bounded by `O(block + filter_state + lookahead)`
//! independent of call length (fixes #67/#68/#32). The convergence + opening-quality the offline 2-pass
//! bought are recovered by a **tunable lookahead window** at the head of the stream:
//!
//! 1. **Warm-up / convergence** (offline pass-1): the first `lookahead` samples are buffered and pushed
//!    through the filter with adaptation on but output discarded, so `W`/`P` converge on real opening audio
//!    before any sample is emitted.
//! 2. **Opening quality** (offline pass-2 "restart from sample 0"): once warm, `W` is snapshotted and the
//!    buffered span is re-run through the now-warm filter, emitting that as the clean opening. The opening's
//!    residual suppression uses the *frozen* snapshot (the offline-frozen-`W` analog) so it matches
//!    offline-style suppression; steady-state blocks beyond the lookahead suppress with the live adapting
//!    `W`.
//! 3. **Delay-sync stability**: [`crate::estimate_delay`] runs over the whole lookahead window (a real
//!    multi-second span) and the result is **locked** for the rest of the call (the room delay is a single
//!    physical constant — ADR Decision 2), then realized as a primed `far_delay` queue so the far reference
//!    is delayed by exactly `delay` samples, bit-equivalent to the offline `x[delay..]` pre-shift.
//!
//! [`cancel`](crate::cancel) is re-expressed as a thin shim over this type with `lookahead == full input`,
//! so every existing offline test proves streaming ERLE parity through the same code path.

use std::collections::VecDeque;
use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::{AecConfig, estimate_delay};

type Cf = Complex<f32>;

/// Default lookahead window when `CORTI_AEC_LOOKAHEAD_SECS` is unset (seconds). Conservatively large per
/// ADR 0007 Decision 1/6 — a more robust short-window delay lock + opening convergence is worth the RAM and
/// the deferred opening, because corti records-then-transcribes (the latency is only at the very start).
const DEFAULT_LOOKAHEAD_SECS: f32 = 5.0;
/// Clamp range for the env-configured lookahead (seconds). 0.0 is legal (no warm-up — used by tests to show
/// the opening degrades); 30 s caps the buffered RAM.
const LOOKAHEAD_SECS_MIN: f32 = 0.0;
const LOOKAHEAD_SECS_MAX: f32 = 30.0;

/// Leaky-NLMS leakage λ — identical to the offline `run_pass` constant.
const LEAK: f32 = 1e-5;
/// Spectral floor for residual suppression (−40 dB) — identical to the offline `suppression_pass` constant.
const SUPPRESS_FLOOR: f32 = 0.01;

/// Read + clamp + round the lookahead window to a whole number of blocks of size `b`.
fn lookahead_samples_from_env(sample_rate: u32, b: usize) -> usize {
    let secs = std::env::var("CORTI_AEC_LOOKAHEAD_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(DEFAULT_LOOKAHEAD_SECS)
        .clamp(LOOKAHEAD_SECS_MIN, LOOKAHEAD_SECS_MAX);
    let raw = (secs * sample_rate as f32).round() as usize;
    round_up_to_block(raw, b)
}

/// Round `n` up to the nearest whole multiple of `b` (so warm-up is an integer number of blocks).
fn round_up_to_block(n: usize, b: usize) -> usize {
    n.div_ceil(b) * b
}

/// A single forward-pass frequency-domain block adaptive filter (FDAF) that processes mic+far audio
/// incrementally. See the module docs for the warm-up / lookahead / delay-sync state machine.
///
/// Usage: [`push`](Self::push) equal-length mic+far chunks of any length, collecting the returned cleaned
/// samples; then [`finish`](Self::finish) to flush. The total output length over all `push` + `finish`
/// equals the total number of mic samples pushed. **Callers must NOT assume `out.len() == input.len()`** —
/// `push` returns empty while warming, then a burst (the re-emitted opening) when the lookahead fills, then
/// ~`input.len()` per call in steady state.
pub struct StreamingAec {
    // ---- immutable config / plans (built once in new) ----
    cfg: AecConfig,
    sample_rate: u32,
    /// Block hop = `filter_len.max(1)`.
    b: usize,
    /// FFT size = `2 * b`.
    m: usize,
    fft: Arc<dyn Fft<f32>>,
    ifft: Arc<dyn Fft<f32>>,
    inv_m: f32,

    // ---- persisted adaptive-filter state (carried across push/blocks) ----
    /// Frequency-domain filter weights `[m]` (offline `w`).
    w: Vec<Cf>,
    /// Smoothed far-end power per bin `[m]` (offline `p`).
    p: Vec<f32>,
    /// Previous block's `b` far samples — the overlap-save history (offline read `x[base-b..base]`).
    prev_far: Vec<f32>,

    // ---- delay sync ----
    /// Locked acoustic delay (samples), set at end of warm-up.
    delay: usize,
    /// Far-end delay line: primed with `delay` zeros at lock; every far sample enters the back and
    /// `process_block` consumes from the front, delaying far by exactly `delay` samples.
    far_delay: VecDeque<f32>,

    // ---- warm-up / lookahead state machine ----
    warming: bool,
    lookahead_samples: usize,
    /// Buffered raw (un-delayed) opening, bounded by `lookahead_samples + b`.
    warm_mic: Vec<f32>,
    warm_far: Vec<f32>,
    /// `W` snapshot taken at end of warm-up; the suppression reference for the re-emitted opening.
    w_frozen: Option<Vec<Cf>>,

    // ---- leftover sub-block input (carried, < b each) ----
    pend_mic: Vec<f32>,
    pend_far: Vec<f32>,

    // ---- bookkeeping for the length invariant ----
    /// Total mic samples handed to push().
    pushed: usize,
    /// Total cleaned samples already emitted to the caller.
    emitted: usize,

    // ---- per-push scratch (sized m, reused; NOT O(n)) ----
    xspec: Vec<Cf>,
    yspec: Vec<Cf>,
    espec: Vec<Cf>,
    grad: Vec<Cf>,
}

impl StreamingAec {
    /// Build a streaming canceller. The lookahead window defaults from `CORTI_AEC_LOOKAHEAD_SECS` (or
    /// [`DEFAULT_LOOKAHEAD_SECS`]), clamped and rounded up to a whole multiple of the block size.
    pub fn new(sample_rate: u32, cfg: AecConfig) -> Self {
        let b = cfg.filter_len.max(1);
        let lookahead = lookahead_samples_from_env(sample_rate, b);
        Self::build(sample_rate, cfg, lookahead)
    }

    /// Build a streaming canceller with the lookahead pinned in samples. Used by the [`cancel`](crate::cancel)
    /// shim (passes `mic.len()` so the whole input is warm-up) and by tests.
    pub fn new_with_lookahead(sample_rate: u32, cfg: AecConfig, lookahead_samples: usize) -> Self {
        let b = cfg.filter_len.max(1);
        let lookahead = round_up_to_block(lookahead_samples, b);
        Self::build(sample_rate, cfg, lookahead)
    }

    fn build(sample_rate: u32, cfg: AecConfig, lookahead_samples: usize) -> Self {
        let b = cfg.filter_len.max(1);
        let m = 2 * b;
        let mut planner = FftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(m);
        let ifft = planner.plan_fft_inverse(m);
        Self {
            cfg,
            sample_rate,
            b,
            m,
            fft,
            ifft,
            inv_m: 1.0 / m as f32,
            w: vec![Cf::new(0.0, 0.0); m],
            p: vec![0.0f32; m],
            prev_far: vec![0.0f32; b],
            delay: 0,
            far_delay: VecDeque::new(),
            warming: true,
            lookahead_samples,
            warm_mic: Vec::new(),
            warm_far: Vec::new(),
            w_frozen: None,
            pend_mic: Vec::new(),
            pend_far: Vec::new(),
            pushed: 0,
            emitted: 0,
            xspec: vec![Cf::new(0.0, 0.0); m],
            yspec: vec![Cf::new(0.0, 0.0); m],
            espec: vec![Cf::new(0.0, 0.0); m],
            grad: vec![Cf::new(0.0, 0.0); m],
        }
    }

    /// Push equal-length mic + far chunks. Returns the cleaned samples that are READY (see the type docs:
    /// empty while warming, a burst when the lookahead fills, then ~`input.len()` in steady state).
    ///
    /// Panics if `mic.len() != far.len()` (the capture path always supplies aligned mic/far blocks).
    pub fn push(&mut self, mic: &[f32], far: &[f32]) -> Vec<f32> {
        assert_eq!(mic.len(), far.len(), "mic/far chunk lengths must match");
        self.pushed += mic.len();
        let mut out = Vec::new();

        if self.warming {
            self.warm_mic.extend_from_slice(mic);
            self.warm_far.extend_from_slice(far);
            if self.warm_mic.len() >= self.lookahead_samples {
                self.lock_and_emit_opening(&mut out);
            }
            self.emitted += out.len();
            return out;
        }

        self.stream_steady(mic, far, &mut out);
        self.emitted += out.len();
        out
    }

    /// Flush remaining state. If still warming (a call shorter than the lookahead), lock + re-emit on the
    /// partial buffer. Then process the trailing partial block (zero-padded) and truncate the whole stream's
    /// output so the TOTAL length over all `push` + `finish` equals the total mic samples pushed.
    pub fn finish(mut self) -> Vec<f32> {
        let mut out = Vec::new();

        if self.warming {
            // Short call: warm up + re-emit on whatever was buffered, exactly like offline cancel() on a
            // short input.
            self.lock_and_emit_opening(&mut out);
        }

        // Drain any leftover sub-block input by zero-padding to a full block.
        if !self.pend_mic.is_empty() {
            let pad = self.b - self.pend_mic.len();
            let mut mic_blk = std::mem::take(&mut self.pend_mic);
            let mut far_blk = std::mem::take(&mut self.pend_far);
            mic_blk.resize(self.b, 0.0);
            far_blk.resize(self.b, 0.0);
            let suppress = self.cfg.suppress_residual > 0.0;
            let block = self.process_block(&mic_blk, &far_blk, true, suppress, SuppressW::Live);
            out.extend_from_slice(&block);
            // `pad` zero samples were appended only to fill the block; they are not real mic input.
            let _ = pad;
        }

        self.emitted += out.len();

        // Length invariant: total emitted == total pushed. Truncate the tail padding that the final
        // zero-padded block contributed beyond the real input. (We can only ever have emitted >= pushed by
        // the final block's padding, never less, because every real input sample is accounted for.)
        if self.emitted > self.pushed {
            let overshoot = self.emitted - self.pushed;
            let keep = out.len().saturating_sub(overshoot);
            out.truncate(keep);
        }
        out
    }

    /// Lock the delay over the warm-up window, run convergence (discarded) + opening re-emit. Transitions
    /// `warming -> false` and frees the warm buffers. Appends the cleaned opening to `out`.
    fn lock_and_emit_opening(&mut self, out: &mut Vec<f32>) {
        // (1) Lock the delay over the whole warm-up window. Search window from cfg.max_lag_ms (default
        // 10 ms, the legacy hardcoded value). This drives both the live streaming path and the offline
        // cancel() shim, which is `new_with_lookahead(.., n)` over this same kernel.
        let max_lag = (self.sample_rate as f32 * self.cfg.max_lag_ms / 1000.0) as usize;
        self.delay = estimate_delay(&self.warm_mic, &self.warm_far, max_lag);

        // Prime the far delay line with `delay` zeros (offline's x[delay..] pre-shift analog).
        self.far_delay.clear();
        for _ in 0..self.delay {
            self.far_delay.push_back(0.0);
        }

        // Number of WHOLE blocks in the buffered opening. A trailing partial (< b) is carried into pend_*
        // so steady state resumes block-aligned.
        let warm_len = self.warm_mic.len();
        let whole = warm_len / self.b;
        let span = whole * self.b;

        let suppress = self.cfg.suppress_residual > 0.0;
        let warm_mic = std::mem::take(&mut self.warm_mic);
        let warm_far = std::mem::take(&mut self.warm_far);

        // (2) Convergence pass: adapt with output discarded so W/P warm on real opening audio.
        for k in 0..whole {
            let lo = k * self.b;
            let mic_blk = &warm_mic[lo..lo + self.b];
            let far_blk = &warm_far[lo..lo + self.b];
            let _ = self.process_block(mic_blk, far_blk, true, false, SuppressW::Live);
        }

        // Snapshot the converged W as the suppression reference for the re-emitted opening.
        self.w_frozen = Some(self.w.clone());

        // Reset the overlap-save + delay-line state so the re-emit starts from sample 0 of the opening,
        // exactly like the offline pass-2 "restart from sample 0 with the converged filter".
        self.prev_far.iter_mut().for_each(|v| *v = 0.0);
        self.far_delay.clear();
        for _ in 0..self.delay {
            self.far_delay.push_back(0.0);
        }

        // (3) Opening re-emit: re-run the buffered span through the warm filter, adapting, suppressing
        // against the frozen snapshot, EMITTING the result.
        for k in 0..whole {
            let lo = k * self.b;
            let mic_blk = &warm_mic[lo..lo + self.b];
            let far_blk = &warm_far[lo..lo + self.b];
            let block = self.process_block(mic_blk, far_blk, true, suppress, SuppressW::Frozen);
            out.extend_from_slice(&block);
        }

        // The frozen snapshot has done its job (only the opening uses it); steady state uses live W.
        self.w_frozen = None;

        // Carry the trailing partial block (< b) into pend_* for steady state. These samples have NOT been
        // emitted yet — they will flow through process_block once enough arrives (or in finish()).
        if span < warm_len {
            self.pend_mic.extend_from_slice(&warm_mic[span..]);
            self.pend_far.extend_from_slice(&warm_far[span..]);
        }

        self.warming = false;
    }

    /// Steady-state streaming: prepend any pending sub-block input, process whole blocks with the live
    /// adapting filter, buffer the `< b` remainder.
    fn stream_steady(&mut self, mic: &[f32], far: &[f32], out: &mut Vec<f32>) {
        let mut mic_buf = std::mem::take(&mut self.pend_mic);
        let mut far_buf = std::mem::take(&mut self.pend_far);
        mic_buf.extend_from_slice(mic);
        far_buf.extend_from_slice(far);

        let whole = mic_buf.len() / self.b;
        let span = whole * self.b;
        let suppress = self.cfg.suppress_residual > 0.0;

        for k in 0..whole {
            let lo = k * self.b;
            // Copy out of the buffers to avoid borrowing self immutably + mutably at once.
            let mic_blk: Vec<f32> = mic_buf[lo..lo + self.b].to_vec();
            let far_blk: Vec<f32> = far_buf[lo..lo + self.b].to_vec();
            let block = self.process_block(&mic_blk, &far_blk, true, suppress, SuppressW::Live);
            out.extend_from_slice(&block);
        }

        self.pend_mic.extend_from_slice(&mic_buf[span..]);
        self.pend_far.extend_from_slice(&far_buf[span..]);
    }

    /// Single source of truth for the per-block FDAF math: overlap-save FFT, `Y = X⊙W`, `e = d − y`,
    /// double-talk freeze, one-sided `P` update, constrained leaky-NLMS, and (when `suppress`) the
    /// spectral-subtraction gain. Returns `b` cleaned samples.
    ///
    /// `adapt`: run the NLMS weight update (false during a discard-only convergence sub-pass — currently
    /// always true, kept for symmetry with the offline 2-pass and future tuning).
    /// `suppress`: apply residual spectral subtraction.
    /// `suppress_w`: which `W` the suppression gain reads — the live adapting weights, or the end-of-warmup
    /// frozen snapshot (the opening uses the snapshot, the offline-frozen-`W` analog).
    // The block kernel reads/writes several length-`b`/`m` arrays in lockstep by index (mic_blk, cur,
    // espec, yspec, clean, w_sup); a single enumerate can't span them, and the explicit index mirrors the
    // offline `run_pass`/`suppression_pass` math 1:1, so keep the range loops.
    #[allow(clippy::needless_range_loop)]
    fn process_block(
        &mut self,
        mic_blk: &[f32],
        far_blk: &[f32],
        adapt: bool,
        suppress: bool,
        suppress_w: SuppressW,
    ) -> Vec<f32> {
        debug_assert_eq!(mic_blk.len(), self.b);
        debug_assert_eq!(far_blk.len(), self.b);
        let b = self.b;
        let m = self.m;
        let inv_m = self.inv_m;

        // Pull the delayed far block out of the delay line: push this block's far samples to the back, then
        // pop `b` from the front. With the queue primed with `delay` zeros at lock, this realizes the
        // offline `x[delay..]` pre-shift exactly (first `delay` far positions are zeros).
        let mut cur = [0.0f32; 0].to_vec();
        cur.reserve(b);
        for &v in far_blk {
            self.far_delay.push_back(v);
        }
        for _ in 0..b {
            cur.push(self.far_delay.pop_front().unwrap_or(0.0));
        }

        // xframe = [prev_far ; cur] (overlap-save), FFT in place → X. The first-ever block has prev_far
        // zero-filled, matching offline k==0.
        for s in self.xspec.iter_mut() {
            *s = Cf::new(0.0, 0.0);
        }
        for i in 0..b {
            self.xspec[i] = Cf::new(self.prev_far[i], 0.0);
        }
        for i in 0..b {
            self.xspec[b + i] = Cf::new(cur[i], 0.0);
        }
        self.fft.process(&mut self.xspec);

        // Y = X ⊙ W ; y = IFFT(Y)/M. Valid linear-convolution samples are the last B (overlap-save).
        for j in 0..m {
            self.yspec[j] = self.xspec[j] * self.w[j];
        }
        self.ifft.process(&mut self.yspec);

        // eb = d_k − yb → cleaned block; build the error frame [0_B ; eb]; accumulate energies for the gate.
        let mut clean = vec![0.0f32; b];
        let mut d_energy = 0.0f32;
        let mut x_energy = 0.0f32;
        for i in 0..b {
            let y_i = self.yspec[b + i].re * inv_m;
            let e_i = mic_blk[i] - y_i;
            clean[i] = e_i;
            self.espec[i] = Cf::new(0.0, 0.0);
            self.espec[b + i] = Cf::new(e_i, 0.0);
            d_energy += mic_blk[i] * mic_blk[i];
            x_energy += cur[i] * cur[i];
        }

        let double_talk = d_energy > self.cfg.double_talk_ratio * x_energy;

        // Adaptation (unless double-talk): one-sided P update + constrained leaky-NLMS. This is verbatim the
        // offline run_pass update.
        if adapt && !double_talk {
            for j in 0..m {
                let inst = self.xspec[j].norm_sqr();
                if inst > self.p[j] {
                    self.p[j] = inst.max(self.cfg.eps);
                } else {
                    self.p[j] = self.cfg.power_smoothing * self.p[j]
                        + (1.0 - self.cfg.power_smoothing) * inst;
                    self.p[j] = self.p[j].max(self.cfg.eps);
                }
            }
            // E = FFT(error frame).
            self.fft.process(&mut self.espec);
            // G = conj(X) ⊙ E / (P + eps).
            for j in 0..m {
                self.grad[j] =
                    (self.xspec[j].conj() * self.espec[j]).unscale(self.p[j] + self.cfg.eps);
            }
            // Constrain to the first B time-domain taps (normalized), zero the wrap-around tail.
            self.ifft.process(&mut self.grad);
            for g in self.grad[..b].iter_mut() {
                *g = g.scale(inv_m);
            }
            for g in self.grad[b..].iter_mut() {
                *g = Cf::new(0.0, 0.0);
            }
            self.fft.process(&mut self.grad);
            // W = (1−λ)·W + μ·G (leaky NLMS).
            for j in 0..m {
                self.w[j] = self.w[j].scale(1.0 - LEAK) + self.grad[j].scale(self.cfg.mu);
            }
            // espec now holds FFT(error); the suppression branch below rebuilds it from `clean`.
        }

        // Update overlap-save history AFTER reading prev_far as the first FFT half (correctness trap).
        self.prev_far.copy_from_slice(&cur);

        if !suppress {
            return clean;
        }

        // Residual spectral subtraction. Mirrors offline suppression_pass: Y = X⊙W_sup is the echo estimate,
        // E = D − Y the FDAF error; suppress bins where the echo estimate dominates the error. During
        // double-talk the user is speaking — pass through unsuppressed.
        let alpha = self.cfg.suppress_residual;
        let eps = self.cfg.eps;
        let w_sup: &[Cf] = match suppress_w {
            SuppressW::Live => &self.w,
            SuppressW::Frozen => self.w_frozen.as_deref().unwrap_or(&self.w),
        };

        // Echo estimate Y = X ⊙ W_sup (reuse yspec).
        for j in 0..m {
            self.yspec[j] = self.xspec[j] * w_sup[j];
        }
        // Time-domain echo to recompute e = d − y against the suppression W.
        let mut yspec_td = self.yspec.clone();
        self.ifft.process(&mut yspec_td);

        let mut sd_energy = 0.0f32;
        let mut sx_energy = 0.0f32;
        for i in 0..b {
            let y_i = yspec_td[b + i].re * inv_m;
            let e_i = mic_blk[i] - y_i;
            self.espec[i] = Cf::new(0.0, 0.0);
            self.espec[b + i] = Cf::new(e_i, 0.0);
            sd_energy += mic_blk[i] * mic_blk[i];
            sx_energy += cur[i] * cur[i];
        }

        if sd_energy > self.cfg.double_talk_ratio * sx_energy {
            for i in 0..b {
                clean[i] = self.espec[b + i].re;
            }
            return clean;
        }

        // Spectral subtraction gain.
        self.fft.process(&mut self.espec);
        for j in 0..m {
            let echo_pow = self.yspec[j].norm_sqr();
            let error_pow = self.espec[j].norm_sqr();
            let gain = (1.0 - alpha * echo_pow / (error_pow + eps)).max(SUPPRESS_FLOOR);
            self.espec[j] = self.espec[j].scale(gain);
        }
        self.ifft.process(&mut self.espec);
        for i in 0..b {
            clean[i] = self.espec[b + i].re * inv_m;
        }
        clean
    }
}

/// Selects which `W` the residual suppressor reads from.
#[derive(Clone, Copy)]
enum SuppressW {
    /// The live adapting weights (steady state).
    Live,
    /// The end-of-warmup frozen snapshot (the re-emitted opening; offline-frozen-`W` analog).
    Frozen,
}
