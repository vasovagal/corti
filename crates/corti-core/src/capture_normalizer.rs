//! Bounded, off-HAL normalization of one capture epoch into the recording's fixed rate.
//!
//! Equal rates copy the original `f32` bits and have no lookahead. Changed rates use a Blackman
//! windowed sinc: cutoff `0.45 * min(1, retained/source)` cycles per source frame, with source-frame
//! radius `ceil(16 * max(1, source/retained))`. These are implementation choices, not a measured
//! production-quality claim. Both rates are restricted to 8–192 kHz, bounding the radius to 384.
//! The low-pass kernel avoids the unfiltered downsampling of simple linear interpolation; no native
//! resampler or AEC implementation is involved.
//!
//! Counts come from [`FractionalFrameMapper`], independently of the filter's lookahead. Output frame
//! `m` samples source position `m * source_rate / retained_rate`; positions use integer arithmetic,
//! not an accumulated floating-point phase. Finishing emits exactly the outstanding count, with
//! endpoint extension from this contiguous available segment, and never appends a filter tail.
//! A gap flushes the preceding available segment, emits exact zeroes marked unavailable, and clears
//! filter history. Construct a new normalizer for every epoch, including same-rate epoch changes.
//!
//! The writer must persist the epoch/loss facts before calling methods that emit dependent frames.
//! It must plan whole consecutive loss runs, including runs spanning epochs, before filling them;
//! this module additionally rejects a same-epoch consecutive gap over ten seconds. It neither owns
//! a quality record nor certifies that any unavailable input was recovered. Downstream filters must
//! retain the unavailable mask and zero those frames after filtering too.

use std::fmt;

use crate::capture_accounting::{FractionalFrameMapper, MAX_LOSS_FILL_SECONDS};

pub const MIN_NORMALIZATION_RATE_HZ: u32 = 8_000;
pub const MAX_NORMALIZATION_RATE_HZ: u32 = 192_000;
pub const MAX_NORMALIZATION_CHANNELS: usize = 64;
pub const MAX_NORMALIZATION_RADIUS: usize = 384;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NormalizationError {
    InvalidRate,
    InvalidChannels,
    InvalidPcmShape,
    NonFiniteSample,
    NonFiniteOutput,
    ArithmeticOverflow,
    GapNeedsEpoch,
    Finished,
    Failed,
}

impl fmt::Display for NormalizationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidRate => "capture normalization rate outside 8–192 kHz",
            Self::InvalidChannels => "capture normalization channel count outside 1–64",
            Self::InvalidPcmShape => "capture normalization PCM has an incomplete frame",
            Self::NonFiniteSample => "capture normalization received nonfinite changed-rate PCM",
            Self::NonFiniteOutput => {
                "capture normalization cannot represent a finite output sample"
            }
            Self::ArithmeticOverflow => "capture normalization arithmetic overflow",
            Self::GapNeedsEpoch => "capture normalization gap exceeds ten seconds; epoch required",
            Self::Finished => "capture normalization epoch is already finished",
            Self::Failed => "capture normalization stopped after an emission failure",
        })
    }
}

impl std::error::Error for NormalizationError {}

#[derive(Debug)]
pub enum NormalizationFailure<E> {
    Normalization(NormalizationError),
    Sink(E),
}

impl<E> From<NormalizationError> for NormalizationFailure<E> {
    fn from(error: NormalizationError) -> Self {
        Self::Normalization(error)
    }
}

impl<E: fmt::Display> fmt::Display for NormalizationFailure<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Normalization(error) => error.fmt(f),
            Self::Sink(error) => write!(f, "capture normalization sink: {error}"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for NormalizationFailure<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Normalization(error) => Some(error),
            Self::Sink(error) => Some(error),
        }
    }
}

