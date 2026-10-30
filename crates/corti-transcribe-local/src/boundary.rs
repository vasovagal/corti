//! Isolated #155 ownership prototype. Opt-in only; not a production capacity/accuracy certificate.

use super::*;
use corti_transcribe::boundary::SourceInterval;

const ACCEPTED_GUARD: u64 = 5727;
/// A reset VAD needs context before it reports speech; replay this much already-owned audio before a
/// forced cut so speech just after the cut is in a region. Replay also starts on the window grid,
/// so the reset model sees the same windows as an uninterrupted run. Attempted audio is never
/// decoded again.
const FORCED_REPLAY_PREROLL: u64 = 16_000;
/// Largest replay beyond settlement: the forced pre-roll plus grid alignment.
const REPLAY_CONTEXT: u64 = FORCED_REPLAY_PREROLL + VAD_WINDOW as u64 - 1;

/// Exact pinned filter-count terms, not a total memory limit. Source packet staging is separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundaryResamplerBounds {
    source_rate: u32,
    tick_frequency: u32,
    filter_source_lag: u64,
    filter_canonical_lag: u64,
    native_remainder_samples: u64,
}

/// Conservative fixed-rate rolling-owner headroom; all native counts are canonical16k samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundaryCapacity {
    pub settlement_source_reserve: u64,
    pub settlement_accept: u64,
    pub replay_samples: u64,
    pub next_accept: u64,
    pub maintenance_trigger: u64,
    pub minimum_epoch_samples: u64,
    pub retained_samples: usize,
}

/// Mirrors the pinned Rust f32 cutoff and C++ GetNumOutputSamples rounding; checks native i32 LCM.
#[doc(hidden)]
pub fn boundary_resampler_bounds(rate: u32) -> Result<BoundaryResamplerBounds, BoundaryError> {
    let ticks =
        crate::input_clock::boundary_tick_frequency(rate).ok_or(BoundaryError::UnsupportedRate)?;
    let cutoff = (0.99_f32 * 0.5) * rate.min(16_000) as f32;
    let width = (6.0_f64 / (2.0 * f64::from(cutoff))) as f32;
    let lag_ticks = (width * ticks as f32).floor() as u64;
    Ok(BoundaryResamplerBounds {
        source_rate: rate,
        tick_frequency: ticks,
        filter_source_lag: lag_ticks.div_ceil(u64::from(ticks / rate)),
        filter_canonical_lag: lag_ticks.div_ceil(u64::from(ticks / 16_000)),
        native_remainder_samples: (((rate * 6) as f32 / cutoff).ceil()) as u64,
    })
}

impl BoundaryResamplerBounds {
    pub fn tick_frequency(self) -> u32 {
        self.tick_frequency
    }

    /// Native SetRemainder temporarily holds this many source samples twice while copying history.
    pub fn native_remainder_samples(self) -> u64 {
        self.native_remainder_samples
    }

