//! Bounded mapping from native 16 kHz regions back to the channel's original input frames.

use std::collections::VecDeque;

use corti_transcribe::decode::InputSpan;

const MAX_RATE_EPOCHS: usize = 256;

/// The pinned native resampler stores LCM(rate,16000) in i32. A numeric rate range alone is unsafe.
pub(crate) fn boundary_tick_frequency(rate: u32) -> Option<u32> {
    if !(8_000..=192_000).contains(&rate) {
        return None;
    }
    let divisor = gcd(u128::from(rate), 16_000);
    let ticks = u128::from(rate).checked_mul(16_000)?.checked_div(divisor)?;
    (ticks <= i32::MAX as u128).then_some(ticks as u32)
}

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

    /// Experimental rolling ownership: assign each canonical sample by its exact source left edge.
    /// This projects an admitted source fence, not proof that resampling/VAD has delivered it yet.
    /// The supported-rate bound belongs to this experiment, not the existing transcription APIs.
    pub(crate) fn canonical_fence(&self, input_cut: u64) -> Result<u64, MappingError> {
        if self.overflowed || input_cut > self.frames {
            return Err(MappingError::Invalid);
        }
        let mut previous = None;
        let mut selected = None;
        for epoch in &self.epochs {
            if boundary_tick_frequency(epoch.rate).is_none()
                || epoch.input_start > epoch.input_end
                || epoch.input_end > self.frames
            {
                return Err(MappingError::Invalid);
            }
            let output_end = epoch.output_end().ok_or(MappingError::Invalid)?;
            if let Some((input_end, canonical_end)) = previous
                && (input_end != epoch.input_start || canonical_end != epoch.output_start)
            {
                return Err(MappingError::Invalid);
            }
            previous = Some((epoch.input_end, output_end));
            if epoch.input_start <= input_cut && input_cut <= epoch.input_end {
                let relative = u128::from(input_cut - epoch.input_start) * 16_000;
                let canonical = u64::try_from(relative.div_ceil(u128::from(epoch.rate)))
                    .ok()
                    .and_then(|count| epoch.output_start.checked_add(count))
                    .filter(|cut| epoch.output_start <= *cut && *cut <= output_end)
                    .ok_or(MappingError::Invalid)?;
                if selected.is_some_and(|old| old != canonical) {
                    return Err(MappingError::Invalid);
                }
                selected = Some(canonical);
            }
        }
        if let Some(selected) = selected {
            return Ok(selected);
        }
        if self
            .evicted
            .as_ref()
            .is_some_and(|prefix| input_cut <= prefix.input_end)
        {
            Err(MappingError::EvictedHistory)
        } else {
            Err(MappingError::Invalid)
        }
    }

    /// Admission precedes flushing the previous rate's resampler. During that flush the newest input
    /// is counted in `frames` but does not have a rate epoch yet, so a future fence must keep waiting.
    pub(crate) fn mapped_input_end(&self) -> Option<u64> {
        self.epochs.back().map(|epoch| epoch.input_end)
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
    fn source_fence_assigns_fractional_canonical_samples_by_left_edge() {
        let mut clock = InputClock::default();
        let span = clock.admit(11).unwrap();
        clock.map_input(span, 44_100, 0);
        assert_eq!(clock.canonical_fence(3), Ok(2));
        assert_eq!(
            clock.input_span(1, 2),
            Ok(InputSpan {
                start_frame: 2,
                end_frame: 6
            })
        );
        assert_eq!(
            clock.input_span(2, 3),
            Ok(InputSpan {
                start_frame: 5,
                end_frame: 9
            })
        );
        // Outward quality envelopes overlap; the two exact canonical ownership slices do not.
        assert_eq!(clock.canonical_fence(11), Ok(4));
    }

    #[test]
    fn source_fence_partitions_actual_resampler_pcm_across_rates_origins_and_ceil_tails() {
        use crate::resample::Resampler;
        for origin in [0_u64, 149, 2003] {
            for rate in [8_000_u32, 16_000, 44_100, 48_000, 192_000] {
                for frames in [1_usize, 3, 511, 513, 2107] {
                    let source: Vec<f32> = (0..frames)
                        .map(|i| ((i * 37 % 997) as f32 - 400.0) / 1000.0)
                        .collect();
                    let mut clock = InputClock::default();
                    let mut output = Vec::new();
                    let mut resampler = Resampler::new(rate as i32).unwrap();
                    let mut start = 0;
                    for width in [1, 17, 511, 3, 4096].into_iter().cycle() {
                        if start == source.len() {
                            break;
                        }
                        let end = (start + width).min(source.len());
                        let span = clock.admit(end - start).unwrap();
                        clock.map_input(span, rate, output.len() as u64);
                        output.extend(resampler.push(&source[start..end]));
                        start = end;
                    }
                    output.extend(resampler.finish());
                    let total = (frames as u64 * 16_000).div_ceil(u64::from(rate));
                    assert_eq!(output.len() as u64, total);
                    for source_cut in 0..=frames as u64 {
                        let common_cut = origin.checked_add(source_cut).unwrap();
                        let canonical = clock.canonical_fence(common_cut - origin).unwrap();
                        assert!(canonical <= total);
                        let numerator = u128::from(source_cut) * 16_000;
                        assert!(u128::from(canonical) * u128::from(rate) >= numerator);
                        if canonical != 0 {
                            assert!(u128::from(canonical - 1) * u128::from(rate) < numerator);
                        }
                        let (old, next) = output.split_at(canonical as usize);
                        assert_eq!(
                            old.iter()
                                .chain(next)
                                .map(|s| s.to_bits())
                                .collect::<Vec<_>>(),
                            output.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
                        );
                        let next_start = canonical as f64 / 16_000.0;
                        let old_end = old.len() as f64 / 16_000.0;
                        assert_eq!(old_end.to_bits(), next_start.to_bits());
                        if canonical > 0 {
                            assert!(
                                clock.input_span(0, canonical).unwrap().end_frame <= frames as u64
                            );
                        }
                        if canonical < total {
                            assert!(
                                clock.input_span(canonical, total).unwrap().start_frame
                                    >= source_cut
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn source_fence_rate_epochs_keep_canonical_offsets_and_refuse_missing_proof() {
        use crate::resample::Resampler;
        let mut clock = InputClock::default();
        let mut output = Vec::new();
        let mut source_start = 0;
        for (frames, rate) in [
            (113, 16_000),
            (1297, 44_100),
            (7, 48_000),
            (3, 8_000),
            (511, 192_000),
        ] {
            let canonical_start = output.len() as u64;
            let span = clock.admit(frames).unwrap();
            clock.map_input(span, rate, canonical_start);
            let mut resampler = Resampler::new(rate as i32).unwrap();
            output.extend(resampler.push(&vec![0.25; frames]));
            output.extend(resampler.finish());
            clock.end_epoch();
            for source_cut in source_start..=source_start + frames as u64 {
                let expected = canonical_start
                    + ((source_cut - source_start) * 16_000).div_ceil(u64::from(rate));
                assert_eq!(clock.canonical_fence(source_cut), Ok(expected));
            }
            assert_eq!(clock.canonical_fence(clock.frames), Ok(output.len() as u64));
            source_start += frames as u64;
        }
        assert_eq!(
            clock.canonical_fence(clock.frames + 1),
            Err(MappingError::Invalid)
        );
        assert_eq!(
            InputClock::default().canonical_fence(0),
            Err(MappingError::Invalid)
        );
        for rate in [0, 7999, 192_001, u32::MAX] {
            let mut invalid = InputClock::default();
            let span = invalid.admit(5).unwrap();
            invalid.map_input(span, rate, 0);
            assert_eq!(invalid.canonical_fence(3), Err(MappingError::Invalid));
        }
        let mut evicted = InputClock::default();
        for n in 0..257 {
            let span = evicted.admit(1).unwrap();
            evicted.map_input(span, 16_000, n);
            evicted.end_epoch();
        }
        assert_eq!(
            evicted.canonical_fence(0),
            Err(MappingError::EvictedHistory)
        );
        assert_eq!(evicted.canonical_fence(257), Ok(257));
        let mut hole = InputClock::default();
        let span = hole.admit(2).unwrap();
        hole.map_input(span, 16_000, 0);
        hole.end_epoch();
        let span = hole.admit(2).unwrap();
        hole.map_input(span, 16_000, 3);
        assert_eq!(hole.canonical_fence(3), Err(MappingError::Invalid));
        let mut overflow = InputClock::default();
        let span = overflow.admit(3).unwrap();
        overflow.map_input(span, 8_000, u64::MAX - 1);
        assert_eq!(overflow.canonical_fence(2), Err(MappingError::Invalid));
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