/// Fixed-capacity PCM storage and constant-size counters for one capture epoch.
///
/// Worst-case PCM history is `(2 * 384 + 2) * 64 * sizeof(f32)` = 197,120 bytes. The first frame,
/// output frame and f64 accumulators add 1,024 bytes at 64 channels. Storage does not grow with
/// input chunk size or recording duration; callbacks receive one retained frame at a time.
pub struct CaptureRateNormalizer {
    source_rate_hz: u32,
    retained_rate_hz: u32,
    channels: usize,
    radius: usize,
    cutoff: f64,
    mapper: FractionalFrameMapper,
    emitted_frames: u64,
    history: Vec<f32>,
    history_head: usize,
    history_len: usize,
    history_start: u64,
    segment_start: Option<u64>,
    segment_first: Vec<f32>,
    output: Vec<f32>,
    sums: Vec<f64>,
    consecutive_gap_frames: u64,
    finished: bool,
    failed: bool,
}

impl CaptureRateNormalizer {
    pub fn new(
        source_rate_hz: u32,
        retained_rate_hz: u32,
        channels: usize,
    ) -> Result<Self, NormalizationError> {
        let rates = MIN_NORMALIZATION_RATE_HZ..=MAX_NORMALIZATION_RATE_HZ;
        if !rates.contains(&source_rate_hz) || !rates.contains(&retained_rate_hz) {
            return Err(NormalizationError::InvalidRate);
        }
        if !(1..=MAX_NORMALIZATION_CHANNELS).contains(&channels) {
            return Err(NormalizationError::InvalidChannels);
        }
        let radius = if source_rate_hz == retained_rate_hz {
            0
        } else {
            (u64::from(source_rate_hz) * 16)
                .div_ceil(u64::from(retained_rate_hz))
                .max(16) as usize
        };
        let history_frames = if radius == 0 { 0 } else { 2 * radius + 2 };
        Ok(Self {
            source_rate_hz,
            retained_rate_hz,
            channels,
            radius,
            cutoff: 0.45 * (f64::from(retained_rate_hz) / f64::from(source_rate_hz)).min(1.0),
            mapper: FractionalFrameMapper::new(source_rate_hz, retained_rate_hz)
                .map_err(|_| NormalizationError::InvalidRate)?,
            emitted_frames: 0,
            history: vec![0.0; history_frames * channels],
            history_head: 0,
            history_len: 0,
            history_start: 0,
            segment_start: None,
            segment_first: vec![0.0; channels],
            output: vec![0.0; channels],
            sums: vec![0.0; channels],
            consecutive_gap_frames: 0,
            finished: false,
            failed: false,
        })
    }

    pub fn source_frames(&self) -> u64 {
        self.mapper.source_frames()
    }

    /// Total allocated retained-frame count, including frames still awaiting lookahead.
    pub fn retained_frames(&self) -> u64 {
        self.mapper.retained_frames()
    }

    pub fn emitted_frames(&self) -> u64 {
        self.emitted_frames
    }

    pub fn fractional_remainder(&self) -> u64 {
        self.mapper.remainder()
    }

    pub fn pending_frames(&self) -> u64 {
        self.retained_frames() - self.emitted_frames
    }

    /// A changed-rate output waits until `floor(source_position) + radius` has arrived.
    /// The initial frame therefore needs at most `radius + 1` source frames; identity needs one.
    pub fn lookahead_source_frames(&self) -> usize {
        self.radius
    }