    /// L is actual AEC source lag, B is received admission, M is the maximum post-AEC source push,
    /// and W is maximum owned source-window length (nominal plus allowed deferral). Fixed rate only.
    /// The caller must fence at current received before future ASR, enforce the source reserve, and
    /// stop admitting once both tracks settle. This is not a bound on model/text/native vector bytes.
    pub fn rolling_capacity(
        self,
        aec_lag: u64,
        received_admission: u64,
        delivered_push: u64,
        max_owned_source_frames: u64,
    ) -> Result<BoundaryCapacity, BoundaryError> {
        if received_admission == 0 || delivered_push == 0 || max_owned_source_frames == 0 {
            return Err(BoundaryError::AdmissionLimit);
        }
        let reserve = self.settlement_source_reserve(aec_lag, received_admission)?;
        let pending_and_future = aec_lag
            .checked_add(reserve)
            .ok_or(BoundaryError::RetentionLimit)?;
        let settlement = self
            .max_push_output(pending_and_future)?
            .checked_add(self.max_finish_output())
            .and_then(|n| n.checked_add(511))
            .ok_or(BoundaryError::RetentionLimit)?;
        let next = self
            .max_push_output(delivered_push)?
            .checked_add(511)
            .ok_or(BoundaryError::RetentionLimit)?;
        let owned = u64::try_from(
            (u128::from(max_owned_source_frames) * 16_000).div_ceil(u128::from(self.source_rate)),
        )
        .map_err(|_| BoundaryError::EpochLimit)?;
        // Arming does not move the ordinary due/deadline: reserve one entire owned window Q,
        // settlement S and the maximum next admission K. After replay (<=S+C, C the replay
        // context), an epoch of Q+2S+K+C+1 can always leave this threshold before a later arm.
        let trigger = owned
            .checked_add(settlement)
            .and_then(|n| n.checked_add(next))
            .ok_or(BoundaryError::RetentionLimit)?;
        let minimum_epoch = settlement
            .checked_add(REPLAY_CONTEXT)
            .and_then(|n| n.checked_add(trigger))
            .and_then(|n| n.checked_add(1))
            .ok_or(BoundaryError::EpochLimit)?;
        if minimum_epoch > i32::MAX as u64 - VAD_WINDOW as u64 {
            return Err(BoundaryError::EpochLimit);
        }
        let retained = u128::from(trigger)
            .checked_add(u128::from(ACCEPTED_GUARD + 511 + REPLAY_CONTEXT))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(BoundaryError::RetentionLimit)?;
        Ok(BoundaryCapacity {
            settlement_source_reserve: reserve,
            settlement_accept: settlement,
            replay_samples: settlement + REPLAY_CONTEXT,
            next_accept: next,
            maintenance_trigger: trigger,
            minimum_epoch_samples: minimum_epoch,
            retained_samples: retained,
        })
    }
    /// Fixed-rate source headroom beyond the immutable fence: AEC lag, native guard, staging,
    /// filter delay and at most one received admission. Does not count arbitrary changing-rate epochs.
    pub fn settlement_source_reserve(
        self,
        aec_lag: u64,
        source_admission: u64,
    ) -> Result<u64, BoundaryError> {
        let guard = (u128::from(ACCEPTED_GUARD + VAD_WINDOW as u64 - 1)
            * u128::from(self.source_rate))
        .div_ceil(16_000);
        u64::try_from(guard)
            .ok()
            .and_then(|n| n.checked_add(511))
            .and_then(|n| n.checked_add(self.filter_source_lag))
            .and_then(|n| n.checked_add(aec_lag))
            .and_then(|n| n.checked_add(source_admission))
            .ok_or(BoundaryError::RetentionLimit)
    }

    /// New canonical output of one ordinary wrapper push; previous source packet phase contributes511.
    pub fn max_push_output(self, delivered_source_push: u64) -> Result<u64, BoundaryError> {
        let count = u128::from(delivered_source_push)
            .checked_add(511)
            .and_then(|n| n.checked_mul(16_000))
            .ok_or(BoundaryError::RetentionLimit)?;
        u64::try_from(count.div_ceil(u128::from(self.source_rate)))
            .map_err(|_| BoundaryError::RetentionLimit)
    }

