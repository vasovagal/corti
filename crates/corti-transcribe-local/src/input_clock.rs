//! Bounded mapping from native 16 kHz regions back to the channel's original input frames.

use std::collections::VecDeque;

use corti_transcribe::decode::InputSpan;

const MAX_RATE_EPOCHS: usize = 256;

struct RateEpoch {
    input_start: u64,
    input_end: u64,
    output_start: u64,
    rate: u32,
}

impl RateEpoch {
    fn output_end(&self) -> Option<u64> {
        let count = u128::from(self.input_end.checked_sub(self.input_start)?) * 16_000;
        let count = count.div_ceil(u128::from(self.rate));
        self.output_start.checked_add(u64::try_from(count).ok()?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MappingError {
    EvictedHistory,
    Invalid,
}

struct EvictedPrefix {
    output_start: u64,
    output_end: u64,
    input_end: u64,
}

#[derive(Default)]
pub(crate) struct InputClock {
    pub(crate) frames: u64,
    epochs: VecDeque<RateEpoch>,
    extend_epoch: bool,
    overflowed: bool,
    // Fixed-size proof of contiguous coverage; detailed rate/source mappings remain capped.
    evicted: Option<EvictedPrefix>,
    evicted_unextendable: bool,
}

impl InputClock {
    pub(crate) fn admit(&mut self, frames: usize) -> Option<InputSpan> {
        if self.overflowed {
            return None;
        }
        let start_frame = self.frames;
        let Some(end) = u64::try_from(frames)
            .ok()
            .and_then(|frames| self.frames.checked_add(frames))
        else {
            self.overflowed = true;
            self.reset_vad();
            return None;
        };
        self.frames = end;
        Some(InputSpan {
            start_frame,
            end_frame: self.frames,
        })
    }

    /// Returns true exactly when one detailed rate epoch is removed by the cap.
    pub(crate) fn map_input(&mut self, span: InputSpan, rate: u32, output_start: u64) -> bool {
        if rate == 0 {
            self.extend_epoch = false;
            return false;
        }
        if self.extend_epoch
            && let Some(epoch) = self.epochs.back_mut()
            && epoch.rate == rate
            && epoch.input_end == span.start_frame
        {
            epoch.input_end = span.end_frame;
            return false;
        }
        let removed = self.epochs.len() == MAX_RATE_EPOCHS;
        if removed && let Some(epoch) = self.epochs.pop_front() {
            self.remember_evicted(epoch);
        }
        self.epochs.push_back(RateEpoch {
            input_start: span.start_frame,
            input_end: span.end_frame,
            output_start,
            rate,
        });
        self.extend_epoch = true;
        removed
    }

    fn remember_evicted(&mut self, epoch: RateEpoch) {
        if self.evicted_unextendable {
            return;
        }
        let Some(output_end) = epoch
            .output_end()
            .filter(|_| epoch.input_end <= self.frames)
        else {
            self.evicted_unextendable = true;
            return;
        };
        if epoch.input_start == epoch.input_end {
            return;
        }
        if let Some(prefix) = &mut self.evicted {
            if prefix.output_end != epoch.output_start || prefix.input_end != epoch.input_start {
                // A forgotten hole can never be reclassified as harmless metadata loss.
                self.evicted_unextendable = true;
                return;
            }
            prefix.output_end = output_end;
            prefix.input_end = epoch.input_end;
        } else {
            self.evicted = Some(EvictedPrefix {
                output_start: epoch.output_start,
                output_end,
                input_end: epoch.input_end,
            });
        }
    }

    pub(crate) fn end_epoch(&mut self) {
        self.extend_epoch = false;
    }

    pub(crate) fn reset_vad(&mut self) {
        self.epochs.clear();
        self.extend_epoch = false;
        self.evicted = None;
        self.evicted_unextendable = false;
    }

    /// Start rounds down, end rounds up; a native flush's ceil count clamps to actual input.
    /// Known evicted coverage loses source metadata, not canonical timing. Every remaining sample
    /// must still be covered; a hole, arithmetic error or out-of-range end is invalid even after eviction.
    pub(crate) fn input_span(&self, start: u64, end: u64) -> Result<InputSpan, MappingError> {
        if start >= end || self.overflowed {
            return Err(MappingError::Invalid);
        }
        let mut cursor = start;
        let mut input_start = None;
        let mut previous_input_end = None;
        let mut evicted = false;
        if let Some(prefix) = &self.evicted
            && prefix.output_start <= cursor
            && cursor < prefix.output_end
        {
            cursor = end.min(prefix.output_end);
            evicted = true;
            if cursor == end {
                return Err(MappingError::EvictedHistory);
            }
            previous_input_end = Some(prefix.input_end);
        }
        for epoch in &self.epochs {
            let output_end = epoch.output_end().ok_or(MappingError::Invalid)?;
            if output_end <= cursor {
                continue;
            }
            if epoch.output_start > cursor || epoch.input_end > self.frames {
                return Err(MappingError::Invalid);
            }
            let piece_end = end.min(output_end);
            let relative_start = u128::from(cursor - epoch.output_start) * u128::from(epoch.rate);
            let relative_end = u128::from(piece_end - epoch.output_start) * u128::from(epoch.rate);
            let first = epoch
                .input_start
                .checked_add(
                    u64::try_from(relative_start / 16_000).map_err(|_| MappingError::Invalid)?,
                )
                .ok_or(MappingError::Invalid)?
                .min(epoch.input_end);
            let last = epoch
                .input_start
                .checked_add(
                    u64::try_from(relative_end.div_ceil(16_000))
                        .map_err(|_| MappingError::Invalid)?,
                )
                .ok_or(MappingError::Invalid)?
                .min(epoch.input_end);
            if previous_input_end.is_some_and(|previous| previous != first) {
                return Err(MappingError::Invalid);
            }
            previous_input_end = Some(last);
            input_start.get_or_insert(first);
            cursor = piece_end;
            if cursor == end {
                if evicted {
                    return Err(MappingError::EvictedHistory);
                }
                let start_frame = input_start.ok_or(MappingError::Invalid)?;
                return (start_frame < last)
                    .then_some(InputSpan {
                        start_frame,
                        end_frame: last,
                    })
                    .ok_or(MappingError::Invalid);
            }
        }
        Err(MappingError::Invalid)
    }
}

/// Exact fractional carry for skipped input. No per-push rounding, and checkpoint does not reset it.
/// The reduced rational is bounded in size; unrepresentable rate combinations fail closed.
pub(crate) struct SkippedClock {
    numerator: u128,
    denominator: u128,
}

impl Default for SkippedClock {
    fn default() -> Self {
        Self {
            numerator: 0,
            denominator: 1,
        }
    }
}

impl SkippedClock {
    pub(crate) fn fractional_seconds(&self) -> f64 {
        self.numerator as f64 / (self.denominator as f64 * 16_000.0)
    }

    pub(crate) fn advance(&mut self, frames: u64, rate: u32) -> Option<u64> {
        if rate == 0 {
            return None;
        }
        let rate = u128::from(rate);
        let divisor = gcd(self.denominator, rate);
        let scale = rate / divisor;
        let denominator = self.denominator.checked_mul(scale)?;
        let numerator = self.numerator.checked_mul(scale)?.checked_add(
            u128::from(frames)
                .checked_mul(16_000)?
                .checked_mul(self.denominator / divisor)?,
        )?;
        let whole = u64::try_from(numerator / denominator).ok()?;
        let remainder = numerator % denominator;
        let divisor = gcd(remainder, denominator);
        self.numerator = remainder / divisor;
        self.denominator = denominator / divisor;
        Some(whole)
    }
}

fn gcd(mut left: u128, mut right: u128) -> u128 {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_ceil_tail_maps_to_actual_input_and_cross_rate_boundaries() {
        let mut clock = InputClock::default();
        let first = clock.admit(44_101).unwrap();
        clock.map_input(first, 44_100, 0);
        assert_eq!(
            clock.input_span(16_000, 16_001),
            Ok(InputSpan {
                start_frame: 44_100,
                end_frame: 44_101
            })
        );
        clock.end_epoch();
        let second = clock.admit(48_000).unwrap();
        clock.map_input(second, 48_000, 16_001);
        assert_eq!(
            clock.input_span(16_000, 16_002),
            Ok(InputSpan {
                start_frame: 44_100,
                end_frame: 44_104
            })
        );
        assert_eq!(
            clock.input_span(0, 32_001),
            Ok(InputSpan {
                start_frame: 0,
                end_frame: 92_101
            })
        );
        assert_eq!(clock.input_span(0, 32_002), Err(MappingError::Invalid));
    }

    #[test]
    fn bounded_epoch_history_cannot_invent_missing_input_mapping() {
        let mut clock = InputClock::default();
        for position in 0..=MAX_RATE_EPOCHS {
            let span = clock.admit(1).unwrap();
            clock.map_input(span, 16_000, position as u64);
            clock.end_epoch();
        }
        assert_eq!(clock.epochs.len(), MAX_RATE_EPOCHS);
        assert_eq!(clock.input_span(0, 1), Err(MappingError::EvictedHistory));
        assert_eq!(
            clock.input_span(1, 2),
            Ok(InputSpan {
                start_frame: 1,
                end_frame: 2
            })
        );
        clock.reset_vad();
        assert_eq!(clock.frames, 257);
        assert_eq!(clock.input_span(1, 2), Err(MappingError::Invalid));
    }

    #[test]
    fn evicted_prefix_proves_only_covered_ranges_and_counts_each_removal_once() {
        let mut clock = InputClock::default();
        let mut removals = 0;
        for position in 0..260 {
            let span = clock.admit(1).unwrap();
            removals += usize::from(clock.map_input(span, 16_000, position));
            clock.end_epoch();
        }
        assert_eq!(removals, 4);
        assert_eq!(clock.epochs.len(), MAX_RATE_EPOCHS);
        for _ in 0..3 {
            assert_eq!(clock.input_span(0, 260), Err(MappingError::EvictedHistory));
            assert_eq!(clock.input_span(1, 3), Err(MappingError::EvictedHistory));
            assert_eq!(clock.input_span(0, 261), Err(MappingError::Invalid));
            assert_eq!(clock.input_span(260, 261), Err(MappingError::Invalid));
            assert_eq!(
                clock.input_span(u64::MAX - 1, u64::MAX),
                Err(MappingError::Invalid)
            );
            assert_eq!(clock.input_span(0, 0), Err(MappingError::Invalid));
        }
        assert_eq!(
            clock.input_span(259, 260),
            Ok(InputSpan {
                start_frame: 259,
                end_frame: 260
            })
        );
        clock.reset_vad();
        assert_eq!(clock.frames, 260);
        assert_eq!(clock.input_span(0, 260), Err(MappingError::Invalid));
    }

    #[test]
    fn evicted_metadata_never_hides_retained_or_forgotten_source_and_output_holes() {
        for source_hole in [false, true] {
            for hole_at in [2, 10] {
                let mut clock = InputClock::default();
                for position in 0..260 {
                    if source_hole && position == hole_at {
                        clock.admit(1).unwrap();
                    }
                    let span = clock.admit(1).unwrap();
                    let output = position + u64::from(!source_hole && position >= hole_at);
                    clock.map_input(span, 16_000, output);
                    clock.end_epoch();
                }
                assert_eq!(clock.input_span(0, 260), Err(MappingError::Invalid));
                // A later hole does not invalidate the already proved first evicted sample.
                assert_eq!(clock.input_span(0, 1), Err(MappingError::EvictedHistory));
            }
        }
    }

    #[test]
    fn evicted_prefix_does_not_cover_pre_epoch_positions_or_arithmetic_overflow() {
        let mut clock = InputClock::default();
        for position in 10..267 {
            let span = clock.admit(1).unwrap();
            clock.map_input(span, 16_000, position);
            clock.end_epoch();
        }
        assert_eq!(clock.input_span(9, 11), Err(MappingError::Invalid));
        assert_eq!(clock.input_span(10, 267), Err(MappingError::EvictedHistory));
        assert_eq!(clock.input_span(10, 268), Err(MappingError::Invalid));
        let mut overflow = InputClock::default();
        let span = overflow.admit(usize::MAX).unwrap();
        overflow.map_input(span, 1, 0);
        assert_eq!(overflow.input_span(0, 1), Err(MappingError::Invalid));
    }

    #[test]
    fn skipped_rational_carry_never_rounds_each_push() {
        let mut clock = SkippedClock::default();
        let mut emitted = 0;
        for _ in 0..44_101 {
            emitted += clock.advance(1, 44_100).unwrap();
        }
        assert_eq!(emitted, 16_000);
        assert_eq!(clock.advance(2, 48_000), Some(1));
        assert_eq!(clock.advance(0, 0), None);
    }
}
