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
//! 3. **Delay-sync stability**: `estimate_delay` runs over the whole lookahead window (a real
//!    multi-second span) and the result is **locked** for the rest of the call (the room delay is a single
//!    physical constant — ADR Decision 2), then realized as a primed `far_delay` queue so the far reference
//!    is delayed by exactly `delay` samples, bit-equivalent to the offline `x[delay..]` pre-shift.
//!
//! [`cancel`](crate::cancel) is re-expressed as a thin shim over this type with `lookahead == full input`,
//! so every existing offline test proves streaming ERLE parity through the same code path.
//!
//! Alongside the audio, each **emitted** block leaves a [`BlockStats`] record in a bounded ring
//! ([`StreamingAec::block_stats`]) — the energies and gate decisions the filter already computes, on the
//! cleaned timeline, for #107's echo-ghost diagnosis. It is instrumentation only: no emitted sample depends
//! on whether anyone reads it.

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

/// Read and clamp the effective lookahead once. Capture stores this value beside its AEC settings so a
/// retry or provenance record cannot drift when the process environment changes later.
pub fn configured_lookahead_seconds() -> f32 {
    std::env::var("CORTI_AEC_LOOKAHEAD_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(DEFAULT_LOOKAHEAD_SECS)
        .clamp(LOOKAHEAD_SECS_MIN, LOOKAHEAD_SECS_MAX)
}

/// Convert a clamped lookahead duration to the exact whole-block sample count used by the filter.
pub fn lookahead_samples_for(sample_rate: u32, filter_len: usize, seconds: f32) -> usize {
    let b = filter_len.max(1);
    let seconds = if seconds.is_finite() {
        seconds.clamp(LOOKAHEAD_SECS_MIN, LOOKAHEAD_SECS_MAX)
    } else {
        DEFAULT_LOOKAHEAD_SECS
    };
    let raw = (seconds * sample_rate as f32).round() as usize;
    round_up_to_block(raw, b)
}

/// Round `n` up to the nearest whole multiple of `b` (so warm-up is an integer number of blocks).
fn round_up_to_block(n: usize, b: usize) -> usize {
    n.div_ceil(b) * b
}

/// How many [`BlockStats`] the ring retains before dropping the oldest. 4096 blocks is ≈ 11.6 minutes at the
/// default 8192-tap hop / 48 kHz — long enough that a consumer draining once per live window or once per
/// `push` never loses a block, and small enough (≈128 KiB) that a consumer that never drains at all still
/// costs nothing. Overflow is counted, never silent: see [`StreamingAec::stats_dropped`].
pub const MAX_BLOCK_STATS: usize = 4096;

/// Energy floor for the dB conversions in [`SpanStats`] (≈ −200 dB), so a digitally silent span reports a
/// finite floor rather than `-inf`.
const ENERGY_FLOOR: f32 = 1e-20;

/// One processed block's echo bookkeeping — the quantities the adaptation gate and the residual suppressor
/// already compute, captured rather than recomputed. Instrumentation only: recording these changes no
/// emitted sample (#107 diagnostics / #149 phase 3).
///
/// **Timeline contract.** `t_start_secs` is call-relative on the **emitted (cleaned) timeline** — the offset
/// of this block's first *cleaned* sample from the first cleaned sample of the call. Because the filter
/// emits exactly one output sample per pushed mic sample (`total emitted == total pushed`), that is also the
/// offset of the block's first mic-input sample, i.e. the timestamp the transcriber will put on this audio.
/// **The lookahead is already accounted for**: the warm-up convergence sub-pass discards its output and
/// records nothing, and the opening re-emit restarts the clock at `0.0`. Block *k* therefore covers cleaned
/// samples `[k·filter_len, (k+1)·filter_len)` — of `-clean.wav`, or of the `push` return stream — with no
/// lookahead offset to subtract. The final block may be short: `finish` zero-pads it to a whole block and
/// then truncates the emitted tail, so its energies cover the padding while its start time stays exact.
///
/// Energies are **sums of squares over the block** (not means), all on the same `f32` sample scale, so a
/// ratio of two of them is directly an ERLE-style figure.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BlockStats {
    /// Start of this block on the emitted/cleaned timeline (seconds from the start of the call).
    pub t_start_secs: f64,
    /// Σ d² — raw mic energy over the block (the gate's numerator).
    pub mic_energy: f32,
    /// Σ x² — energy of the **delay-aligned** far-end reference actually convolved for this block (the
    /// gate's denominator), not of the caller's un-delayed far chunk.
    pub far_energy: f32,
    /// Σ y² — energy of the echo estimate that was subtracted from the mic for this block. Taken from the
    /// `W` the block actually used: the residual suppressor's `W` when suppression ran, the live adapting
    /// `W` otherwise.
    pub echo_estimate_energy: f32,
    /// Σ e² — energy of the samples this block **emits**: the FDAF error, or the suppressed output when the
    /// suppressor ran.
    pub error_energy: f32,
    /// The adaptation gate fired (`Σ d² > double_talk_ratio · Σ x²`): the NLMS update was frozen for this
    /// block because the near end is judged to dominate.
    pub double_talk: bool,
    /// The residual suppressor actually applied its spectral-subtraction gain to this block. False when
    /// `suppress_residual` is 0 (disabled) and false when the suppressor's own gate — today the *same*
    /// `Σ d² > ratio · Σ x²` test — bypassed it and passed the unsuppressed FDAF error through. That
    /// shared test is #107 root cause #1; the two flags are recorded separately so the pair stays
    /// meaningful once the gates diverge.
    pub suppressed: bool,
}