    /// Accept whole interleaved frames. The callback's boolean is `true` only for unavailable PCM.
    ///
    /// Shape, changed-rate finiteness and total count overflow are checked before admitting input.
    /// Equal rates preserve every input bit, including signed zero and any nonfinite payload;
    /// this bypass deliberately does not sanitize capture PCM. Changed-rate interpolation rejects
    /// nonfinite input, and fails if a finite f64 result would overflow f32 instead of clipping it.
    /// After a sink or interpolation failure, this normalizer cannot be reused.
    pub fn push<E>(
        &mut self,
        interleaved: &[f32],
        mut emit: impl FnMut(&[f32], bool) -> Result<(), E>,
    ) -> Result<(), NormalizationFailure<E>> {
        self.check_open()?;
        if !interleaved.len().is_multiple_of(self.channels) {
            return Err(NormalizationError::InvalidPcmShape.into());
        }
        if self.radius != 0 && interleaved.iter().any(|sample| !sample.is_finite()) {
            return Err(NormalizationError::NonFiniteSample.into());
        }
        let frames = u64::try_from(interleaved.len() / self.channels)
            .map_err(|_| NormalizationError::ArithmeticOverflow)?;
        self.checked_mapping(frames)?;
        for frame in interleaved.chunks_exact(self.channels) {
            let source_index = self.source_frames();
            self.mapper
                .push(1)
                .map_err(|_| NormalizationError::ArithmeticOverflow)?;
            self.consecutive_gap_frames = 0;
            if self.radius == 0 {
                self.output.copy_from_slice(frame);
                self.emit_current(false, &mut emit)?;
            } else {
                self.store_frame(source_index, frame);
                self.emit_available(false, &mut emit)?;
            }
        }
        Ok(())
    }

    /// Advance through unavailable source frames, preserving the mapper's fractional remainder.
    /// All emitted gap frames are exact positive zero and have the unavailable flag set.
    ///
    /// This flushes preceding available PCM before emitting the gap. Consecutive calls are combined
    /// for the ten-second cap. A rejected gap admits no input and emits nothing; use a new mapped
    /// epoch for a larger run, after planning its entire duration in the writer.
    pub fn gap<E>(
        &mut self,
        source_frames: u64,
        mut emit: impl FnMut(&[f32], bool) -> Result<(), E>,
    ) -> Result<(), NormalizationFailure<E>> {
        self.check_open()?;
        let run = self
            .consecutive_gap_frames
            .checked_add(source_frames)
            .ok_or(NormalizationError::ArithmeticOverflow)?;
        if run > u64::from(self.source_rate_hz) * MAX_LOSS_FILL_SECONDS {
            return Err(NormalizationError::GapNeedsEpoch.into());
        }
        let mapped = self.checked_mapping(source_frames)?;
        if source_frames == 0 {
            return Ok(());
        }
        self.emit_available(true, &mut emit)?;
        self.mapper = mapped;
        self.consecutive_gap_frames = run;
        self.clear_segment();
        self.output.fill(0.0);
        while self.emitted_frames < self.retained_frames() {
            self.emit_current(true, &mut emit)?;
        }
        Ok(())
    }

    /// Flush exactly the remaining allocated frames using this segment's endpoint extension.
    /// The following epoch must use a fresh normalizer; this method admits no synthetic source time.
    pub fn finish<E>(
        &mut self,
        mut emit: impl FnMut(&[f32], bool) -> Result<(), E>,
    ) -> Result<(), NormalizationFailure<E>> {
        self.check_open()?;
        self.emit_available(true, &mut emit)?;
        self.finished = true;
        self.clear_segment();
        Ok(())
    }

    fn check_open(&self) -> Result<(), NormalizationError> {
        if self.failed {
            Err(NormalizationError::Failed)
        } else if self.finished {
            Err(NormalizationError::Finished)
        } else {
            Ok(())
        }
    }

    fn checked_mapping(
        &self,
        source_frames: u64,
    ) -> Result<FractionalFrameMapper, NormalizationError> {
        let mut mapper = self.mapper;
        mapper
            .push(source_frames)
            .map_err(|_| NormalizationError::ArithmeticOverflow)?;
        Ok(mapper)
    }

    fn store_frame(&mut self, source_index: u64, frame: &[f32]) {
        if self.segment_start.is_none() {
            self.segment_start = Some(source_index);
            self.segment_first.copy_from_slice(frame);
            self.history_start = source_index;
        }
        let capacity = self.history.len() / self.channels;
        let slot = (self.history_head + self.history_len) % capacity;
        self.history[slot * self.channels..(slot + 1) * self.channels].copy_from_slice(frame);
        if self.history_len == capacity {
            self.history_head = (self.history_head + 1) % capacity;
            // A checked mapper push already established that this source frame is representable.
            self.history_start += 1;
        } else {
            self.history_len += 1;
        }
    }

