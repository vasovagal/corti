//! Terminal processing certificate for the pinned fresh Silero detector, not an accuracy claim.
use super::*;

pub(super) const PINNED_GUARD: usize = 575 + 2 * 576 + 4000;
const SIGNED_LIMIT: u64 = i32::MAX as u64 - 576;

pub(super) struct TerminalState {
    pub(super) guard: Option<usize>,
    pub(super) native_accepted: u64,
    pub(super) last_attempt_end: u64,
    pub(super) clip_end: Option<u64>,
    pub(super) ownership_valid: bool,
    pub(super) certified_end: Option<u64>,
    pub(super) padding: usize,
    finalized: bool,
    reset_tail: Vec<f32>,
    reset_tail_start: u64,
}

impl Default for TerminalState {
    fn default() -> Self {
        Self {
            guard: None,
            native_accepted: 0,
            last_attempt_end: 0,
            clip_end: None,
            ownership_valid: true,
            certified_end: None,
            padding: 0,
            finalized: false,
            reset_tail: Vec::new(),
            reset_tail_start: 0,
        }
    }
}

impl TerminalState {
    pub(super) fn new(guard: Option<usize>) -> Self {
        Self {
            guard,
            ..Self::default()
        }
    }

    pub(super) fn reset_epoch(&mut self) {
        self.native_accepted = 0;
        self.reset_tail.clear();
    }
}

impl LiveTranscriber {
    /// Count every native Accept, including ordinary/batch real input and retained replay.
    pub(super) fn accept_native_pcm(&mut self, pcm: &[f32]) {
        let next = self.terminal.native_accepted.checked_add(pcm.len() as u64);
        if next.is_none_or(|next| next > SIGNED_LIMIT) {
            self.terminal.ownership_valid = false;
            self.report.omit_failure();
            self.decode_complete = false;
            if let Some(state) = &mut self.boundary {
                state.failed = Some(BoundaryError::EpochLimit);
            }
            return;
        }
        self.terminal.native_accepted = next.unwrap();
        self.vad.accept(pcm);
    }

    /// Bounded evidence only for destructive legacy resets, which cannot use terminal-only padding.
    pub(super) fn retain_reset_tail(&mut self, pcm: &[f32]) {
        let position = self.output_position();
        let tail = &mut self.terminal;
        if tail
            .reset_tail
            .try_reserve_exact(PINNED_GUARD.saturating_sub(tail.reset_tail.len()))
            .is_err()
        {
            self.report.omit_failure();
            self.decode_complete = false;
            return;
        }
        if tail
            .reset_tail_start
            .checked_add(tail.reset_tail.len() as u64)
            != Some(position)
        {
            tail.reset_tail.clear();
            tail.reset_tail_start = position;
        }
        if pcm.len() >= PINNED_GUARD {
            tail.reset_tail.clear();
            tail.reset_tail
                .extend_from_slice(&pcm[pcm.len() - PINNED_GUARD..]);
            tail.reset_tail_start = position + (pcm.len() - PINNED_GUARD) as u64;
        } else {
            let remove = (tail.reset_tail.len() + pcm.len()).saturating_sub(PINNED_GUARD);
            tail.reset_tail.drain(..remove);
            tail.reset_tail_start += remove as u64;
            tail.reset_tail.extend_from_slice(pcm);
        }
    }

    fn record_unproven(&mut self, start: u64, end: u64, error: BoundaryError) {
        match self.input_clock.input_span(start, end) {
            Ok(span) if start < end => self.report.record(RegionOutcome {
                span,
                status: RegionStatus::Partial(DecodeErrorCategory::OutputTruncated),
            }),
            _ => self.report.omit_failure(),
        }
        self.decode_complete = false;
        if let Some(state) = &mut self.boundary {
            state.failed = Some(error);
        }
    }