/// Aggregate of the blocks overlapping one time span — the accessor a transcript-cleanup pass consumes to
/// ask "was this mic segment mostly echo?" (#149 phase 1 will call it; nothing does yet).
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SpanStats {
    /// Number of blocks folded in.
    pub blocks: usize,
    /// 10·log10 of the **mean block** mic energy. The mean is taken over energies and converted once, not
    /// averaged in dB, so a single silent block cannot dominate the answer.
    pub mean_mic_db: f32,
    /// 10·log10 of the mean block echo-estimate energy.
    pub mean_echo_estimate_db: f32,
    /// 10·log10 of the mean block emitted-error energy.
    pub mean_error_db: f32,
    /// Fraction of the folded blocks whose adaptation gate fired.
    pub double_talk_fraction: f32,
    /// Fraction of the folded blocks the residual suppressor actually processed.
    pub suppressed_fraction: f32,
}

/// Summarize the blocks overlapping `[start, end]` (seconds on the [`BlockStats`] timeline).
///
/// `blocks` must be sorted by `t_start_secs` — [`StreamingAec::block_stats`] drains them in order, so a
/// consumer that only ever appends is already sorted. A block overlaps the span when it *starts* inside it
/// **or** when it is the last block starting at or before `start` (the block that contains `start`). Since
/// blocks tile the timeline contiguously, that is exactly the set of blocks whose audio intersects the
/// span — including the single containing block when the span is shorter than one hop.
///
/// Returns `None` when nothing overlaps: an empty slice, or a span that ends before the first block starts.
/// Degenerate the other way, a span starting after the last block's start collapses to that last block —
/// the block extents are not recorded, so "after the end of the record" is not distinguishable here.
/// Consumers ask about spans inside the call, where the tiling assumption holds.
pub fn span_stats(blocks: &[BlockStats], start: f64, end: f64) -> Option<SpanStats> {
    let (start, end) = if end < start {
        (end, start)
    } else {
        (start, end)
    };

    // The block containing `start` is the last one starting at or before it; everything after that up to
    // `end` also overlaps. `partition_point` needs the slice sorted by `t_start_secs`, which is the
    // documented precondition.
    let first_after_start = blocks.partition_point(|b| b.t_start_secs <= start);
    let lo = first_after_start.saturating_sub(1);
    let hi = blocks.partition_point(|b| b.t_start_secs <= end);
    if hi <= lo {
        return None;
    }
    let sel = &blocks[lo..hi];
    let n = sel.len() as f32;
    let mut mic = 0.0f64;
    let mut echo = 0.0f64;
    let mut err = 0.0f64;
    let mut dt = 0usize;
    let mut sup = 0usize;
    for b in sel {
        mic += b.mic_energy as f64;
        echo += b.echo_estimate_energy as f64;
        err += b.error_energy as f64;
        dt += usize::from(b.double_talk);
        sup += usize::from(b.suppressed);
    }
    let db = |total: f64| -> f32 {
        let mean = (total / sel.len() as f64) as f32;
        10.0 * mean.max(ENERGY_FLOOR).log10()
    };
    Some(SpanStats {
        blocks: sel.len(),
        mean_mic_db: db(mic),
        mean_echo_estimate_db: db(echo),
        mean_error_db: db(err),
        double_talk_fraction: dt as f32 / n,
        suppressed_fraction: sup as f32 / n,
    })
}

/// What [`StreamingAec::finish_with_stats`] hands back. `finish` consumes the filter, so this is the only
/// way to reach the blocks the flush itself recorded (and, for a call shorter than the lookahead, every
/// block of the call).
#[derive(Debug, Clone, PartialEq)]
pub struct FinishOutput {
    /// The cleaned tail, identical to what [`StreamingAec::finish`] returns.
    pub audio: Vec<f32>,
    /// Statistics still in the ring, oldest first.
    pub stats: Vec<BlockStats>,
    /// Lifetime count of statistics the bounded ring dropped (see [`MAX_BLOCK_STATS`]).
    pub stats_dropped: u64,
    /// The acoustic delay locked at the end of warm-up, in samples.
    pub delay_samples: usize,
}

/// The per-block observation `process_block` hands its caller, which decides whether it reaches the ring.
/// Identical to [`BlockStats`] minus `t_start_secs`, which only the emitting call sites can assign.
#[derive(Clone, Copy)]
struct BlockObservation {
    mic_energy: f32,
    far_energy: f32,
    echo_estimate_energy: f32,
    error_energy: f32,
    double_talk: bool,
    suppressed: bool,
}