    fn clear_segment(&mut self) {
        self.history_head = 0;
        self.history_len = 0;
        self.history_start = self.source_frames();
        self.segment_start = None;
    }

    fn source_position(&self) -> (u64, f64) {
        let numerator = u128::from(self.emitted_frames) * u128::from(self.source_rate_hz);
        let rate = u128::from(self.retained_rate_hz);
        // Called only while emitted_frames < floor(source_frames * retained/source), so the
        // quotient is bounded by source_frames and fits u64 even at the end of a long recording.
        (
            (numerator / rate) as u64,
            (numerator % rate) as f64 / f64::from(self.retained_rate_hz),
        )
    }

    fn emit_available<E>(
        &mut self,
        flush: bool,
        emit: &mut impl FnMut(&[f32], bool) -> Result<(), E>,
    ) -> Result<(), NormalizationFailure<E>> {
        while self.emitted_frames < self.retained_frames() {
            let (center, fraction) = self.source_position();
            if !flush
                && u128::from(center) + self.radius as u128 >= u128::from(self.source_frames())
            {
                break;
            }
            if let Err(error) = self.interpolate(center, fraction) {
                self.failed = true;
                return Err(error.into());
            }
            self.emit_current(false, emit)?;
        }
        Ok(())
    }

    fn interpolate(&mut self, center: u64, fraction: f64) -> Result<(), NormalizationError> {
        let first = self
            .segment_start
            .ok_or(NormalizationError::ArithmeticOverflow)?;
        let last = self.source_frames() - 1;
        self.sums.fill(0.0);
        let mut total_weight = 0.0;
        for offset in -(self.radius as i64)..=self.radius as i64 {
            let distance = offset as f64 - fraction;
            if distance.abs() >= self.radius as f64 {
                continue;
            }
            let phase = std::f64::consts::PI * distance / self.radius as f64;
            let window = 0.42 + 0.5 * phase.cos() + 0.08 * (2.0 * phase).cos();
            let argument = 2.0 * std::f64::consts::PI * self.cutoff * distance;
            let sinc = if argument == 0.0 {
                1.0
            } else {
                argument.sin() / argument
            };
            let weight = window * sinc;
            total_weight += weight;
            let index = (i128::from(center) + i128::from(offset))
                .clamp(i128::from(first), i128::from(last)) as u64;
            let samples = if index == first {
                &self.segment_first[..]
            } else {
                let relative = index
                    .checked_sub(self.history_start)
                    .filter(|index| *index < self.history_len as u64)
                    .ok_or(NormalizationError::ArithmeticOverflow)?;
                let capacity = self.history.len() / self.channels;
                let slot = (self.history_head + relative as usize) % capacity;
                &self.history[slot * self.channels..(slot + 1) * self.channels]
            };
            for (sum, sample) in self.sums.iter_mut().zip(samples) {
                *sum += weight * f64::from(*sample);
            }
        }
        if !total_weight.is_finite() || total_weight == 0.0 {
            return Err(NormalizationError::NonFiniteOutput);
        }
        for (output, sum) in self.output.iter_mut().zip(&self.sums) {
            let sample = (sum / total_weight) as f32;
            if !sample.is_finite() {
                return Err(NormalizationError::NonFiniteOutput);
            }
            *output = sample;
        }
        Ok(())
    }

    fn emit_current<E>(
        &mut self,
        unavailable: bool,
        emit: &mut impl FnMut(&[f32], bool) -> Result<(), E>,
    ) -> Result<(), NormalizationFailure<E>> {
        if let Err(error) = emit(&self.output, unavailable) {
            self.failed = true;
            return Err(NormalizationFailure::Sink(error));
        }
        // Emission is bounded by the already checked mapper quota, including the identity path.
        self.emitted_frames += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use super::*;

    type Collected = Vec<(Vec<f32>, bool)>;

    fn collect(output: &mut Collected) -> impl FnMut(&[f32], bool) -> Result<(), Infallible> + '_ {
        |frame, unavailable| {
            output.push((frame.to_vec(), unavailable));
            Ok(())
        }
    }