    pub fn max_finish_output(self) -> u64 {
        (511 * 16_000_u64).div_ceil(u64::from(self.source_rate)) + self.filter_canonical_lag
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundaryLimits {
    pub max_retained_samples: usize,
    pub max_epoch_samples: u64,
    pub max_source_push: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryError {
    NotEnabled,
    InvalidTransition,
    MappingUnavailable,
    AlreadyAttempted,
    RetentionLimit,
    EpochLimit,
    AdmissionLimit,
    UnsupportedRate,
    ResamplerUnavailable,
    InsufficientLookahead,
    UnprovenEofSuffix,
}

impl std::fmt::Display for BoundaryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("live boundary could not be completed")
    }
}

impl std::error::Error for BoundaryError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryKind {
    Natural,
    /// The same natural source cut, with a certified VAD epoch reset after old-prefix retirement.
    NaturalMaintenance,
    Forced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryInput {
    Continue,
    SourceEnded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundaryToken {
    generation: u64,
    pub source_frame: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundarySnapshot {
    /// Track-local source coordinates; the app adds its established track origin.
    pub natural: Option<SourceInterval>,
    pub delivered_through: u64,
    pub epoch_remaining: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryProgress {
    Waiting,
    OldSettled(BoundaryToken),
    TerminalUnsettled(BoundaryError),
    Ended,
}

struct Fence {
    token: BoundaryToken,
    kind: BoundaryKind,
    canonical: Option<u64>,
    settled: bool,
    natural_retire: u64,
}

pub(super) struct BoundaryState {
    pub(super) limits: BoundaryLimits,
    pub(super) failed: Option<BoundaryError>,
    pub(super) last_attempt_end: u64,
    /// End of already-attempted audio replayed only as VAD context after the last reset. A region
    /// starting inside it is decoded from this point; any other overlap still fails closed.
    pub(super) replay_context_end: u64,
    pub(super) last_attempt_source_end: Option<u64>,
    pub(super) proof_available: bool,
    retained: Vec<f32>,
    retained_start: u64,
    accepted_end: u64,
    pub(super) native_accepted: u64,
    owned_source_start: u64,
    generation: u64,
    fence: Option<Fence>,
    input_ended: bool,
    terminal_flushed: bool,
}

impl BoundaryState {
    pub(super) fn allowed_end(
        &mut self,
        clock: &InputClock,
        start: u64,
        end: u64,
    ) -> Result<u64, BoundaryError> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let Some(fence) = &mut self.fence else {
            return Ok(end);
        };
        if fence.settled {
            return Ok(start);
        }
        if fence.canonical.is_none()
            && clock
                .mapped_input_end()
                .is_some_and(|end| fence.token.source_frame <= end)
        {
            fence.canonical = Some(
                clock
                    .canonical_fence(fence.token.source_frame)
                    .map_err(|_| BoundaryError::MappingUnavailable)?,
            );
        }
        Ok(fence.canonical.map_or(end, |cut| end.min(cut)))
    }

    fn check_accept(&self, count: usize) -> Result<(), BoundaryError> {
        if self
            .native_accepted
            .checked_add(count as u64)
            .is_none_or(|end| end > self.limits.max_epoch_samples)
        {
            return Err(BoundaryError::EpochLimit);
        }
        Ok(())
    }
}

impl LiveTranscriber {
    /// Experimental integration seam. Must be enabled before any source input.
    #[doc(hidden)]
    pub fn enable_boundaries(&mut self, limits: BoundaryLimits) -> Result<(), BoundaryError> {
        if self.boundary.is_some()
            || self.input_clock.frames != 0
            || self.finished
            || limits.max_retained_samples == 0
            || limits.max_source_push == 0
            || limits.max_epoch_samples == 0
            || limits.max_epoch_samples > i32::MAX as u64 - VAD_WINDOW as u64
        {
            return Err(BoundaryError::InvalidTransition);
        }
        let mut retained = Vec::new();
        retained
            .try_reserve_exact(limits.max_retained_samples)
            .map_err(|_| BoundaryError::RetentionLimit)?;
        self.boundary = Some(BoundaryState {
            limits,
            failed: None,
            last_attempt_end: 0,
            replay_context_end: 0,
            last_attempt_source_end: None,
            proof_available: true,
            retained,
            retained_start: 0,
            accepted_end: 0,
            native_accepted: 0,
            owned_source_start: 0,
            generation: 0,
            fence: None,
            input_ended: false,
            terminal_flushed: false,
        });
        Ok(())
    }

    #[doc(hidden)]
    pub fn boundary_snapshot(&self) -> Option<BoundarySnapshot> {
        let state = self.boundary.as_ref()?;
        let natural = (|| {
            if state.failed.is_some()
                || state.fence.is_some()
                || !state.proof_available
                || !self.clock_available
                || !self.vad.boundary_idle()
            {
                return None;
            }
            let future = state.accepted_end.checked_sub(ACCEPTED_GUARD)?;
            if state.last_attempt_end > future || future < state.retained_start {
                return None;
            }
            let first = state
                .owned_source_start
                .max(state.last_attempt_source_end.unwrap_or(0));
            let last = self
                .input_clock
                .input_span(future, future.checked_add(1)?)
                .ok()?
                .start_frame
                .min(self.input_clock.frames);
            (first <= last).then_some(SourceInterval { first, last })
        })();
        Some(BoundarySnapshot {
            natural,
            delivered_through: self.input_clock.frames,
            epoch_remaining: state
                .limits
                .max_epoch_samples
                .checked_sub(state.native_accepted)?,
        })
    }

    #[doc(hidden)]
    pub fn latch_boundary(
        &mut self,
        source_frame: u64,
        kind: BoundaryKind,
    ) -> Result<BoundaryToken, BoundaryError> {
        let natural = if kind != BoundaryKind::Forced {
            Some(
                self.boundary_snapshot()
                    .and_then(|snapshot| snapshot.natural)
                    .ok_or(BoundaryError::MappingUnavailable)?,
            )
        } else {
            None
        };
        let state = self.boundary.as_mut().ok_or(BoundaryError::NotEnabled)?;
        if state.fence.is_some()
            || state.failed.is_some()
            || state.input_ended
            || source_frame < state.owned_source_start
        {
            return Err(BoundaryError::InvalidTransition);
        }
        if let Some(snapshot) = natural
            && !(snapshot.first..=snapshot.last).contains(&source_frame)
        {
            return Err(BoundaryError::MappingUnavailable);
        }
        let fresh_empty = source_frame == 0
            && self.input_clock.frames == 0
            && state.last_attempt_end == 0
            && state.accepted_end == 0;
        let canonical = if fresh_empty {
            Some(0)
        } else if source_frame <= self.input_clock.frames {
            Some(
                self.input_clock
                    .canonical_fence(source_frame)
                    .map_err(|_| BoundaryError::MappingUnavailable)?,
            )
        } else {
            None
        };
        if canonical.is_some_and(|cut| {
            cut < state.last_attempt_end
                || (kind == BoundaryKind::Forced && cut < state.retained_start)
        }) {
            return Err(BoundaryError::AlreadyAttempted);
        }
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or(BoundaryError::InvalidTransition)?;
        let token = BoundaryToken {
            generation: state.generation,
            source_frame,
        };
        state.fence = Some(Fence {
            token,
            kind,
            canonical,
            settled: kind != BoundaryKind::Forced || fresh_empty,
            natural_retire: state.accepted_end.saturating_sub(ACCEPTED_GUARD),
        });
        Ok(token)
    }

    #[doc(hidden)]
    pub fn advance_boundary(&mut self, input: BoundaryInput) -> BoundaryProgress {
        let Some(state) = self.boundary.as_ref() else {
            return BoundaryProgress::TerminalUnsettled(BoundaryError::NotEnabled);
        };
        if let Some(error) = state.failed {
            return BoundaryProgress::TerminalUnsettled(error);
        }
        // A natural fence is already certified. Preserve external/resampler tails until the old
        // prefix is retired: a no-reset acknowledgement cannot recover next regions popped here.
        if let Some(fence) = &state.fence
            && fence.kind != BoundaryKind::Forced
            && fence.settled
        {
            return BoundaryProgress::OldSettled(fence.token);
        }
        if input == BoundaryInput::SourceEnded && !state.input_ended {
            self.flush_resampler_tail();
            self.accept_boundary_remainder();
            self.boundary.as_mut().unwrap().input_ended = true;
            self.finished = true;
        }
        let state = self.boundary.as_mut().unwrap();
        if let Some(error) = state.failed {
            return BoundaryProgress::TerminalUnsettled(error);
        }
        if let Some(fence) = &mut state.fence {
            if fence.settled {
                return BoundaryProgress::OldSettled(fence.token);
            }
            if fence.canonical.is_none()
                && self
                    .input_clock
                    .mapped_input_end()
                    .is_some_and(|end| fence.token.source_frame <= end)
            {
                match self.input_clock.canonical_fence(fence.token.source_frame) {
                    Ok(cut) => fence.canonical = Some(cut),
                    Err(_) => {
                        state.failed = Some(BoundaryError::MappingUnavailable);
                        return BoundaryProgress::TerminalUnsettled(
                            BoundaryError::MappingUnavailable,
                        );
                    }
                }
            }
            let ready = fence
                .canonical
                .and_then(|cut| cut.checked_add(ACCEPTED_GUARD))
                .is_some_and(|needed| state.accepted_end >= needed);
            if !ready {
                return if state.input_ended {
                    BoundaryProgress::TerminalUnsettled(BoundaryError::InsufficientLookahead)
                } else {
                    BoundaryProgress::Waiting
                };
            }
            self.vad.flush();
            self.drain_regions();
            let state = self.boundary.as_mut().unwrap();
            if let Some(error) = state.failed {
                return BoundaryProgress::TerminalUnsettled(error);
            }
            let fence = state.fence.as_mut().unwrap();
            fence.settled = true;
            return BoundaryProgress::OldSettled(fence.token);
        }
        if state.input_ended {
            if !state.terminal_flushed {
                state.terminal_flushed = true;
                self.finalize_terminal_vad();
            }
            match self.boundary.as_ref().unwrap().failed {
                Some(error) => BoundaryProgress::TerminalUnsettled(error),
                None => BoundaryProgress::Ended,
            }
        } else {
            BoundaryProgress::Waiting
        }
    }

    #[doc(hidden)]
    pub fn acknowledge_boundary(&mut self, token: BoundaryToken) -> Result<(), BoundaryError> {
        if !self.pending.is_empty() {
            return Err(BoundaryError::InvalidTransition);
        }
        let state = self.boundary.as_mut().ok_or(BoundaryError::NotEnabled)?;
        if state.failed.is_some() {
            return Err(BoundaryError::InvalidTransition);
        }
        let fence = state
            .fence
            .as_ref()
            .ok_or(BoundaryError::InvalidTransition)?;
        if fence.token != token || !fence.settled {
            return Err(BoundaryError::InvalidTransition);
        }
        let reset = fence.kind != BoundaryKind::Natural;
        let owned_start = if fence.kind == BoundaryKind::Forced {
            fence.canonical.ok_or(BoundaryError::MappingUnavailable)?
        } else {
            // The historical source cut C remains fixed; natural maintenance restarts at the
            // certified future-start bound U=T-5727, never the whole historical silence after C.
            // This proves ownership/retention, not model-inference equivalence after Reset.
            fence.natural_retire
        };
        if owned_start < state.retained_start
            || owned_start < state.last_attempt_end
            || owned_start > state.accepted_end
        {
            return Err(BoundaryError::AlreadyAttempted);
        }
        let replay_start = if reset {
            let preroll = if fence.kind == BoundaryKind::Forced {
                FORCED_REPLAY_PREROLL
            } else {
                0
            };
            let window = VAD_WINDOW as u64;
            (owned_start.saturating_sub(preroll) / window * window).max(state.retained_start)
        } else {
            owned_start
        };
        let remove = usize::try_from(replay_start - state.retained_start)
            .map_err(|_| BoundaryError::RetentionLimit)?;
        let accepted_len = usize::try_from(state.accepted_end - replay_start)
            .map_err(|_| BoundaryError::RetentionLimit)?;
        if remove
            .checked_add(accepted_len)
            .is_none_or(|end| end > state.retained.len())
        {
            return Err(BoundaryError::RetentionLimit);
        }
        let mut replay = Vec::new();
        if reset {
            replay
                .try_reserve_exact(accepted_len)
                .map_err(|_| BoundaryError::RetentionLimit)?;
            replay.extend_from_slice(&state.retained[remove..remove + accepted_len]);
            if replay.len() as u64 > state.limits.max_epoch_samples {
                return Err(BoundaryError::EpochLimit);
            }
        }
        state.retained.drain(..remove);
        state.retained_start = replay_start;
        state.owned_source_start = token.source_frame;
        state.fence = None;
        if reset {
            self.vad.reset();
            self.terminal.reset_epoch();
            self.vad_base_samples = replay_start;
            let state = self.boundary.as_mut().unwrap();
            state.replay_context_end = state.last_attempt_end;
            state.native_accepted = 0;
            state.accepted_end = replay_start;
            for chunk in replay.chunks(VAD_WINDOW) {
                self.accept_boundary_pcm(chunk);
                self.drain_regions();
                if let Some(error) = self.boundary.as_ref().unwrap().failed {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn fail_boundary(&mut self, error: BoundaryError) {
        self.boundary.as_mut().unwrap().failed = Some(error);
        self.report.omit_failure();
        self.decode_complete = false;
    }

    pub(super) fn feed_boundary_16k(&mut self, pcm: &[f32]) {
        let position = self.output_position();
        let state = self.boundary.as_mut().unwrap();
        if state.failed.is_some() {
            return;
        }
        if state
            .retained_start
            .checked_add(state.retained.len() as u64)
            != Some(position)
            || state
                .retained
                .len()
                .checked_add(pcm.len())
                .is_none_or(|n| n > state.limits.max_retained_samples)
            || state.retained.try_reserve_exact(pcm.len()).is_err()
        {
            self.fail_boundary(BoundaryError::RetentionLimit);
            return;
        }
        let complete = (self.win.buf.len() + pcm.len()) / VAD_WINDOW * VAD_WINDOW;
        if let Err(error) = state.check_accept(complete) {
            self.fail_boundary(error);
            return;
        }
        state.retained.extend_from_slice(pcm);
        self.win.extend(pcm);
        let windows = self.win.take_windows();
        for window in windows.chunks(VAD_WINDOW) {
            self.accept_boundary_pcm(window);
            self.drain_regions();
            if self.boundary.as_ref().unwrap().failed.is_some() {
                return;
            }
        }
    }

    fn accept_boundary_pcm(&mut self, pcm: &[f32]) {
        let state = self.boundary.as_mut().unwrap();
        if let Err(error) = state.check_accept(pcm.len()) {
            self.fail_boundary(error);
            return;
        }
        let Some(end) = state.accepted_end.checked_add(pcm.len() as u64) else {
            self.fail_boundary(BoundaryError::MappingUnavailable);
            return;
        };
        state.native_accepted += pcm.len() as u64;
        state.accepted_end = end;
        self.accept_native_pcm(pcm);
    }

    pub(super) fn accept_boundary_remainder(&mut self) {
        if self.boundary.as_ref().unwrap().failed.is_some() {
            return;
        }
        let remainder = self.win.take_remainder();
        if !remainder.is_empty() {
            self.accept_boundary_pcm(&remainder);
            self.drain_regions();
        }
    }
}

#[cfg(test)]
#[path = "boundary_tests.rs"]
mod tests;