    pub(super) fn preserve_reset_uncertainty(&mut self) {
        let real_end = self.output_position();
        let tail = &self.terminal;
        if tail.guard == Some(0) || tail.last_attempt_end >= real_end {
            return;
        }
        if tail.guard.is_none() {
            self.record_unproven(
                tail.last_attempt_end.max(self.vad_base_samples),
                real_end,
                BoundaryError::UnprovenEofSuffix,
            );
            return;
        }
        let start = tail.last_attempt_end.max(tail.reset_tail_start);
        let offset = (start - tail.reset_tail_start) as usize;
        if let Some(pcm) = tail.reset_tail.get(offset..)
            && let Some(first) = pcm.iter().position(|value| *value != 0.0)
            && let Some(last) = pcm.iter().rposition(|value| *value != 0.0)
        {
            self.record_unproven(
                start + first as u64,
                start + last as u64 + 1,
                BoundaryError::UnprovenEofSuffix,
            );
        }
    }

    pub(super) fn refuse_compatibility_finish(&mut self, error: BoundaryError) {
        if !self.terminal.finalized {
            self.terminal.finalized = true;
            self.record_unproven(
                self.terminal.last_attempt_end.max(self.vad_base_samples),
                self.output_position(),
                error,
            );
        }
        self.finished = true;
    }

    pub(super) fn finalize_terminal_vad(&mut self) {
        if self.terminal.finalized {
            return;
        }
        self.terminal.finalized = true;
        let real_end = self.output_position();
        let native = self.terminal.native_accepted;
        let reconciled = self.vad_base_samples.checked_add(native) == Some(real_end)
            && self.win.buf.is_empty()
            && self.clock_available
            && self.terminal.ownership_valid
            && (native == 0
                || self.input_clock.input_span(self.vad_base_samples, real_end)
                    != Err(MappingError::Invalid))
            && self
                .boundary
                .as_ref()
                .is_none_or(|state| state.native_accepted == native);
        let limit = self.boundary.as_ref().map_or(SIGNED_LIMIT, |state| {
            state.limits.max_epoch_samples.min(SIGNED_LIMIT)
        });
        let reason = match self.terminal.guard {
            None => Some(BoundaryError::UnprovenEofSuffix),
            Some(guard) if guard > PINNED_GUARD => Some(BoundaryError::UnprovenEofSuffix),
            Some(_) if !reconciled => Some(BoundaryError::MappingUnavailable),
            Some(guard)
                if native
                    .checked_add(guard as u64)
                    .is_none_or(|end| end > limit) =>
            {
                Some(BoundaryError::EpochLimit)
            }
            Some(_) => None,
        };
        if let Some(reason) = reason {
            self.vad.flush();
            self.drain_regions();
            if !self.terminal.ownership_valid || !self.clock_available {
                // The failing admission/mapping already recorded this loss; do not fabricate a
                // second source span or conceal it by granting a current-epoch certificate.
                if let Some(state) = &mut self.boundary {
                    state.failed = Some(reason);
                }
                return;
            }
            self.record_unproven(
                self.terminal.last_attempt_end.max(self.vad_base_samples),
                real_end,
                reason,
            );
            return;
        }
        if native == 0 {
            self.terminal.certified_end = Some(real_end);
            return;
        }

        // R stays immutable. P>=B+A-575 and future starts>=P-5152. With A>=R-B+5727,
        // all real samples have been processed and every future new region starts at/after R.
        self.terminal.clip_end = Some(real_end);
        let guard = self.terminal.guard.unwrap();
        for chunk in [0.0; PINNED_GUARD][..guard].chunks(VAD_WINDOW) {
            if let Some(state) = &mut self.boundary {
                state.native_accepted += chunk.len() as u64;
            }
            self.terminal.padding += chunk.len();
            self.accept_native_pcm(chunk);
            self.drain_regions();
        }
        self.vad.flush();
        self.drain_regions();
        let future_start = self
            .vad_base_samples
            .checked_add(self.terminal.native_accepted)
            .and_then(|end| end.checked_sub(guard as u64));
        if self.terminal.ownership_valid && future_start.is_some_and(|start| start >= real_end) {
            self.terminal.certified_end = Some(real_end);
        } else if !self.terminal.ownership_valid {
            if let Some(state) = &mut self.boundary {
                state.failed = Some(BoundaryError::MappingUnavailable);
            }
        } else {
            self.record_unproven(
                self.terminal.last_attempt_end.max(self.vad_base_samples),
                real_end,
                BoundaryError::UnprovenEofSuffix,
            );
        }
    }
}