/// A single forward-pass frequency-domain block adaptive filter (FDAF) that processes mic+far audio
/// incrementally. See the module docs for the warm-up / lookahead / delay-sync state machine.
///
/// Usage: [`push`](Self::push) equal-length mic+far chunks of any length, collecting the returned cleaned
/// samples; then [`finish`](Self::finish) to flush. The total output length over all `push` + `finish`
/// equals the total number of mic samples pushed. **Callers must NOT assume `out.len() == input.len()`** —
/// `push` returns empty while warming, then a burst (the re-emitted opening) when the lookahead fills, then
/// ~`input.len()` per call in steady state.
///
/// Diagnostics ride along: [`block_stats`](Self::block_stats) drains the per-block echo record and
/// [`finish_with_stats`](Self::finish_with_stats) returns the tail of it with the locked delay.
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
    /// Buffered raw (un-delayed) opening, bounded by `lookahead_samples` even when a caller supplies the
    /// whole recording in one `push`.
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

    // ---- instrumentation: bounded per-block statistics (#107 / #149 phase 3) ----
    /// Bounded ring of per-block echo statistics, oldest first. Pre-allocated to [`MAX_BLOCK_STATS`] so a
    /// push never allocates.
    stats: VecDeque<BlockStats>,
    /// Lifetime count of statistics evicted because the ring was full.
    stats_dropped: u64,
    /// Emitted-sample cursor that stamps `t_start_secs`. Advances by `b` per RECORDED block only, which is
    /// what keeps the stats timeline equal to the cleaned-audio timeline across the warm-up (see
    /// [`BlockStats`]).
    stats_clock: usize,

    // ---- per-push scratch (sized m, reused; NOT O(n)) ----
    xspec: Vec<Cf>,
    yspec: Vec<Cf>,
    espec: Vec<Cf>,
    grad: Vec<Cf>,
}

impl StreamingAec {
    /// Build a streaming canceller. The lookahead window defaults from `CORTI_AEC_LOOKAHEAD_SECS` (or
    /// `DEFAULT_LOOKAHEAD_SECS`), clamped and rounded up to a whole multiple of the block size.
    pub fn new(sample_rate: u32, cfg: AecConfig) -> Self {
        let seconds = configured_lookahead_seconds();
        Self::new_with_lookahead_seconds(sample_rate, cfg, seconds)
    }

    /// Build a streaming canceller with a lookahead duration captured by the caller. Unlike [`new`](Self::new),
    /// this never re-reads the environment, which keeps the writer and its durable processing record exact.
    pub fn new_with_lookahead_seconds(sample_rate: u32, cfg: AecConfig, seconds: f32) -> Self {
        let lookahead = lookahead_samples_for(sample_rate, cfg.filter_len, seconds);
        Self::build(sample_rate, cfg, lookahead)
    }

    /// Build a streaming canceller with the lookahead pinned in samples. Used by the [`cancel`](crate::cancel)
    /// shim (passes `mic.len()` so the whole input is warm-up) and by tests.
    pub fn new_with_lookahead(sample_rate: u32, cfg: AecConfig, lookahead_samples: usize) -> Self {
        let b = cfg.filter_len.max(1);
        let lookahead = round_up_to_block(lookahead_samples, b);
        Self::build(sample_rate, cfg, lookahead)
    }

    /// Maximum number of pushed samples that may legitimately be awaiting output. Callers can use this to
    /// enforce a hard bound around a generic streaming-filter seam.
    pub fn max_output_lag_samples(&self) -> usize {
        self.lookahead_samples.saturating_add(self.b)
    }

    /// **Drain** the per-block statistics recorded since the last call, oldest first. Instrumentation only —
    /// draining (or never draining) cannot change a single emitted sample. See [`BlockStats`] for the
    /// timeline contract; the ring is bounded by [`MAX_BLOCK_STATS`], so a consumer that wants every block
    /// of a long call must drain periodically and check [`stats_dropped`](Self::stats_dropped).
    pub fn block_stats(&mut self) -> Vec<BlockStats> {
        self.stats.drain(..).collect()
    }

    /// Lifetime count of block statistics the bounded ring dropped. Non-zero means the consumer drained too
    /// slowly (or not at all) and the record has a hole at its OLD end — the retained blocks are the most
    /// recent [`MAX_BLOCK_STATS`].
    pub fn stats_dropped(&self) -> u64 {
        self.stats_dropped
    }

    /// The acoustic mic↔far delay locked at the end of warm-up, in samples; `None` while still warming.
    /// Constant for the rest of the call by ADR 0007 Decision 2.
    pub fn locked_delay_samples(&self) -> Option<usize> {
        (!self.warming).then_some(self.delay)
    }