    fn normalize(source: u32, retained: u32, pcm: &[f32], chunks: &[usize]) -> Collected {
        let mut normalizer = CaptureRateNormalizer::new(source, retained, 2).unwrap();
        let mut output = Vec::new();
        let mut start = 0;
        for frames in chunks.iter().copied().cycle() {
            if start == pcm.len() {
                break;
            }
            let end = (start + 2 * frames).min(pcm.len());
            normalizer
                .push(&pcm[start..end], collect(&mut output))
                .unwrap();
            start = end;
        }
        normalizer.finish(collect(&mut output)).unwrap();
        assert_eq!(normalizer.pending_frames(), 0);
        assert_eq!(normalizer.emitted_frames(), output.len() as u64);
        output
    }

    #[test]
    fn identity_preserves_bits_without_lag_or_tail() {
        let pcm = [
            0.0,
            -0.0,
            f32::MAX,
            -f32::MAX,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            f32::from_bits(0x7fc0_0123),
            f32::INFINITY,
        ];
        let mut normalizer = CaptureRateNormalizer::new(48_000, 48_000, 2).unwrap();
        let mut output = Vec::new();
        normalizer.push(&pcm, collect(&mut output)).unwrap();
        assert_eq!(normalizer.lookahead_source_frames(), 0);
        assert_eq!(normalizer.pending_frames(), 0);
        assert_eq!(normalizer.emitted_frames(), 4);
        normalizer.finish(collect(&mut output)).unwrap();
        assert_eq!(output.len(), 4);
        let bits: Vec<_> = output
            .iter()
            .flat_map(|(frame, _)| frame.iter().map(|x| x.to_bits()))
            .collect();
        assert_eq!(bits, pcm.map(f32::to_bits));
        assert!(output.iter().all(|(_, unavailable)| !unavailable));
    }