    /// Stamp an observation with its emitted-timeline start and push it into the bounded ring, evicting the
    /// oldest (and counting the eviction) when full. The only allocation-free write on the hot path.
    fn record_block(&mut self, obs: BlockObservation) {
        let t_start_secs = self.stats_clock as f64 / self.sample_rate.max(1) as f64;
        self.stats_clock += self.b;
        if self.stats.len() >= MAX_BLOCK_STATS {
            self.stats.pop_front();
            self.stats_dropped += 1;
        }
        self.stats.push_back(BlockStats {
            t_start_secs,
            mic_energy: obs.mic_energy,
            far_energy: obs.far_energy,
            echo_estimate_energy: obs.echo_estimate_energy,
            error_energy: obs.error_energy,
            double_talk: obs.double_talk,
            suppressed: obs.suppressed,
        });
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
            stats: VecDeque::with_capacity(MAX_BLOCK_STATS),
            stats_dropped: 0,
            stats_clock: 0,
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
            // Consume only the prefix that belongs to the configured opening. Treating the caller's whole
            // slice atomically made a one-shot batch call redefine a five-second lookahead as "the whole
            // recording", so every sample was run through both convergence and re-emission.
            let needed = self.lookahead_samples.saturating_sub(self.warm_mic.len());
            let opening = needed.min(mic.len());
            self.warm_mic.extend_from_slice(&mic[..opening]);
            self.warm_far.extend_from_slice(&far[..opening]);
            debug_assert!(self.warm_mic.len() <= self.lookahead_samples);
            if self.warm_mic.len() >= self.lookahead_samples {
                self.lock_and_emit_opening(&mut out);
            }
            if self.warming {
                // This chunk ended before the opening filled; all of it is now in the bounded warm buffer.
                self.emitted += out.len();
                return out;
            }
            // The same push may contain steady-state audio after the exact opening prefix. Process that
            // remainder now rather than dropping it or making chunk boundaries observable.
            self.stream_steady(&mic[opening..], &far[opening..], &mut out);
        } else {
            self.stream_steady(mic, far, &mut out);
        }
        self.emitted += out.len();
        out
    }

    /// Flush remaining state. If still warming (a call shorter than the lookahead), lock + re-emit on the
    /// partial buffer. Then process the trailing partial block (zero-padded) and truncate the whole stream's
    /// output so the TOTAL length over all `push` + `finish` equals the total mic samples pushed.
    pub fn finish(self) -> Vec<f32> {
        self.finish_with_stats().audio
    }

    /// [`finish`](Self::finish) plus the instrumentation the caller can no longer reach once the filter is
    /// consumed: whatever is still in the stats ring (including the blocks this flush itself records — and,
    /// for a call shorter than the lookahead, every block of the call), the lifetime drop count, and the
    /// locked delay.
    pub fn finish_with_stats(mut self) -> FinishOutput {
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
            let (block, obs) =
                self.process_block(&mic_blk, &far_blk, true, suppress, SuppressW::Live);
            self.record_block(obs);
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

        FinishOutput {
            audio: out,
            stats: self.stats.drain(..).collect(),
            stats_dropped: self.stats_dropped,
            delay_samples: self.delay,
        }
    }

    /// Lock the delay over the warm-up window, run convergence (discarded) + opening re-emit. Transitions
    /// `warming -> false` and frees the warm buffers. Appends the cleaned opening to `out`.
    fn lock_and_emit_opening(&mut self, out: &mut Vec<f32>) {
        // (1) Lock the delay over the whole warm-up window. Search window from cfg.max_lag_ms (default
        // 10 ms, the legacy hardcoded value). This drives both the live streaming path and the offline
        // cancel() shim, which is `new_with_lookahead(.., n)` over this same kernel.
        let max_lag = (self.sample_rate as f32 * self.cfg.max_lag_ms / 1000.0) as usize;
        self.delay = estimate_delay(&self.warm_mic, &self.warm_far, max_lag);

        // #107's cheapest diagnostic: the delay is locked exactly once per call and never re-estimated, so a
        // lock that pins to the edge of the search window (or to 0) is the whole hypothesis, visible in one
        // line. Logged before the buffers are consumed.
        let sr = self.sample_rate.max(1) as f32;
        tracing::info!(
            target: "corti::aec",
            delay_samples = self.delay as u64,
            delay_ms = (self.delay as f32 * 1000.0 / sr) as f64,
            max_lag_samples = max_lag as u64,
            max_lag_ms = self.cfg.max_lag_ms as f64,
            warmup_samples = self.warm_mic.len() as u64,
            warmup_secs = (self.warm_mic.len() as f32 / sr) as f64,
            filter_len = self.b as u64,
            sample_rate = self.sample_rate,
            "locked AEC mic-to-far delay"
        );

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
        // No stats here on purpose: this sub-pass exists only to converge W/P and its output is discarded,
        // so recording it would put the opening on the timeline twice. The clock starts at the re-emit.
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
            let (block, obs) =
                self.process_block(mic_blk, far_blk, true, suppress, SuppressW::Frozen);
            self.record_block(obs);
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

    /// Steady-state streaming: fill any pending sub-block, process whole blocks with the live adapting
    /// filter, and retain only the `< b` tail. Never concatenate the caller's entire slice into staging —
    /// even a one-shot whole-call push keeps only bounded filter/pending state.
    fn stream_steady(&mut self, mic: &[f32], far: &[f32], out: &mut Vec<f32>) {
        debug_assert_eq!(mic.len(), far.len());
        let suppress = self.cfg.suppress_residual > 0.0;
        let mut pos = 0usize;

        // Complete a partial block left by the previous push before reading aligned blocks directly from
        // this slice. `pend_*` stays strictly smaller than b between calls.
        if !self.pend_mic.is_empty() {
            let take = (self.b - self.pend_mic.len()).min(mic.len());
            self.pend_mic.extend_from_slice(&mic[..take]);
            self.pend_far.extend_from_slice(&far[..take]);
            pos = take;
            if self.pend_mic.len() == self.b {
                let mic_blk = std::mem::take(&mut self.pend_mic);
                let far_blk = std::mem::take(&mut self.pend_far);
                let (block, obs) =
                    self.process_block(&mic_blk, &far_blk, true, suppress, SuppressW::Live);
                self.record_block(obs);
                out.extend_from_slice(&block);
            }
        }

        while mic.len() - pos >= self.b {
            let (block, obs) = self.process_block(
                &mic[pos..pos + self.b],
                &far[pos..pos + self.b],
                true,
                suppress,
                SuppressW::Live,
            );
            self.record_block(obs);
            out.extend_from_slice(&block);
            pos += self.b;
        }

        self.pend_mic.extend_from_slice(&mic[pos..]);
        self.pend_far.extend_from_slice(&far[pos..]);
        debug_assert!(self.pend_mic.len() < self.b);
    }

    /// Single source of truth for the per-block FDAF math: overlap-save FFT, `Y = X⊙W`, `e = d − y`,
    /// double-talk freeze, one-sided `P` update, constrained leaky-NLMS, and (when `suppress`) the
    /// spectral-subtraction gain. Returns `b` cleaned samples plus the block's [`BlockObservation`] — the
    /// energies and gate decisions this math already computes, so the caller (which alone knows whether the
    /// block is emitted) can put them on the stats timeline. Nothing here branches on the observation.
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
    ) -> (Vec<f32>, BlockObservation) {
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
        // Instrumentation only (#149 phase 3): the echo-estimate and error energies fall out of the same
        // loop; nothing downstream of here reads them.
        let mut y_energy = 0.0f32;
        let mut e_energy = 0.0f32;
        for i in 0..b {
            let y_i = self.yspec[b + i].re * inv_m;
            let e_i = mic_blk[i] - y_i;
            clean[i] = e_i;
            self.espec[i] = Cf::new(0.0, 0.0);
            self.espec[b + i] = Cf::new(e_i, 0.0);
            d_energy += mic_blk[i] * mic_blk[i];
            x_energy += cur[i] * cur[i];
            y_energy += y_i * y_i;
            e_energy += e_i * e_i;
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
            let obs = BlockObservation {
                mic_energy: d_energy,
                far_energy: x_energy,
                echo_estimate_energy: y_energy,
                error_energy: e_energy,
                double_talk,
                suppressed: false,
            };
            return (clean, obs);
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
        // The suppression pass recomputes the echo estimate against W_sup, so these supersede the
        // adaptation-path energies for this block's record.
        let mut sy_energy = 0.0f32;
        let mut se_energy = 0.0f32;
        for i in 0..b {
            let y_i = yspec_td[b + i].re * inv_m;
            let e_i = mic_blk[i] - y_i;
            self.espec[i] = Cf::new(0.0, 0.0);
            self.espec[b + i] = Cf::new(e_i, 0.0);
            sd_energy += mic_blk[i] * mic_blk[i];
            sx_energy += cur[i] * cur[i];
            sy_energy += y_i * y_i;
            se_energy += e_i * e_i;
        }

        if sd_energy > self.cfg.double_talk_ratio * sx_energy {
            for i in 0..b {
                clean[i] = self.espec[b + i].re;
            }
            let obs = BlockObservation {
                mic_energy: sd_energy,
                far_energy: sx_energy,
                echo_estimate_energy: sy_energy,
                error_energy: se_energy,
                double_talk,
                suppressed: false,
            };
            return (clean, obs);
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
        let mut out_energy = 0.0f32;
        for i in 0..b {
            clean[i] = self.espec[b + i].re * inv_m;
            out_energy += clean[i] * clean[i];
        }
        let obs = BlockObservation {
            mic_energy: sd_energy,
            far_energy: sx_energy,
            echo_estimate_energy: sy_energy,
            // What this block emits is the SUPPRESSED signal, not the raw FDAF error.
            error_energy: out_energy,
            double_talk,
            suppressed: true,
        };
        (clean, obs)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AecConfig;
    use crate::tests::{convolve, noise, synthetic_echo_fixture};

    /// A block record with only the fields a summariser test cares about.
    fn stat(t: f64, mic: f32, echo: f32, err: f32, dt: bool, sup: bool) -> BlockStats {
        BlockStats {
            t_start_secs: t,
            mic_energy: mic,
            far_energy: 1.0,
            echo_estimate_energy: echo,
            error_energy: err,
            double_talk: dt,
            suppressed: sup,
        }
    }

    /// Push `mic`/`far` through a `StreamingAec` in one go, returning the audio and every recorded block.
    fn drive_with_stats(
        sr: u32,
        cfg: &AecConfig,
        lookahead: usize,
        mic: &[f32],
        far: &[f32],
    ) -> (Vec<f32>, Vec<BlockStats>) {
        let mut s = StreamingAec::new_with_lookahead(sr, cfg.clone(), lookahead);
        let mut out = s.push(mic, far);
        let mut stats = s.block_stats();
        let fin = s.finish_with_stats();
        out.extend(fin.audio);
        stats.extend(fin.stats);
        (out, stats)
    }

    #[test]
    fn stats_ring_is_bounded_and_counts_drops() {
        // More blocks than the ring holds, with nobody draining: it must keep the newest MAX_BLOCK_STATS and
        // count every eviction, rather than growing with call length (the whole point of a bounded ring).
        let sr = 8_000u32;
        let b = 32usize;
        let blocks = MAX_BLOCK_STATS + 137;
        let n = blocks * b;
        let cfg = AecConfig {
            filter_len: b,
            ..AecConfig::default()
        };
        let mic = noise(n, 0.3, 11);
        let far = noise(n, 0.3, 22);

        // lookahead 0: no warm-up, every block is a steady-state block, so the arithmetic is exact.
        let mut s = StreamingAec::new_with_lookahead(sr, cfg, 0);
        let _ = s.push(&mic, &far);
        assert_eq!(
            s.stats_dropped(),
            (blocks - MAX_BLOCK_STATS) as u64,
            "every evicted block must be counted"
        );
        let drained = s.block_stats();
        assert_eq!(drained.len(), MAX_BLOCK_STATS, "ring must stay bounded");
        assert!(
            s.block_stats().is_empty(),
            "block_stats() drains: a second call must be empty"
        );

        // The retained window is the NEWEST blocks; the hole is at the old end.
        let hop = b as f64 / sr as f64;
        let first_kept = (blocks - MAX_BLOCK_STATS) as f64 * hop;
        assert!(
            (drained[0].t_start_secs - first_kept).abs() < 1e-9,
            "oldest retained block starts at {} s, expected {first_kept} s",
            drained[0].t_start_secs
        );
        assert!(
            (drained[MAX_BLOCK_STATS - 1].t_start_secs - (blocks - 1) as f64 * hop).abs() < 1e-9
        );

        // Draining resets nothing but the ring: the lifetime drop count is a running total.
        assert_eq!(s.stats_dropped(), (blocks - MAX_BLOCK_STATS) as u64);
    }

    #[test]
    fn stats_timeline_matches_the_emitted_audio() {
        // A far-end burst at exactly t = 2.0 s must appear in the record at t = 2.0 s — on the CLEANED
        // timeline, with the lookahead already accounted for. The warm-up convergence sub-pass processes
        // blocks whose output is discarded; if those were recorded the whole record would be shifted by the
        // lookahead and every consumer would read the wrong span.
        let sr = 8_000u32;
        let b = 512usize;
        let hop = b as f64 / sr as f64; // 64 ms
        let secs = 4usize;
        let n = sr as usize * secs;
        let burst_start = 2 * sr as usize;
        let burst_end = burst_start + sr as usize / 2; // [2.0, 2.5) s

        // Far end: quiet broadband noise throughout (so the filter converges during warm-up), 30x louder
        // through the burst.
        let quiet = noise(n, 0.02, 5);
        let loud = noise(n, 0.6, 6);
        let far: Vec<f32> = (0..n)
            .map(|i| {
                if (burst_start..burst_end).contains(&i) {
                    loud[i]
                } else {
                    quiet[i]
                }
            })
            .collect();
        // Mic: pure echo (a 1 ms delay + short decay), no near end, so nothing but the burst moves.
        let mut h = vec![0.0f32; 64];
        for (k, slot) in h.iter_mut().enumerate().skip(8) {
            *slot = 0.3 * (-((k - 8) as f32) / 10.0).exp();
        }
        let mic = convolve(&far, &h);

        let cfg = AecConfig {
            filter_len: b,
            ..AecConfig::default()
        };
        // 1 s of lookahead, rounded up to 16 whole blocks (1.024 s) of warm-up.
        let lookahead = round_up_to_block(sr as usize, b);
        assert_eq!(lookahead, 16 * b);
        let (audio, stats) = drive_with_stats(sr, &cfg, lookahead, &mic, &far);

        // One record per EMITTED block: 62 whole blocks of the 32000 samples plus the zero-padded tail
        // block that `finish` flushes. Not 16 more for the discarded convergence pass.
        assert_eq!(audio.len(), n, "length invariant");
        assert_eq!(stats.len(), n.div_ceil(b), "one record per emitted block");
        assert_eq!(stats[0].t_start_secs, 0.0, "the clock starts at the call");
        for (k, st) in stats.iter().enumerate() {
            assert!(
                (st.t_start_secs - k as f64 * hop).abs() < 1e-9,
                "block {k} starts at {} s, expected {} s",
                st.t_start_secs,
                k as f64 * hop
            );
        }

        // The burst begins inside the block starting at 1.984 s, so the first elevated block must start in
        // (2.0 - hop, 2.0]: one hop of quantization, no lookahead offset.
        let median = |mut v: Vec<f32>| -> f32 {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let onset = |pick: fn(&BlockStats) -> f32| -> f64 {
            let floor = 10.0 * median(stats.iter().map(pick).collect());
            stats
                .iter()
                .find(|s| pick(s) > floor)
                .expect("no block exceeded the quiet floor")
                .t_start_secs
        };
        for (what, t) in [
            ("far_energy", onset(|s| s.far_energy)),
            ("mic_energy", onset(|s| s.mic_energy)),
            ("echo_estimate_energy", onset(|s| s.echo_estimate_energy)),
        ] {
            assert!(
                t > 2.0 - hop && t <= 2.0,
                "{what} rose at {t:.3} s; the burst is at 2.000 s (block hop {hop:.3} s)"
            );
        }
    }

    #[test]
    fn gate_flags_agree_with_the_double_talk_regions() {
        // The crate's double-talk fixture: echo-only outside [1s, 2s), a loud near-end tone inside it. The
        // recorded flags must say exactly that — the gate froze adaptation through the near-end region and
        // nowhere else, and the residual suppressor ran exactly where the gate did not.
        let (mic, far, _near, sr, n) = synthetic_echo_fixture();
        let cfg = AecConfig::default();
        assert!(
            cfg.suppress_residual > 0.0,
            "fixture assumes suppression on"
        );
        let lookahead = round_up_to_block(sr as usize, cfg.filter_len);
        let (audio, stats) = drive_with_stats(sr, &cfg, lookahead, &mic, &far);
        assert_eq!(audio.len(), n);

        let hop = cfg.filter_len as f64 / sr as f64;
        let inside = |st: &BlockStats, lo: f64, hi: f64| {
            st.t_start_secs >= lo && st.t_start_secs + hop <= hi
        };

        let mut near_end_blocks = 0;
        let mut echo_only_blocks = 0;
        for st in &stats {
            // Today both gates are the same `Σd² > ratio·Σx²` test on the same block, so the suppressor is
            // bypassed exactly when adaptation freezes. That coupling IS #107 root cause #1; the flags are
            // recorded separately so this assertion starts failing the day the gates diverge.
            assert_eq!(
                st.suppressed, !st.double_talk,
                "at {:.3} s the suppressor and the adaptation gate disagreed",
                st.t_start_secs
            );

            if inside(st, 1.0, 2.0) {
                near_end_blocks += 1;
                assert!(
                    st.double_talk,
                    "block at {:.3} s is inside the near-end tone but the gate did not fire",
                    st.t_start_secs
                );
                assert!(
                    st.mic_energy > 4.0 * st.echo_estimate_energy,
                    "near-end block at {:.3} s: mic {:.3} vs echo estimate {:.3}",
                    st.t_start_secs,
                    st.mic_energy,
                    st.echo_estimate_energy
                );
            } else if inside(st, 2.0, 3.0) {
                echo_only_blocks += 1;
                assert!(
                    !st.double_talk,
                    "block at {:.3} s is echo-only but the gate fired",
                    st.t_start_secs
                );
                assert!(
                    st.error_energy < st.mic_energy,
                    "echo-only block at {:.3} s was not cancelled",
                    st.t_start_secs
                );
            }
        }
        assert!(near_end_blocks >= 4, "fixture should span several blocks");
        assert!(echo_only_blocks >= 4, "fixture should span several blocks");
    }

    #[test]
    fn recording_stats_does_not_change_the_audio() {
        // Instrumentation only: the emitted samples must not depend on whether anyone reads the ring, or on
        // the ring overflowing. Sample-exact, with a real echo so a broken filter can't pass by no-op.
        let sr = 8_000u32;
        let b = 128usize;
        let n = 40_000usize;
        let cfg = AecConfig {
            filter_len: b,
            ..AecConfig::default()
        };
        let far = noise(n, 0.4, 31);
        let mut h = vec![0.0f32; 32];
        for (k, slot) in h.iter_mut().enumerate().skip(4) {
            *slot = 0.25 * (-((k - 4) as f32) / 8.0).exp();
        }
        let mic = convolve(&far, &h);

        let mut drained = StreamingAec::new_with_lookahead(sr, cfg.clone(), 4 * b);
        let mut a = Vec::new();
        for start in (0..n).step_by(1000) {
            let end = (start + 1000).min(n);
            a.extend(drained.push(&mic[start..end], &far[start..end]));
            let _ = drained.block_stats();
        }
        a.extend(drained.finish());

        let mut ignored = StreamingAec::new_with_lookahead(sr, cfg, 4 * b);
        let mut c = Vec::new();
        for start in (0..n).step_by(1000) {
            let end = (start + 1000).min(n);
            c.extend(ignored.push(&mic[start..end], &far[start..end]));
        }
        c.extend(ignored.finish());

        assert_eq!(a, c, "draining the stats ring changed the emitted audio");
    }

    #[test]
    fn locked_delay_is_none_until_warm_then_reported() {
        let sr = 8_000u32;
        let cfg = AecConfig {
            filter_len: 128,
            ..AecConfig::default()
        };
        let n = 8_000usize;
        let far = noise(n, 0.4, 77);
        let mic: Vec<f32> = (0..n)
            .map(|i| if i >= 40 { far[i - 40] * 0.5 } else { 0.0 })
            .collect();

        let mut s = StreamingAec::new_with_lookahead(sr, cfg, 4_000);
        assert_eq!(s.locked_delay_samples(), None, "not locked while warming");
        let _ = s.push(&mic[..1_000], &far[..1_000]);
        assert_eq!(s.locked_delay_samples(), None, "still warming");
        let _ = s.push(&mic[1_000..], &far[1_000..]);
        let locked = s.locked_delay_samples().expect("locked after warm-up");
        // max_lag_ms defaults to 10 ms = 80 samples at 8 kHz; the true 40-sample lag is inside that window.
        assert_eq!(locked, 40, "delay lock missed the synthetic 40-sample lag");
        assert_eq!(s.finish_with_stats().delay_samples, 40);
    }

    #[test]
    fn span_stats_covers_the_blocks_that_overlap_the_span() {
        let blocks: Vec<BlockStats> = (0..10)
            .map(|k| stat(k as f64 * 0.1, 1.0, 1.0, 1.0, false, true))
            .collect();

        // [0.25, 0.55] starts inside the block at 0.2 and ends inside the block at 0.5 → 0.2..=0.5.
        let s = span_stats(&blocks, 0.25, 0.55).unwrap();
        assert_eq!(s.blocks, 4);

        // Shorter than one hop → the single block containing it.
        assert_eq!(span_stats(&blocks, 0.25, 0.27).unwrap().blocks, 1);
        // Exactly on a boundary: the block starting at 0.3 contains 0.3.
        assert_eq!(span_stats(&blocks, 0.3, 0.3).unwrap().blocks, 1);
        // Reversed spans normalize rather than returning nothing.
        assert_eq!(span_stats(&blocks, 0.55, 0.25).unwrap().blocks, 4);
        // Whole record.
        assert_eq!(span_stats(&blocks, -1.0, 99.0).unwrap().blocks, 10);
        // Entirely before the record, and an empty record.
        assert!(span_stats(&blocks, -5.0, -1.0).is_none());
        assert!(span_stats(&[], 0.0, 1.0).is_none());
    }

    #[test]
    fn span_stats_averages_energy_then_converts_to_db() {
        // Means are taken over ENERGY and converted once: mean(1, 3) = 2 → 3.01 dB. Averaging per-block dB
        // would give 2.39 dB and would be dominated by a single quiet block.
        let blocks = vec![
            stat(0.0, 1.0, 10.0, 0.5, true, false),
            stat(0.1, 3.0, 30.0, 1.5, false, true),
            stat(0.2, 1.0, 10.0, 0.5, false, true),
            stat(0.3, 3.0, 30.0, 1.5, false, true),
        ];
        let s = span_stats(&blocks, 0.0, 0.3).unwrap();
        assert_eq!(s.blocks, 4);
        assert!((s.mean_mic_db - 10.0 * 2.0f32.log10()).abs() < 1e-4);
        assert!((s.mean_echo_estimate_db - 10.0 * 20.0f32.log10()).abs() < 1e-4);
        assert!((s.mean_error_db - 10.0 * 1.0f32.log10()).abs() < 1e-4);
        assert!((s.double_talk_fraction - 0.25).abs() < 1e-6);
        assert!((s.suppressed_fraction - 0.75).abs() < 1e-6);

        // The +3 dB "this mic block was mostly echo" comparison the cleanup pass will make.
        let echo = vec![stat(0.0, 4.0, 3.0, 0.2, false, true)];
        let e = span_stats(&echo, 0.0, 0.0).unwrap();
        assert!(e.mean_mic_db - e.mean_echo_estimate_db < 3.0);

        // Digital silence reports the floor, not -inf.
        let silent = vec![stat(0.0, 0.0, 0.0, 0.0, false, false)];
        let z = span_stats(&silent, 0.0, 0.0).unwrap();
        assert_eq!(z.mean_mic_db, -200.0);
        assert!(z.mean_error_db.is_finite());
    }
}