    #[test]
    fn exact_counts_and_pcm_are_invariant_under_irregular_chunks() {
        for (source, retained) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (48_000, 16_000),
            (16_000, 48_000),
        ] {
            let pcm: Vec<_> = (0..source)
                .flat_map(|i| {
                    [
                        ((i * 17 % 211) as f32 - 105.0) / 128.0,
                        (i as f32 * 0.013).sin(),
                    ]
                })
                .collect();
            let whole = normalize(source, retained, &pcm, &[source as usize]);
            let irregular = normalize(source, retained, &pcm, &[1, 137, 2, 997, 31]);
            assert_eq!(
                whole, irregular,
                "chunk dependence at {source} -> {retained}"
            );
            assert_eq!(whole.len(), retained as usize);
            assert!(whole.iter().all(|(frame, unavailable)| !unavailable && frame.iter().all(|x| x.is_finite())));
        }
    }

    #[test]
    fn ramp_positions_have_no_added_delay_and_channels_stay_separate() {
        for (source, retained) in [(44_100, 48_000), (48_000, 44_100), (48_000, 16_000)] {
            let pcm: Vec<_> = (0..2_003).flat_map(|i| [i as f32 / 2_003.0, 0.0]).collect();
            let output = normalize(source, retained, &pcm, &[1, 23, 61]);
            let radius = CaptureRateNormalizer::new(source, retained, 2)
                .unwrap()
                .radius as f64;
            for (m, (frame, _)) in output.iter().enumerate() {
                let position = m as f64 * f64::from(source) / f64::from(retained);
                if position >= radius && position < 2_002.0 - radius {
                    assert!((f64::from(frame[0]) - position / 2_003.0).abs() < 2e-6);
                }
                assert_eq!(frame[1], 0.0);
            }
        }
    }

    #[test]
    fn impulse_start_middle_and_end_match_across_push_boundaries() {
        for impulse in [0, 997, 1_999] {
            let mut pcm = vec![0.0; 4_000];
            pcm[2 * impulse] = 1.0;
            let whole = normalize(48_000, 16_000, &pcm, &[2_000]);
            let chunked = normalize(48_000, 16_000, &pcm, &[1, 97, 13, 3]);
            assert_eq!(whole, chunked);
            assert!(whole.iter().all(|(frame, _)| frame[1] == 0.0));
            let peak = whole
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.0[0].abs().total_cmp(&b.0[0].abs()))
                .unwrap()
                .0;
            let expected = (impulse as f64 / 3.0).min((whole.len() - 1) as f64);
            assert!(
                (peak as f64 - expected).abs() <= 1.0,
                "impulse {impulse}, peak {peak}"
            );
        }
    }

    #[test]
    fn constants_and_extreme_finite_pcm_remain_finite() {
        for (source, retained) in [
            (44_100, 48_000),
            (48_000, 16_000),
            (192_000, 8_000),
            (8_000, 192_000),
        ] {
            let pcm: Vec<_> = (0..1_013).flat_map(|_| [f32::MAX, -0.375]).collect();
            let output = normalize(source, retained, &pcm, &[1, 113, 7]);
            for (frame, _) in output {
                assert_eq!(frame[0], f32::MAX);
                assert!((frame[1] + 0.375).abs() <= f32::EPSILON);
            }
        }
    }

    #[test]
    fn downsample_low_pass_has_a_measured_synthetic_attenuation() {
        fn rms(frequency: f64) -> f64 {
            let pcm: Vec<_> = (0..4_800)
                .flat_map(|i| {
                    [
                        (2.0 * std::f64::consts::PI * frequency * i as f64 / 48_000.0).sin() as f32,
                        0.0,
                    ]
                })
                .collect();
            let output = normalize(48_000, 16_000, &pcm, &[137]);
            let interior = &output[100..output.len() - 100];
            (interior
                .iter()
                .map(|(frame, _)| f64::from(frame[0]).powi(2))
                .sum::<f64>()
                / interior.len() as f64)
                .sqrt()
        }
        let low = rms(1_000.0);
        let high = rms(12_000.0);
        // This isolates an alias-prone tone well above the retained Nyquist frequency. It is not a
        // speech/ASR benchmark or a general passband/stopband guarantee across every supported rate.
        assert!(low > 0.69 && low < 0.72, "1 kHz RMS {low}");
        assert!(high < 0.005, "12 kHz RMS {high}");
    }

    #[test]
    fn gaps_keep_exact_fractional_counts_and_zero_masks_without_signal_bleed() {
        let mut normalizer = CaptureRateNormalizer::new(44_100, 48_000, 2).unwrap();
        let mut output = Vec::new();
        normalizer.push(&[0.75; 14], collect(&mut output)).unwrap();
        normalizer.gap(11, collect(&mut output)).unwrap();
        let available_end = 7 * 48_000 / 44_100;
        let gap_end = 18 * 48_000 / 44_100;
        assert_eq!(output.len(), gap_end);
        assert!(
            output[..available_end]
                .iter()
                .all(|(frame, unavailable)| !unavailable
                    && frame.iter().all(|x| (*x - 0.75).abs() < 1e-6))
        );
        assert!(
            output[available_end..gap_end]
                .iter()
                .all(|(frame, unavailable)| *unavailable && frame.iter().all(|x| x.to_bits() == 0))
        );
        normalizer.push(&[-0.25; 38], collect(&mut output)).unwrap();
        normalizer.finish(collect(&mut output)).unwrap();
        assert!(
            output[gap_end..]
                .iter()
                .all(|(frame, unavailable)| !unavailable
                    && frame.iter().all(|x| (*x + 0.25).abs() < 1e-6))
        );
        assert_eq!(output.len(), 37 * 48_000 / 44_100);
        assert_eq!(normalizer.fractional_remainder(), (37 * 48_000) % 44_100);
    }

    #[test]
    fn consecutive_gap_chunks_share_one_cap_and_one_fractional_clock() {
        let mut normalizer = CaptureRateNormalizer::new(44_100, 48_000, 1).unwrap();
        let mut count = 0;
        let mut emit = |frame: &[f32], unavailable: bool| {
            assert!(unavailable);
            assert_eq!(frame[0].to_bits(), 0);
            count += 1;
            Ok::<_, Infallible>(())
        };
        for _ in 0..100 {
            normalizer.gap(4_410, &mut emit).unwrap();
        }
        assert!(matches!(
            normalizer.gap(1, &mut emit),
            Err(NormalizationFailure::Normalization(
                NormalizationError::GapNeedsEpoch
            ))
        ));
        normalizer.finish(&mut emit).unwrap();
        assert_eq!(count, 480_000);
        assert_eq!(normalizer.source_frames(), 441_000);
        assert_eq!(normalizer.fractional_remainder(), 0);
    }

    #[test]
    fn new_epochs_do_not_share_impulse_history() {
        let mut previous = CaptureRateNormalizer::new(48_000, 44_100, 1).unwrap();
        let mut first = Vec::new();
        previous.push(&[1.0], collect(&mut first)).unwrap();
        previous.finish(collect(&mut first)).unwrap();
        assert!(first.is_empty()); // floor(1 * 44_100 / 48_000); no invented tail frame.
        let mut next = CaptureRateNormalizer::new(44_100, 48_000, 1).unwrap();
        let mut second = Vec::new();
        next.push(&[0.0; 100], collect(&mut second)).unwrap();
        next.finish(collect(&mut second)).unwrap();
        assert_eq!(second.len(), 100 * 48_000 / 44_100);
        assert!(second.iter().all(|(frame, _)| frame[0] == 0.0));
    }

    #[test]
    fn lookahead_and_history_storage_have_fixed_bounds() {
        for (source, retained) in [(8_000, 192_000), (192_000, 8_000), (44_100, 48_000)] {
            let mut normalizer = CaptureRateNormalizer::new(source, retained, 1).unwrap();
            let capacity = normalizer.history.capacity();
            let radius = normalizer.radius;
            assert!(radius <= MAX_NORMALIZATION_RADIUS);
            assert_eq!(normalizer.history.len(), 2 * radius + 2);
            for _ in 0..radius {
                normalizer
                    .push(&[0.5], |_, _| Ok::<_, Infallible>(()))
                    .unwrap();
                assert_eq!(normalizer.emitted_frames(), 0);
            }
            normalizer
                .push(&[0.5], |_, _| Ok::<_, Infallible>(()))
                .unwrap();
            assert!(normalizer.emitted_frames() > 0);
            for _ in 0..4_097 {
                normalizer
                    .push(&[0.5], |_, _| Ok::<_, Infallible>(()))
                    .unwrap();
                assert_eq!(normalizer.history.capacity(), capacity);
                assert!(normalizer.history_len <= 2 * radius + 2);
                let pending_bound =
                    ((radius as u64 + 1) * u64::from(retained)).div_ceil(u64::from(source));
                assert!(normalizer.pending_frames() <= pending_bound);
            }
            normalizer.finish(|_, _| Ok::<_, Infallible>(())).unwrap();
            assert_eq!(normalizer.pending_frames(), 0);
        }
    }

    #[test]
    fn invalid_inputs_and_count_overflow_are_rejected_before_admission() {
        for (source, retained) in [(0, 48_000), (48_000, 0), (7_999, 48_000), (48_000, 192_001)] {
            assert!(matches!(
                CaptureRateNormalizer::new(source, retained, 1),
                Err(NormalizationError::InvalidRate)
            ));
        }
        for channels in [0, 65, usize::MAX] {
            assert!(matches!(
                CaptureRateNormalizer::new(48_000, 44_100, channels),
                Err(NormalizationError::InvalidChannels)
            ));
        }
        let mut normalizer = CaptureRateNormalizer::new(44_100, 48_000, 2).unwrap();
        let mut unexpected =
            |_: &[f32], _: bool| -> Result<(), Infallible> { panic!("invalid input emitted PCM") };
        assert!(matches!(
            normalizer.push(&[1.0], &mut unexpected),
            Err(NormalizationFailure::Normalization(
                NormalizationError::InvalidPcmShape
            ))
        ));
        assert!(matches!(
            normalizer.push(&[1.0, f32::NAN], &mut unexpected),
            Err(NormalizationFailure::Normalization(
                NormalizationError::NonFiniteSample
            ))
        ));
        assert!(matches!(
            normalizer.gap(441_001, &mut unexpected),
            Err(NormalizationFailure::Normalization(
                NormalizationError::GapNeedsEpoch
            ))
        ));
        assert_eq!(normalizer.source_frames(), 0);
        normalizer.mapper = FractionalFrameMapper::new(48_000, 48_000).unwrap();
        normalizer.mapper.push(u64::MAX).unwrap();
        normalizer.emitted_frames = u64::MAX;
        assert!(matches!(
            normalizer.push(&[1.0, 1.0], &mut unexpected),
            Err(NormalizationFailure::Normalization(
                NormalizationError::ArithmeticOverflow
            ))
        ));
        assert_eq!(normalizer.source_frames(), u64::MAX);
    }

    #[test]
    fn sink_failure_is_terminal_and_does_not_claim_an_emitted_frame() {
        let mut normalizer = CaptureRateNormalizer::new(48_000, 48_000, 1).unwrap();
        assert!(matches!(
            normalizer.push(&[0.25, 0.75], |_, _| Err("sink failed")),
            Err(NormalizationFailure::Sink("sink failed"))
        ));
        assert_eq!(normalizer.source_frames(), 1);
        assert_eq!(normalizer.emitted_frames(), 0);
        assert!(matches!(
            normalizer.finish(|_, _| Ok::<_, Infallible>(())),
            Err(NormalizationFailure::Normalization(
                NormalizationError::Failed
            ))
        ));
        let mut empty = CaptureRateNormalizer::new(44_100, 48_000, 1).unwrap();
        empty.finish(|_, _| Ok::<_, Infallible>(())).unwrap();
        assert!(matches!(
            empty.push(&[], |_, _| Ok::<_, Infallible>(())),
            Err(NormalizationFailure::Normalization(
                NormalizationError::Finished
            ))
        ));
    }

    #[test]
    fn finite_input_overshoot_fails_before_emitting_nonfinite_pcm() {
        let mut normalizer = CaptureRateNormalizer::new(48_000, 44_100, 1).unwrap();
        let mut pcm = vec![f32::MAX; 256];
        pcm.extend_from_slice(&[-f32::MAX; 256]);
        let result = normalizer.push(&pcm, |frame, _| {
            assert!(frame[0].is_finite());
            Ok::<_, Infallible>(())
        });
        assert!(matches!(
            result,
            Err(NormalizationFailure::Normalization(
                NormalizationError::NonFiniteOutput
            ))
        ));
        assert!(matches!(
            normalizer.finish(|_, _| Ok::<_, Infallible>(())),
            Err(NormalizationFailure::Normalization(
                NormalizationError::Failed
            ))
        ));
    }

    #[test]
    fn oversized_gap_does_not_flush_or_discard_the_pending_available_tail() {
        let mut normalizer = CaptureRateNormalizer::new(44_100, 48_000, 1).unwrap();
        let mut output = Vec::new();
        normalizer.push(&[0.25; 7], collect(&mut output)).unwrap();
        assert!(output.is_empty());
        assert!(matches!(
            normalizer.gap(441_001, collect(&mut output)),
            Err(NormalizationFailure::Normalization(
                NormalizationError::GapNeedsEpoch
            ))
        ));
        assert!(output.is_empty());
        assert_eq!(normalizer.source_frames(), 7);
        normalizer.finish(collect(&mut output)).unwrap();
        assert_eq!(output.len(), 7 * 48_000 / 44_100);
        assert!(output.iter().all(|(frame, unavailable)| {
            !unavailable && (frame[0] - 0.25).abs() <= f32::EPSILON
        }));
    }
}
