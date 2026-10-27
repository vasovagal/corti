//! Allocation-free callback accounting. This module never owns PCM or writes a WAV.
//!
//! The adapter reserves PCM and every reported descriptor slot before copying samples, publishes
//! PCM, then drains the admitted descriptors in order. The writer consumes only described PCM.
//! Format observation, resampling, quality persistence and all filesystem work remain off HAL.
//!
//! A valid sample-clock gap supplies exact missing frames. A host-only discontinuity does not: it
//! records unknown source loss and starts a mapped epoch, without guessed gap frames or silence.
//! Such a host gap is compressed in retained PCM; its WAV duration is not the full source duration.
//! Consecutive actual host observations, with a one-source-frame residual tolerance, distinguish a
//! discontinuity from quantization/bounded drift without accumulating error from the epoch base.

use std::fmt;

use crate::quality::{
    CaptureEpoch, CaptureEpochReason, QualityCause, QualityClass, QualityRecord, QualityResolution,
    QualitySpan, QualityStage, QualityTrack,
};

pub const HAL_DESCRIPTOR_CAPACITY: usize = 4_096;
pub const PENDING_DESCRIPTOR_CAPACITY: usize = 64;
pub const MAX_LOSS_FILL_SECONDS: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureFormat {
    /// A control-side observation generation, bound to this callback's actual format.
    pub generation: u64,
    pub source_rate_hz: u32,
    pub mic_channels: u32,
    pub tap_channels: u32,
}

impl CaptureFormat {
    fn channels(self) -> Result<u64, AccountingError> {
        if self.source_rate_hz == 0 {
            return Err(AccountingError::InvalidFormat);
        }
        let channels = self
            .mic_channels
            .checked_add(self.tap_channels)
            .filter(|channels| *channels != 0)
            .ok_or(AccountingError::InvalidFormat)?;
        Ok(u64::from(channels))
    }
}

/// `source_offset_ns` is the already-converted offset from recording start. The HAL adapter must
/// supply the native validity flags; a value with a false flag is ignored, including zero.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CaptureTimestamp {
    pub sample_time: f64,
    pub sample_time_valid: bool,
    pub source_offset_ns: u64,
    pub host_time_valid: bool,
}

impl CaptureTimestamp {
    fn sample_frame(self) -> Option<i64> {
        // Beyond 2^53, f64 cannot represent successive frame counters. Fractional/nonfinite native
        // values cannot establish an exact frame discontinuity either.
        (self.sample_time_valid
            && self.sample_time.is_finite()
            && self.sample_time.fract() == 0.0
            && self.sample_time.abs() <= 9_007_199_254_740_992.0)
            .then_some(self.sample_time as i64)
    }

    fn offset(self) -> Option<u64> {
        self.host_time_valid.then_some(self.source_offset_ns)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CallbackInput {
    pub frames: u64,
    pub format: CaptureFormat,
    pub timestamp: CaptureTimestamp,
}

/// A transient epoch descriptor. The writer supplies the actual retained WAV base; there is no
/// placeholder retained coordinate that could accidentally be persisted as a real mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochStart {
    pub id: u32,
    pub source_start_ns: Option<u64>,
    pub source_frame_base: u64,
    pub format: CaptureFormat,
    pub reason: CaptureEpochReason,
}

impl EpochStart {
    pub fn with_retained_base(
        self,
        retained_start_frame: u64,
        retained_rate_hz: u32,
    ) -> Result<CaptureEpoch, AccountingError> {
        self.format.channels()?;
        if retained_rate_hz == 0 {
            return Err(AccountingError::InvalidFormat);
        }
        Ok(CaptureEpoch {
            id: self.id,
            source_start_ns: self.source_start_ns,
            source_rate_hz: self.format.source_rate_hz,
            source_frame_base: self.source_frame_base,
            retained_start_frame,
            retained_rate_hz,
            mic_channels: self.format.mic_channels,
            tap_channels: self.format.tap_channels,
            reason: self.reason,
        })
    }

    fn time_at(self, frame: u64) -> Result<Option<u64>, AccountingError> {
        let delta = frame
            .checked_sub(self.source_frame_base)
            .ok_or(AccountingError::ArithmeticOverflow)?;
        self.source_start_ns
            .map(|base| {
                let nanos =
                    u128::from(delta) * 1_000_000_000 / u128::from(self.format.source_rate_hz);
                let nanos =
                    u64::try_from(nanos).map_err(|_| AccountingError::ArithmeticOverflow)?;
                base.checked_add(nanos)
                    .ok_or(AccountingError::ArithmeticOverflow)
            })
            .transpose()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioBlock {
    pub epoch: u32,
    pub start_frame: u64,
    pub frames: u64,
    pub samples: u64,
    pub format: CaptureFormat,
}

/// A transient descriptor summary, applied to the single shared record by the writer. It is never
/// a separate persisted quality model. Counts are deltas, drained exactly once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountingOverflow {
    pub omitted_epochs: u64,
    pub omitted_source_spans: u64,
    pub counter_saturated: bool,
}

impl AccountingOverflow {
    fn is_empty(self) -> bool {
        self == Self::default()
    }

    /// Off-HAL: the record's bounded vectors and validation are not callback operations.
    pub fn apply_to(self, record: &mut QualityRecord) -> Result<(), crate::quality::QualityError> {
        record.record_omitted_spans(
            QualityClass::SourceLoss,
            QualityResolution::Outstanding,
            self.omitted_source_spans,
        )?;
        record.record_omitted_epochs(self.omitted_epochs);
        record.overflow.counter_saturated |= self.counter_saturated;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureDescriptor {
    Epoch(EpochStart),
    Loss(QualitySpan),
    Overflow(AccountingOverflow),
    Audio(AudioBlock),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DroppedCounts {
    pub frames: u64,
    pub samples: u64,
    pub counter_saturated: bool,
}

#[derive(Clone, Copy)]
struct LossRunStart {
    epoch: u32,
    frame: u64,
    source_ns: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountingError {
    InvalidFormat,
    InvalidCapacity,
    EmptyCallback,
    ArithmeticOverflow,
    EpochIdExhausted,
    AdmissionNotDrained,
    Stopped,
}

impl fmt::Display for AccountingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidFormat => "invalid capture accounting format",
            Self::InvalidCapacity => "invalid capture descriptor capacity",
            Self::EmptyCallback => "empty capture callback",
            Self::ArithmeticOverflow => "capture accounting arithmetic overflow",
            Self::EpochIdExhausted => "capture epoch identifiers exhausted",
            Self::AdmissionNotDrained => "capture admission descriptors remain undrained",
            Self::Stopped => "capture accounting is stopped",
        })
    }
}

impl std::error::Error for AccountingError {}

/// Fixed callback storage: `PENDING` descriptors plus one overflow summary, one resynchronizing
/// epoch and one admitted audio descriptor. Construction and every method below allocate nothing.
/// The external SPSC descriptor ring has [`HAL_DESCRIPTOR_CAPACITY`] preallocated slots.
pub struct CaptureTimeline<const PENDING: usize = PENDING_DESCRIPTOR_CAPACITY> {
    pending: [Option<CaptureDescriptor>; PENDING],
    pending_len: usize,
    overflow: AccountingOverflow,
    current: Option<EpochStart>,
    format_needs_binding: bool,
    next_frame: u64,
    expected_sample: Option<i64>,
    /// Next callback's host boundary, reanchored on every actual valid host observation. Known-format
    /// callbacks without host flags advance an existing anchor; they never create a missing one.
    /// Unknown formats break the anchor. Epoch-base prediction would accumulate ordinary drift.
    expected_host_ns: Option<u64>,
    /// Fractional nanoseconds at the previous callback's source rate, retained while host flags are
    /// absent so repeated floor conversion cannot manufacture a discontinuity at a fractional rate.
    expected_host_remainder: u64,
    minimum_source_ns: Option<u64>,
    loss_run_start: Option<LossRunStart>,
    unknown_loss_run: bool,
    need_resync: bool,
    ready_epoch: Option<EpochStart>,
    ready_audio: Option<AudioBlock>,
    admission_open: bool,
    stopped: bool,
    dropped: DroppedCounts,
}

impl<const PENDING: usize> CaptureTimeline<PENDING> {
    pub fn new() -> Result<Self, AccountingError> {
        if PENDING < 2 || PENDING > HAL_DESCRIPTOR_CAPACITY - 3 {
            return Err(AccountingError::InvalidCapacity);
        }
        Ok(Self {
            pending: [None; PENDING],
            pending_len: 0,
            overflow: AccountingOverflow::default(),
            current: None,
            format_needs_binding: false,
            next_frame: 0,
            expected_sample: None,
            expected_host_ns: None,
            expected_host_remainder: 0,
            minimum_source_ns: None,
            loss_run_start: None,
            unknown_loss_run: false,
            need_resync: false,
            ready_epoch: None,
            ready_audio: None,
            admission_open: false,
            stopped: false,
            dropped: DroppedCounts::default(),
        })
    }

    pub fn dropped_counts(&self) -> DroppedCounts {
        self.dropped
    }

    /// Plan one callback using capacities available to its sole producer. The producer must reserve
    /// all `descriptor_slots()` and `pcm_samples()` together; a rejected callback admits no PCM.
    /// After copying/publishing admitted PCM, exhaust the returned iterator before the next callback.
    /// Dropping an admitted iterator early leaves the state blocked rather than losing its tail.
    pub fn plan_callback(
        &mut self,
        input: CallbackInput,
        available_pcm_samples: usize,
        available_descriptors: usize,
    ) -> Result<CallbackAdmission<'_, PENDING>, AccountingError> {
        self.check_idle()?;
        if input.frames == 0 {
            return Err(AccountingError::EmptyCallback);
        }
        let channels = match input.format.channels() {
            Ok(channels) => channels,
            Err(error) => {
                let supplied_channels =
                    u64::from(input.format.mic_channels) + u64::from(input.format.tap_channels);
                let samples = input
                    .frames
                    .checked_mul(supplied_channels)
                    .unwrap_or_else(|| {
                        self.dropped.counter_saturated = true;
                        u64::MAX
                    });
                self.account_unknown_loss(input.frames, samples);
                self.stopped = true;
                return Err(error);
            }
        };
        let samples = match input.frames.checked_mul(channels) {
            Some(samples) => samples,
            None => {
                self.dropped.counter_saturated = true;
                self.account_unknown_loss(input.frames, u64::MAX);
                self.stopped = true;
                return Err(AccountingError::ArithmeticOverflow);
            }
        };
        let observed_host = input.timestamp.offset();
        let same_source_rate = self
            .current
            .is_some_and(|current| current.format.source_rate_hz == input.format.source_rate_hz);
        let unobserved_sample_gap = if observed_host.is_none() && same_source_rate {
            input
                .timestamp
                .sample_frame()
                .zip(self.expected_sample)
                .and_then(|(now, expected)| {
                    u64::try_from(i128::from(now) - i128::from(expected)).ok()
                })
                .unwrap_or(0)
        } else {
            0
        };
        let host_remainder = if observed_host.is_none() && same_source_rate {
            self.expected_host_remainder
        } else {
            0
        };
        let next_host = observed_host
            .or(self.expected_host_ns)
            .map(|offset| {
                let numerator = (u128::from(input.frames) + u128::from(unobserved_sample_gap))
                    * 1_000_000_000
                    + u128::from(host_remainder);
                let rate = u128::from(input.format.source_rate_hz);
                let duration = u64::try_from(numerator / rate)
                    .map_err(|_| AccountingError::ArithmeticOverflow)?;
                offset
                    .checked_add(duration)
                    .map(|next| (next, (numerator % rate) as u64))
                    .ok_or(AccountingError::ArithmeticOverflow)
            })
            .transpose();
        let next_host = match next_host {
            Ok(next) => next,
            Err(error) => {
                self.account_unknown_loss(input.frames, samples);
                self.stopped = true;
                return Err(error);
            }
        };
        let prepared = self.prepare_callback(input);
        if let Err(error) = prepared {
            self.account_unknown_loss(input.frames, samples);
            self.stopped = true;
            return Err(error);
        }
        let start_frame = self.next_frame;
        let end_frame = match start_frame.checked_add(input.frames) {
            Some(end) => end,
            None => {
                self.account_unknown_loss(input.frames, samples);
                self.stopped = true;
                return Err(AccountingError::ArithmeticOverflow);
            }
        };
        let current = self.current.ok_or(AccountingError::InvalidFormat)?;
        let mut source_end = match current.time_at(end_frame) {
            Ok(source_end) => source_end,
            Err(error) => {
                self.account_unknown_loss(input.frames, samples);
                self.stopped = true;
                return Err(error);
            }
        };
        let slots = self.pending_len
            + usize::from(!self.overflow.is_empty())
            + usize::from(self.need_resync)
            + 1;
        let accepted = samples <= available_pcm_samples as u64
            && slots <= available_descriptors.min(HAL_DESCRIPTOR_CAPACITY);
        if accepted {
            if self.need_resync {
                let predicted = current.time_at(start_frame)?;
                let minimum = match (predicted, self.minimum_source_ns) {
                    (Some(predicted), Some(minimum)) => Some(predicted.max(minimum)),
                    (predicted, minimum) => predicted.or(minimum),
                };
                let offset = input
                    .timestamp
                    .offset()
                    .or(predicted)
                    .filter(|offset| minimum.is_none_or(|minimum| *offset >= minimum));
                let epoch = match self.next_epoch(
                    input.format,
                    offset,
                    CaptureEpochReason::HostDiscontinuity,
                ) {
                    Ok(epoch) => epoch,
                    Err(error) => {
                        self.account_unknown_loss(input.frames, samples);
                        self.stopped = true;
                        return Err(error);
                    }
                };
                source_end = match epoch.time_at(end_frame) {
                    Ok(source_end) => source_end,
                    Err(error) => {
                        self.account_unknown_loss(input.frames, samples);
                        self.stopped = true;
                        return Err(error);
                    }
                };
                self.current = Some(epoch);
                self.ready_epoch = Some(epoch);
                self.need_resync = false;
            }
            let epoch = self.current.ok_or(AccountingError::InvalidFormat)?;
            self.ready_audio = Some(AudioBlock {
                epoch: epoch.id,
                start_frame,
                frames: input.frames,
                samples,
                format: input.format,
            });
            self.admission_open = true;
            self.loss_run_start = None;
            self.unknown_loss_run = false;
        } else {
            self.queue_loss(QualitySpan {
                epoch: current.id,
                start_frame,
                end_frame,
                track: QualityTrack::Both,
                cause: QualityCause::RingOverflow,
                class: QualityClass::SourceLoss,
                stage: QualityStage::Capture,
                resolution: QualityResolution::Outstanding,
            });
            self.count_dropped(input.frames, samples);
        }
        self.next_frame = end_frame;
        if let Some(source_end) = source_end {
            self.minimum_source_ns = Some(
                self.minimum_source_ns
                    .map_or(source_end, |minimum| minimum.max(source_end)),
            );
        }
        // A sample anchor cannot advance in a different rate's units. If the transition has no
        // valid sample clock, wait for a valid observation to anchor the new rate.
        let sample = input
            .timestamp
            .sample_frame()
            .or(self.expected_sample.filter(|_| same_source_rate));
        self.expected_sample = sample.and_then(|sample| {
            i64::try_from(input.frames)
                .ok()
                .and_then(|frames| sample.checked_add(frames))
        });
        // Rejected callbacks still advance the host boundary. An invalid flag's supplied value is
        // ignored; only a previously anchored, known-format interval may be advanced through it.
        self.expected_host_ns = next_host.map(|(next, _)| next);
        self.expected_host_remainder = next_host.map_or(0, |(_, remainder)| remainder);
        Ok(CallbackAdmission {
            timeline: self,
            accepted,
            pcm_samples: samples,
            descriptor_slots: if accepted { slots } else { 0 },
        })
    }

    /// Use this when a control-side format generation cannot yet be bound to the callback. No stale
    /// rate/layout is used to interpret its PCM. A later valid callback starts a fresh mapped epoch.
    pub fn record_unknown_format_loss(
        &mut self,
        frames: u64,
        samples: u64,
    ) -> Result<(), AccountingError> {
        self.check_idle()?;
        self.account_unknown_loss(frames, samples);
        self.format_needs_binding = true;
        self.expected_sample = None;
        self.expected_host_ns = None;
        self.expected_host_remainder = 0;
        self.next_frame = match self.next_frame.checked_add(frames) {
            Some(next) => next,
            None => {
                self.stopped = true;
                return Err(AccountingError::ArithmeticOverflow);
            }
        };
        Ok(())
    }

    /// Call only after destroying/stopping the IO proc, before closing the producer or flushing the
    /// filter tail. Subsequent callbacks are rejected. This also works after arithmetic exhaustion.
    pub fn finish(&mut self) -> Result<TerminalDescriptors<'_, PENDING>, AccountingError> {
        if self.admission_open {
            return Err(AccountingError::AdmissionNotDrained);
        }
        self.stopped = true;
        Ok(TerminalDescriptors { timeline: self })
    }

    fn check_idle(&self) -> Result<(), AccountingError> {
        if self.admission_open {
            Err(AccountingError::AdmissionNotDrained)
        } else if self.stopped {
            Err(AccountingError::Stopped)
        } else {
            Ok(())
        }
    }

    fn prepare_callback(&mut self, input: CallbackInput) -> Result<(), AccountingError> {
        let Some(current) = self.current else {
            let epoch = self.next_epoch(
                input.format,
                input.timestamp.offset(),
                CaptureEpochReason::Start,
            )?;
            self.current = Some(epoch);
            self.format_needs_binding = false;
            self.queue_epoch(epoch);
            return Ok(());
        };
        let format_changed = input.format != current.format;
        let rate_changed = input.format.source_rate_hz != current.format.source_rate_hz;
        if self.format_needs_binding {
            // Unknown-format frames cannot be timed through the previous epoch's rate. Bind the
            // next known callback before admission is decided, so a rejected or terminal callback
            // also records its loss in the new epoch. Preserve the previously published mapping.
            let offset = input.timestamp.offset().filter(|offset| {
                self.minimum_source_ns
                    .is_none_or(|minimum| *offset >= minimum)
            });
            let reason = if rate_changed {
                CaptureEpochReason::RateChange
            } else if format_changed {
                CaptureEpochReason::LayoutChange
            } else {
                CaptureEpochReason::HostDiscontinuity
            };
            let epoch = self.next_epoch(input.format, offset, reason)?;
            self.current = Some(epoch);
            self.format_needs_binding = false;
            self.queue_epoch(epoch);
            return Ok(());
        }
        let sample = input.timestamp.sample_frame();
        let sample_delta = sample
            .zip(self.expected_sample)
            .and_then(|(now, expected)| {
                (now != expected).then_some(i128::from(now) - i128::from(expected))
            });
        // A layout generation alone does not change the sample clock's unit. Preserve a coincident
        // jump. Across changed rates the clock domain may reset, so account unknown intervening
        // mapping instead of interpreting the difference in either the stale or the new rate.
        let unknown_clock_change = rate_changed && sample_delta.is_some();
        let jump = if unknown_clock_change {
            None
        } else {
            sample_delta
        };
        let host_delta =
            self.expected_host_ns
                .zip(input.timestamp.offset())
                .map(|(expected, observed)| {
                    // The existing exact sample gap already accounts for this duration. Across a rate
                    // change there is no shared sample-clock unit, so never apply that correction.
                    let known_gap_ns = jump.filter(|jump| *jump > 0).map_or(0, |gap| {
                        (gap * 1_000_000_000 + i128::from(self.expected_host_remainder))
                            / i128::from(current.format.source_rate_hz)
                    });
                    i128::from(observed) - (i128::from(expected) + known_gap_ns)
                });
        // A derived one-source-frame tolerance covers integer-nanosecond quantization and bounded
        // callback jitter. Flooring keeps the tolerance no larger than one frame; no tuning applies.
        let tolerance_ns = 1_000_000_000 / u64::from(current.format.source_rate_hz);
        let host_discontinuity =
            host_delta.is_some_and(|delta| delta.unsigned_abs() > u128::from(tolerance_ns));
        if !format_changed && jump.is_none() && !host_discontinuity {
            return Ok(());
        }
        let reason = if rate_changed {
            CaptureEpochReason::RateChange
        } else if format_changed {
            CaptureEpochReason::LayoutChange
        } else {
            CaptureEpochReason::HostDiscontinuity
        };
        if unknown_clock_change {
            increment(
                &mut self.overflow.omitted_epochs,
                1,
                &mut self.overflow.counter_saturated,
            );
        }
        if host_discontinuity {
            // A host residual does not prove an exact missing source-frame count. Preserve a sticky
            // source-loss fact and remap without inserting guessed silence or advancing next_frame.
            // A simultaneous backwards sample reset already contributes this unknown-loss fact.
            if !jump.is_some_and(|jump| jump < 0) {
                self.omit_source_span();
            }
            self.unknown_loss_run = true;
        }
        if let Some(jump) = jump {
            if jump > 0 {
                let missing =
                    u64::try_from(jump).map_err(|_| AccountingError::ArithmeticOverflow)?;
                let end = self
                    .next_frame
                    .checked_add(missing)
                    .ok_or(AccountingError::ArithmeticOverflow)?;
                current.time_at(end)?;
                self.queue_loss(QualitySpan {
                    epoch: current.id,
                    start_frame: self.next_frame,
                    end_frame: end,
                    track: QualityTrack::Both,
                    cause: QualityCause::HostDiscontinuity,
                    class: QualityClass::SourceLoss,
                    stage: QualityStage::Capture,
                    resolution: QualityResolution::Outstanding,
                });
                self.next_frame = end;
            } else {
                // A backwards/reset sample clock does not identify a missing interval's length.
                self.omit_source_span();
                self.need_resync = true;
            }
        }
        let previous_boundary = current.time_at(self.next_frame)?;
        let minimum = match (previous_boundary, self.minimum_source_ns) {
            (Some(boundary), Some(minimum)) => Some(boundary.max(minimum)),
            (boundary, minimum) => boundary.or(minimum),
        };
        let predicted =
            if unknown_clock_change || host_discontinuity || jump.is_some_and(|jump| jump < 0) {
                None
            } else {
                previous_boundary
            };
        let offset = if host_discontinuity && host_delta.is_some_and(|delta| delta < 0) {
            None
        } else {
            input.timestamp.offset().or(predicted)
        };
        // A backwards host timestamp cannot repair the source mapping.
        let offset = offset.filter(|offset| minimum.is_none_or(|minimum| *offset >= minimum));
        let epoch = self.next_epoch(input.format, offset, reason)?;
        self.current = Some(epoch);
        self.queue_epoch(epoch);
        Ok(())
    }

    fn next_epoch(
        &self,
        format: CaptureFormat,
        source_start_ns: Option<u64>,
        reason: CaptureEpochReason,
    ) -> Result<EpochStart, AccountingError> {
        let id = self
            .current
            .map(|current| {
                current
                    .id
                    .checked_add(1)
                    .ok_or(AccountingError::EpochIdExhausted)
            })
            .transpose()?
            .unwrap_or(0);
        Ok(EpochStart {
            id,
            source_start_ns,
            source_frame_base: self.next_frame,
            format,
            reason,
        })
    }

    fn queue_epoch(&mut self, epoch: EpochStart) {
        if self.push_pending(CaptureDescriptor::Epoch(epoch)) {
            self.need_resync = false;
        } else {
            increment(
                &mut self.overflow.omitted_epochs,
                1,
                &mut self.overflow.counter_saturated,
            );
            self.need_resync = true;
        }
    }

    fn queue_loss(&mut self, span: QualitySpan) {
        self.need_resync |= self.loss_run_exceeds_cap(span);
        if let Some(Some(CaptureDescriptor::Loss(previous))) =
            self.pending.get_mut(self.pending_len.wrapping_sub(1))
            && previous.epoch == span.epoch
            && previous.track == span.track
            && previous.cause == span.cause
            && previous.class == span.class
            && previous.stage == span.stage
            && previous.resolution == span.resolution
            && previous.end_frame == span.start_frame
        {
            previous.end_frame = span.end_frame;
            return;
        }
        if !self.push_pending(CaptureDescriptor::Loss(span)) {
            self.omit_source_span();
            self.need_resync = true;
        }
    }

    fn loss_run_exceeds_cap(&mut self, span: QualitySpan) -> bool {
        if self.unknown_loss_run {
            return true;
        }
        let Some(current) = self.current else {
            return true;
        };
        let run = *self.loss_run_start.get_or_insert_with(|| LossRunStart {
            epoch: span.epoch,
            frame: span.start_frame,
            source_ns: current.time_at(span.start_frame).ok().flatten(),
        });
        if run.epoch == span.epoch {
            return span.end_frame.checked_sub(run.frame).is_none_or(|frames| {
                frames > u64::from(current.format.source_rate_hz) * MAX_LOSS_FILL_SECONDS
            });
        }
        // Separate rate/layout epochs still belong to one unavailable run until audio is accepted.
        // If its absolute duration is unknown, remap conservatively before the next audio block.
        run.source_ns
            .zip(current.time_at(span.end_frame).ok().flatten())
            .and_then(|(start, end)| end.checked_sub(start))
            .is_none_or(|duration| duration > MAX_LOSS_FILL_SECONDS * 1_000_000_000)
    }

    fn push_pending(&mut self, descriptor: CaptureDescriptor) -> bool {
        if self.pending_len == PENDING {
            return false;
        }
        self.pending[self.pending_len] = Some(descriptor);
        self.pending_len += 1;
        true
    }

    fn pop_pending(&mut self) -> Option<CaptureDescriptor> {
        if self.pending_len == 0 {
            return None;
        }
        let descriptor = self.pending[0].take();
        self.pending.copy_within(1..self.pending_len, 0);
        self.pending_len -= 1;
        self.pending[self.pending_len] = None;
        descriptor
    }

    fn omit_source_span(&mut self) {
        increment(
            &mut self.overflow.omitted_source_spans,
            1,
            &mut self.overflow.counter_saturated,
        );
    }

    fn count_dropped(&mut self, frames: u64, samples: u64) {
        increment(
            &mut self.dropped.frames,
            frames,
            &mut self.dropped.counter_saturated,
        );
        increment(
            &mut self.dropped.samples,
            samples,
            &mut self.dropped.counter_saturated,
        );
        self.overflow.counter_saturated |= self.dropped.counter_saturated;
    }

    fn account_unknown_loss(&mut self, frames: u64, samples: u64) {
        self.unknown_loss_run = true;
        self.omit_source_span();
        increment(
            &mut self.overflow.omitted_epochs,
            1,
            &mut self.overflow.counter_saturated,
        );
        self.count_dropped(frames, samples);
        self.need_resync = true;
    }

    fn pop_overflow(&mut self) -> Option<CaptureDescriptor> {
        if self.overflow.is_empty() {
            None
        } else {
            Some(CaptureDescriptor::Overflow(std::mem::take(
                &mut self.overflow,
            )))
        }
    }

    fn pop_ready(&mut self) -> Option<CaptureDescriptor> {
        if !self.admission_open {
            return None;
        }
        if let Some(descriptor) = self.pop_pending().or_else(|| self.pop_overflow()) {
            return Some(descriptor);
        }
        if let Some(epoch) = self.ready_epoch.take() {
            return Some(CaptureDescriptor::Epoch(epoch));
        }
        let audio = self.ready_audio.take().map(CaptureDescriptor::Audio);
        self.admission_open = false;
        audio
    }
}

pub struct CallbackAdmission<'a, const PENDING: usize> {
    timeline: &'a mut CaptureTimeline<PENDING>,
    accepted: bool,
    pcm_samples: u64,
    descriptor_slots: usize,
}

impl<const PENDING: usize> CallbackAdmission<'_, PENDING> {
    pub fn accepted(&self) -> bool {
        self.accepted
    }

    /// Required PCM sample slots, including rejected callbacks (which must publish none).
    pub fn pcm_samples(&self) -> u64 {
        self.pcm_samples
    }

    /// Exact number of slots reserved for an accepted callback; zero if rejected.
    pub fn descriptor_slots(&self) -> usize {
        self.descriptor_slots
    }
}

impl<const PENDING: usize> Iterator for CallbackAdmission<'_, PENDING> {
    type Item = CaptureDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        if self.accepted {
            self.timeline.pop_ready()
        } else {
            None
        }
    }
}

pub struct TerminalDescriptors<'a, const PENDING: usize> {
    timeline: &'a mut CaptureTimeline<PENDING>,
}

impl<const PENDING: usize> Iterator for TerminalDescriptors<'_, PENDING> {
    type Item = CaptureDescriptor;

    fn next(&mut self) -> Option<Self::Item> {
        self.timeline
            .pop_pending()
            .or_else(|| self.timeline.pop_overflow())
    }
}

fn increment(value: &mut u64, count: u64, saturated: &mut bool) {
    match value.checked_add(count) {
        Some(sum) => *value = sum,
        None => {
            *value = u64::MAX;
            *saturated = true;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LossFillPlan {
    /// Feed this many unavailable source-rate frames through the writer's existing normalization.
    pub fill_source_frames: u64,
    /// Larger gaps need a new retained/source mapping instead of expanding the WAV.
    pub epoch_break: bool,
}

impl LossFillPlan {
    /// Plan the entire consecutive unavailable run at one rate, even when its QualitySpans have
    /// different causes. The writer buffers loss descriptors through the next epoch/audio boundary
    /// before filling; planning each marker independently could exceed the ten-second limit. For
    /// runs crossing rates, compare the complete run's duration before filling any constituent span.
    pub fn for_gap(source_frames: u64, source_rate_hz: u32) -> Result<Self, AccountingError> {
        if source_rate_hz == 0 {
            return Err(AccountingError::InvalidFormat);
        }
        let epoch_break = source_frames > u64::from(source_rate_hz) * MAX_LOSS_FILL_SECONDS;
        Ok(Self {
            fill_source_frames: if epoch_break { 0 } else { source_frames },
            epoch_break,
        })
    }
}

/// Checked source-to-WAV frame counts. Construct a new mapper for each rate epoch; this is timing
/// arithmetic, not a resampler. Unchanged-rate input maps identically without touching its PCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FractionalFrameMapper {
    source_rate_hz: u32,
    retained_rate_hz: u32,
    remainder: u64,
    source_frames: u64,
    retained_frames: u64,
}

impl FractionalFrameMapper {
    pub fn new(source_rate_hz: u32, retained_rate_hz: u32) -> Result<Self, AccountingError> {
        if source_rate_hz == 0 || retained_rate_hz == 0 {
            return Err(AccountingError::InvalidFormat);
        }
        Ok(Self {
            source_rate_hz,
            retained_rate_hz,
            remainder: 0,
            source_frames: 0,
            retained_frames: 0,
        })
    }

    pub fn push(&mut self, frames: u64) -> Result<u64, AccountingError> {
        let numerator =
            u128::from(frames) * u128::from(self.retained_rate_hz) + u128::from(self.remainder);
        let mapped = u64::try_from(numerator / u128::from(self.source_rate_hz))
            .map_err(|_| AccountingError::ArithmeticOverflow)?;
        let remainder = (numerator % u128::from(self.source_rate_hz)) as u64;
        let source_frames = self
            .source_frames
            .checked_add(frames)
            .ok_or(AccountingError::ArithmeticOverflow)?;
        let retained_frames = self
            .retained_frames
            .checked_add(mapped)
            .ok_or(AccountingError::ArithmeticOverflow)?;
        self.source_frames = source_frames;
        self.retained_frames = retained_frames;
        self.remainder = remainder;
        Ok(mapped)
    }

    pub fn source_frames(&self) -> u64 {
        self.source_frames
    }

    pub fn retained_frames(&self) -> u64 {
        self.retained_frames
    }

    pub fn remainder(&self) -> u64 {
        self.remainder
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::SourceCompleteness;

    fn format() -> CaptureFormat {
        CaptureFormat {
            generation: 0,
            source_rate_hz: 48_000,
            mic_channels: 1,
            tap_channels: 1,
        }
    }

    fn block(start: u64, frames: u64) -> CallbackInput {
        CallbackInput {
            frames,
            format: format(),
            timestamp: CaptureTimestamp {
                sample_time: start as f64,
                sample_time_valid: true,
                source_offset_ns: start * 1_000_000_000 / 48_000,
                host_time_valid: true,
            },
        }
    }

    fn accepted<const N: usize>(
        timeline: &mut CaptureTimeline<N>,
        input: CallbackInput,
    ) -> Vec<CaptureDescriptor> {
        let admission = timeline
            .plan_callback(input, usize::MAX, HAL_DESCRIPTOR_CAPACITY)
            .unwrap();
        assert!(admission.accepted());
        let slots = admission.descriptor_slots();
        let descriptors: Vec<_> = admission.collect();
        assert_eq!(descriptors.len(), slots);
        assert!(matches!(
            descriptors.last(),
            Some(CaptureDescriptor::Audio(_))
        ));
        descriptors
    }

    fn rejected<const N: usize>(
        timeline: &mut CaptureTimeline<N>,
        input: CallbackInput,
        pcm: usize,
        descriptors: usize,
    ) {
        let mut admission = timeline.plan_callback(input, pcm, descriptors).unwrap();
        assert!(!admission.accepted());
        assert_eq!(admission.descriptor_slots(), 0);
        assert!(admission.next().is_none());
    }

    #[test]
    fn accepted_lost_accepted_is_ordered_and_counts_frames_separately_from_samples() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        let first = accepted(&mut timeline, block(0, 100));
        assert!(matches!(
            first.as_slice(),
            [CaptureDescriptor::Epoch(_), CaptureDescriptor::Audio(_)]
        ));
        rejected(&mut timeline, block(100, 100), 199, HAL_DESCRIPTOR_CAPACITY);
        rejected(&mut timeline, block(200, 100), 199, HAL_DESCRIPTOR_CAPACITY);
        let third = accepted(&mut timeline, block(300, 100));
        let [
            CaptureDescriptor::Loss(loss),
            CaptureDescriptor::Audio(audio),
        ] = third.as_slice()
        else {
            panic!("expected positioned loss before audio");
        };
        assert_eq!((loss.start_frame, loss.end_frame), (100, 300));
        assert_eq!(loss.cause, QualityCause::RingOverflow);
        assert_eq!(loss.track, QualityTrack::Both);
        assert_eq!(audio.start_frame, 300);
        assert_eq!(audio.samples, 200);
        assert_eq!(
            timeline.dropped_counts(),
            DroppedCounts {
                frames: 200,
                samples: 400,
                counter_saturated: false
            }
        );
    }

    #[test]
    fn independent_descriptor_pressure_reserves_all_preceding_facts() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        rejected(&mut timeline, block(0, 100), 200, 1);
        // Initial epoch + accumulated loss + current audio require three descriptors, although PCM fits.
        rejected(&mut timeline, block(100, 100), 200, 2);
        let admission = timeline.plan_callback(block(200, 100), 200, 3).unwrap();
        assert!(admission.accepted());
        assert_eq!(admission.descriptor_slots(), 3);
        let descriptors: Vec<_> = admission.collect();
        let [
            CaptureDescriptor::Epoch(epoch),
            CaptureDescriptor::Loss(loss),
            CaptureDescriptor::Audio(audio),
        ] = descriptors.as_slice()
        else {
            panic!("expected epoch and loss before accepted audio");
        };
        assert_eq!(epoch.source_frame_base, 0);
        assert_eq!((loss.start_frame, loss.end_frame), (0, 200));
        assert_eq!(audio.start_frame, 200);
    }

    #[test]
    fn partial_admission_drain_blocks_further_callbacks_and_terminal_completion() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        {
            let mut admission = timeline.plan_callback(block(0, 100), 200, 2).unwrap();
            assert!(matches!(
                admission.next(),
                Some(CaptureDescriptor::Epoch(_))
            ));
        }
        assert!(matches!(
            timeline.plan_callback(block(100, 100), 200, 2),
            Err(AccountingError::AdmissionNotDrained)
        ));
        assert!(matches!(
            timeline.finish(),
            Err(AccountingError::AdmissionNotDrained)
        ));
        assert_eq!(timeline.next_frame, 100);
    }

    #[test]
    fn forward_sample_jump_is_old_epoch_loss_followed_by_new_mapping() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        let descriptors = accepted(&mut timeline, block(300, 100));
        let [
            CaptureDescriptor::Loss(loss),
            CaptureDescriptor::Epoch(epoch),
            CaptureDescriptor::Audio(audio),
        ] = descriptors.as_slice()
        else {
            panic!("expected host loss then epoch then audio");
        };
        assert_eq!(loss.epoch, 0);
        assert_eq!((loss.start_frame, loss.end_frame), (100, 300));
        assert_eq!(loss.cause, QualityCause::HostDiscontinuity);
        assert_eq!(epoch.id, 1);
        assert_eq!(epoch.source_frame_base, 300);
        assert_eq!(epoch.source_start_ns, Some(6_250_000));
        assert_eq!(epoch.reason, CaptureEpochReason::HostDiscontinuity);
        assert_eq!((audio.epoch, audio.start_frame), (1, 300));
        assert_eq!(timeline.dropped_counts().frames, 0);
    }

    fn check_host_only_forward_gap(sample_valid: bool, resumed_host_ns: u64) {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        let mut record = QualityRecord::observed();
        let mut retained = 0;
        let mut first = block(0, 48_000);
        first.timestamp.sample_time_valid = sample_valid;
        apply_descriptors(&mut record, &mut retained, &accepted(&mut timeline, first));
        let mut resumed = block(48_000, 48_000);
        resumed.timestamp.sample_time_valid = sample_valid;
        resumed.timestamp.source_offset_ns = resumed_host_ns;
        let descriptors = accepted(&mut timeline, resumed);
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
            CaptureDescriptor::Epoch(epoch) if epoch.reason == CaptureEpochReason::HostDiscontinuity
                && epoch.source_start_ns == Some(resumed_host_ns) && epoch.source_frame_base == 48_000)),
            "host-only gap must preserve the later source offset: {descriptors:?}");
        assert!(
            descriptors
                .iter()
                .all(|descriptor| !matches!(descriptor, CaptureDescriptor::Loss(_))),
            "host time alone must not invent an exact missing-frame interval"
        );
        apply_descriptors(&mut record, &mut retained, &descriptors);
        record.validate().unwrap();
        assert_eq!(
            retained, 96_000,
            "host-only gap must not request fabricated fill"
        );
        assert_eq!(record.epochs.len(), 2);
        assert_eq!(
            record.epochs[1].source_time_ns(96_000).unwrap(),
            Some(resumed_host_ns + 1_000_000_000)
        );
        assert_eq!(
            record.overflow.omitted_spans[QualityClass::SourceLoss.index()],
            1
        );
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
        assert_eq!(timeline.dropped_counts().frames, 0);
    }

    #[test]
    fn host_gap_with_invalid_sample_clock_preserves_later_source_time_and_unknown_loss() {
        check_host_only_forward_gap(false, 2_000_000_000);
    }

    #[test]
    fn host_gap_with_continuous_sample_clock_preserves_later_source_time_and_unknown_loss() {
        check_host_only_forward_gap(true, 2_000_000_000);
    }

    #[test]
    fn host_sleep_jump_never_expands_retained_pcm_or_hides_source_loss() {
        check_host_only_forward_gap(false, 3_600_000_000_000);
    }

    #[test]
    fn backward_host_only_jump_records_unknown_mapping_without_fabricated_frames() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        let mut first = block(0, 48_000);
        first.timestamp.source_offset_ns = 1_000_000_000;
        accepted(&mut timeline, first);
        let mut backwards = block(48_000, 48_000);
        backwards.timestamp.source_offset_ns = 500_000_000;
        let descriptors = accepted(&mut timeline, backwards);
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
            CaptureDescriptor::Epoch(epoch) if epoch.reason == CaptureEpochReason::HostDiscontinuity
                && epoch.source_start_ns.is_none() && epoch.source_frame_base == 48_000)));
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
            CaptureDescriptor::Overflow(overflow) if overflow.omitted_source_spans == 1)));
        assert!(
            descriptors
                .iter()
                .all(|descriptor| !matches!(descriptor, CaptureDescriptor::Loss(_)))
        );
        let mut continued = block(96_000, 48_000);
        continued.timestamp.source_offset_ns = 1_500_000_000;
        let next = accepted(&mut timeline, continued);
        assert!(matches!(next.as_slice(), [CaptureDescriptor::Audio(_)]));
        assert_eq!(timeline.current.unwrap().source_start_ns, None);
    }

    #[test]
    fn host_residual_tolerance_is_exactly_one_source_frame_or_less() {
        for rate in [44_100, 48_000, 192_000] {
            let duration = 137 * 1_000_000_000_u64 / u64::from(rate);
            let tolerance = 1_000_000_000_u64 / u64::from(rate);
            for residual in [tolerance, tolerance + 1] {
                let mut timeline = CaptureTimeline::<64>::new().unwrap();
                let mut first = block(0, 137);
                first.format.source_rate_hz = rate;
                accepted(&mut timeline, first);
                let mut next = block(137, 137);
                next.format = first.format;
                next.timestamp.source_offset_ns = duration + residual;
                let descriptors = accepted(&mut timeline, next);
                if residual == tolerance {
                    assert!(matches!(
                        descriptors.as_slice(),
                        [CaptureDescriptor::Audio(_)]
                    ));
                } else {
                    assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
                        CaptureDescriptor::Overflow(overflow) if overflow.omitted_source_spans == 1)));
                }
            }
        }
    }

    #[test]
    fn bounded_host_quantization_jitter_and_long_drift_do_not_accumulate_into_loss() {
        for rate in [44_100, 48_000] {
            for drift in [-100_i64, 100] {
                let mut timeline = CaptureTimeline::<64>::new().unwrap();
                let duration = 137 * 1_000_000_000_u64 / u64::from(rate);
                let mut host = 0;
                for index in 0..2_000 {
                    let mut input = block(index * 137, 137);
                    input.format.source_rate_hz = rate;
                    input.timestamp.source_offset_ns = host;
                    let descriptors = accepted(&mut timeline, input);
                    if index > 0 {
                        assert!(
                            matches!(descriptors.as_slice(), [CaptureDescriptor::Audio(_)]),
                            "bounded consecutive drift became cumulative loss: {descriptors:?}"
                        );
                    }
                    let jitter = if index % 2 == 0 { 1 } else { -1 };
                    host = (host as i64 + duration as i64 + drift + jitter) as u64;
                }
                assert_eq!(timeline.current.unwrap().id, 0);
                assert!(timeline.finish().unwrap().next().is_none());
            }
        }
    }

    #[test]
    fn matching_sample_and_host_gap_is_counted_once_as_exact_source_frames() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 48_000));
        let descriptors = accepted(&mut timeline, block(144_000, 48_000));
        let [
            CaptureDescriptor::Loss(loss),
            CaptureDescriptor::Epoch(epoch),
            CaptureDescriptor::Audio(audio),
        ] = descriptors.as_slice()
        else {
            panic!("matching clocks must retain only the exact sample gap: {descriptors:?}");
        };
        assert_eq!((loss.start_frame, loss.end_frame), (48_000, 144_000));
        assert_eq!(epoch.source_start_ns, Some(3_000_000_000));
        assert_eq!(audio.start_frame, 144_000);
        assert_eq!(
            LossFillPlan::for_gap(96_000, 48_000)
                .unwrap()
                .fill_source_frames,
            96_000
        );
    }

    #[test]
    fn invalid_host_flag_between_valid_observations_does_not_hide_a_later_jump() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 48_000));
        let mut unobserved = block(48_000, 48_000);
        unobserved.timestamp.host_time_valid = false;
        unobserved.timestamp.source_offset_ns = u64::MAX;
        assert!(matches!(
            accepted(&mut timeline, unobserved).as_slice(),
            [CaptureDescriptor::Audio(_)]
        ));
        let mut later = block(96_000, 48_000);
        later.timestamp.source_offset_ns = 100_000_000_000;
        let descriptors = accepted(&mut timeline, later);
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
            CaptureDescriptor::Epoch(epoch) if epoch.source_start_ns == Some(100_000_000_000)
                && epoch.source_frame_base == 96_000)));
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
            CaptureDescriptor::Overflow(overflow) if overflow.omitted_source_spans == 1)));
        assert!(
            descriptors
                .iter()
                .all(|descriptor| !matches!(descriptor, CaptureDescriptor::Loss(_)))
        );
    }

    #[test]
    fn invalid_host_callback_advances_existing_anchor_through_an_exact_sample_gap() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 48_000));
        let mut unobserved = block(144_000, 48_000);
        unobserved.timestamp.host_time_valid = false;
        unobserved.timestamp.source_offset_ns = u64::MAX;
        let descriptors = accepted(&mut timeline, unobserved);
        assert!(matches!(
            descriptors.as_slice(),
            [
                CaptureDescriptor::Loss(_),
                CaptureDescriptor::Epoch(_),
                CaptureDescriptor::Audio(_)
            ]
        ));
        let resumed = accepted(&mut timeline, block(192_000, 48_000));
        assert!(
            matches!(resumed.as_slice(), [CaptureDescriptor::Audio(_)]),
            "sample gap was counted twice after invalid host flag: {resumed:?}"
        );
    }

    #[test]
    fn absent_host_flags_preserve_fractional_duration_without_rounding_drift() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        for index in 0..100_001 {
            let mut input = block(index, 1);
            input.format.source_rate_hz = 44_100;
            input.timestamp.host_time_valid = index == 0 || index == 100_000;
            input.timestamp.source_offset_ns = index * 1_000_000_000 / 44_100;
            let descriptors = accepted(&mut timeline, input);
            if index > 0 {
                assert!(
                    matches!(descriptors.as_slice(), [CaptureDescriptor::Audio(_)]),
                    "fractional duration created a host jump: {descriptors:?}"
                );
            }
        }
        assert_eq!(timeline.current.unwrap().id, 0);
        assert!(timeline.finish().unwrap().next().is_none());
    }

    #[test]
    fn rate_transition_uses_old_callback_host_duration_and_does_not_reinterpret_sample_clock() {
        for changed_sample in [0, 48_000] {
            let mut timeline = CaptureTimeline::<64>::new().unwrap();
            accepted(&mut timeline, block(0, 48_000));
            let mut changed = block(48_000, 44_100);
            changed.format.source_rate_hz = 44_100;
            changed.format.generation = 1;
            changed.timestamp.sample_time = changed_sample as f64;
            changed.timestamp.source_offset_ns = 1_000_000_000;
            let descriptors = accepted(&mut timeline, changed);
            assert!(
                descriptors
                    .iter()
                    .all(|descriptor| !matches!(descriptor, CaptureDescriptor::Loss(_)))
            );
            assert!(descriptors.iter().all(|descriptor| !matches!(descriptor,
                CaptureDescriptor::Overflow(overflow) if overflow.omitted_source_spans != 0)));
            assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
                CaptureDescriptor::Epoch(epoch) if epoch.reason == CaptureEpochReason::RateChange
                    && epoch.source_start_ns == Some(1_000_000_000))));
            let mut next = block(92_100, 44_100);
            next.format = changed.format;
            next.timestamp.sample_time = (changed_sample + 44_100) as f64;
            next.timestamp.source_offset_ns = 2_000_000_000;
            assert!(matches!(
                accepted(&mut timeline, next).as_slice(),
                [CaptureDescriptor::Audio(_)]
            ));
        }
    }

    #[test]
    fn rate_transition_with_invalid_sample_clock_reanchors_without_fabricated_loss() {
        for (old_rate, new_rate) in [(44_100, 48_000), (48_000, 44_100)] {
            for (sample_time, sample_time_valid) in [
                (0.0, false),
                (1_000_000.0, false),
                (f64::NAN, true),
                (f64::INFINITY, true),
                (0.5, true),
                (9_007_199_254_740_994.0, true),
            ] {
                for host_valid in [true, false] {
                    for invalid_callbacks in [1, 3] {
                        for reset_sample in [false, true] {
                            let old_frames = u64::from(old_rate / 100);
                            let new_frames = u64::from(new_rate / 100);
                            let mut timeline = CaptureTimeline::<64>::new().unwrap();
                            let mut first = block(0, old_frames);
                            first.format.source_rate_hz = old_rate;
                            accepted(&mut timeline, first);
                            let mut changed = block(old_frames, new_frames);
                            changed.format.source_rate_hz = new_rate;
                            changed.format.generation = 1;
                            changed.timestamp.sample_time = sample_time;
                            changed.timestamp.sample_time_valid = sample_time_valid;
                            changed.timestamp.host_time_valid = host_valid;
                            for index in 0..invalid_callbacks {
                                changed.timestamp.source_offset_ns = if host_valid {
                                    (index + 1) * 10_000_000
                                } else {
                                    u64::MAX
                                };
                                let descriptors = accepted(&mut timeline, changed);
                                if index == 0 {
                                    let [
                                        CaptureDescriptor::Epoch(epoch),
                                        CaptureDescriptor::Audio(_),
                                    ] = descriptors.as_slice()
                                    else {
                                        panic!(
                                            "expected a loss-free rate transition: {descriptors:?}"
                                        );
                                    };
                                    assert_eq!(epoch.reason, CaptureEpochReason::RateChange);
                                    assert_eq!(epoch.source_start_ns, Some(10_000_000));
                                } else {
                                    assert!(matches!(
                                        descriptors.as_slice(),
                                        [CaptureDescriptor::Audio(_)]
                                    ));
                                }
                            }
                            let source_frame = old_frames + invalid_callbacks * new_frames;
                            let source_ns = (invalid_callbacks + 1) * 10_000_000;
                            let resumed_sample = if reset_sample {
                                0
                            } else {
                                (invalid_callbacks + 1) * new_frames
                            };
                            let mut resumed = block(source_frame, new_frames);
                            resumed.format = changed.format;
                            resumed.timestamp.sample_time = resumed_sample as f64;
                            resumed.timestamp.source_offset_ns = source_ns;
                            let descriptors = accepted(&mut timeline, resumed);
                            assert!(
                                matches!(descriptors.as_slice(), [CaptureDescriptor::Audio(audio)]
                                if audio.start_frame == source_frame && audio.epoch == 1),
                                "invalid sample across {old_rate}->{new_rate} fabricated loss: {descriptors:?}"
                            );
                            assert_eq!(
                                timeline.current.unwrap().time_at(source_frame).unwrap(),
                                Some(source_ns)
                            );
                            resumed.timestamp.sample_time += new_frames as f64;
                            resumed.timestamp.source_offset_ns += 10_000_000;
                            assert!(matches!(
                                accepted(&mut timeline, resumed).as_slice(),
                                [CaptureDescriptor::Audio(_)]
                            ));
                            assert_eq!(timeline.dropped_counts(), DroppedCounts::default());
                            assert!(timeline.finish().unwrap().next().is_none());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn reanchored_sample_clock_after_rate_transition_preserves_later_exact_gap() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        let mut first = block(0, 441);
        first.format.source_rate_hz = 44_100;
        accepted(&mut timeline, first);
        let mut changed = block(441, 480);
        changed.format.generation = 1;
        changed.timestamp.sample_time_valid = false;
        changed.timestamp.source_offset_ns = 10_000_000;
        accepted(&mut timeline, changed);
        let mut resumed = block(960, 480);
        resumed.format = changed.format;
        resumed.timestamp.source_offset_ns = 20_000_000;
        accepted(&mut timeline, resumed);
        resumed.timestamp.sample_time = 1_920.0;
        resumed.timestamp.source_offset_ns = 40_000_000;
        let descriptors = accepted(&mut timeline, resumed);
        let [
            CaptureDescriptor::Loss(loss),
            CaptureDescriptor::Epoch(epoch),
            CaptureDescriptor::Audio(audio),
        ] = descriptors.as_slice()
        else {
            panic!("expected the exact gap after reanchoring: {descriptors:?}");
        };
        assert_eq!((loss.start_frame, loss.end_frame), (1_401, 1_881));
        assert_eq!(loss.cause, QualityCause::HostDiscontinuity);
        assert_eq!(epoch.source_start_ns, Some(40_000_000));
        assert_eq!(audio.start_frame, 1_881);
    }

    #[test]
    fn invalid_sample_rate_transition_preserves_real_host_gap_and_offset() {
        for (gap_at_transition, transition_host_valid) in
            [(true, true), (false, true), (false, false)]
        {
            let mut timeline = CaptureTimeline::<64>::new().unwrap();
            let mut first = block(0, 441);
            first.format.source_rate_hz = 44_100;
            accepted(&mut timeline, first);
            let mut changed = block(441, 480);
            changed.format.generation = 1;
            changed.timestamp.sample_time_valid = false;
            changed.timestamp.host_time_valid = transition_host_valid;
            changed.timestamp.source_offset_ns = if !transition_host_valid {
                u64::MAX
            } else if gap_at_transition {
                20_000_000
            } else {
                10_000_000
            };
            let mut descriptors = accepted(&mut timeline, changed);
            let mut resumed = block(960, 480);
            resumed.format = changed.format;
            resumed.timestamp.source_offset_ns = 30_000_000;
            descriptors.extend(accepted(&mut timeline, resumed));
            assert!(
                descriptors
                    .iter()
                    .all(|descriptor| !matches!(descriptor, CaptureDescriptor::Loss(_))),
                "host-only gap must not invent an exact interval: {descriptors:?}"
            );
            let omitted: u64 = descriptors
                .iter()
                .filter_map(|descriptor| match descriptor {
                    CaptureDescriptor::Overflow(overflow) => Some(overflow.omitted_source_spans),
                    _ => None,
                })
                .sum();
            assert_eq!(omitted, 1);
            assert_eq!(
                timeline.current.unwrap().time_at(921).unwrap(),
                Some(30_000_000)
            );
            assert_eq!(timeline.next_frame, 1_401);
        }
    }

    #[test]
    fn same_rate_invalid_sample_clock_keeps_anchor_across_layout_change() {
        for layout_changed in [true, false] {
            let mut timeline = CaptureTimeline::<64>::new().unwrap();
            accepted(&mut timeline, block(0, 480));
            let mut unobserved = block(480, 480);
            unobserved.format.generation = u64::from(layout_changed);
            unobserved.timestamp.sample_time = 1_000_000.0;
            unobserved.timestamp.sample_time_valid = false;
            unobserved.timestamp.source_offset_ns = u64::MAX;
            unobserved.timestamp.host_time_valid = false;
            accepted(&mut timeline, unobserved);
            let mut resumed = block(1_440, 480);
            resumed.format = unobserved.format;
            let descriptors = accepted(&mut timeline, resumed);
            let [
                CaptureDescriptor::Loss(loss),
                CaptureDescriptor::Epoch(epoch),
                CaptureDescriptor::Audio(audio),
            ] = descriptors.as_slice()
            else {
                panic!("same-rate anchor must retain the exact gap: {descriptors:?}");
            };
            assert_eq!((loss.start_frame, loss.end_frame), (960, 1_440));
            assert_eq!(epoch.source_start_ns, Some(30_000_000));
            assert_eq!(audio.start_frame, 1_440);
        }
    }

    #[test]
    fn rate_transition_with_real_host_gap_keeps_actual_offset_without_guessed_sample_gap() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 48_000));
        let mut changed = block(48_000, 16_000);
        changed.format.source_rate_hz = 16_000;
        changed.format.generation = 1;
        changed.timestamp.sample_time = 123_456.0;
        changed.timestamp.source_offset_ns = 2_000_000_000;
        let descriptors = accepted(&mut timeline, changed);
        assert!(
            descriptors
                .iter()
                .all(|descriptor| !matches!(descriptor, CaptureDescriptor::Loss(_)))
        );
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
            CaptureDescriptor::Epoch(epoch) if epoch.source_start_ns == Some(2_000_000_000)
                && epoch.source_frame_base == 48_000 && epoch.reason == CaptureEpochReason::RateChange)));
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
            CaptureDescriptor::Overflow(overflow) if overflow.omitted_source_spans == 1)));
        assert_eq!(timeline.next_frame, 64_000);
    }

    #[test]
    fn first_valid_host_observation_does_not_retroactively_locate_an_unknown_epoch() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        let mut first = block(0, 48_000);
        first.timestamp.host_time_valid = false;
        first.timestamp.source_offset_ns = u64::MAX;
        accepted(&mut timeline, first);
        let mut observed = block(48_000, 48_000);
        observed.timestamp.source_offset_ns = 9_000_000_000;
        assert!(matches!(
            accepted(&mut timeline, observed).as_slice(),
            [CaptureDescriptor::Audio(_)]
        ));
        assert_eq!(timeline.current.unwrap().source_start_ns, None);
        let mut later = block(96_000, 48_000);
        later.timestamp.source_offset_ns = 11_000_000_000;
        let descriptors = accepted(&mut timeline, later);
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
            CaptureDescriptor::Epoch(epoch) if epoch.source_start_ns == Some(11_000_000_000)
                && epoch.source_frame_base == 96_000)));
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor,
            CaptureDescriptor::Overflow(overflow) if overflow.omitted_source_spans == 1)));
    }

    #[test]
    fn rejected_host_jump_and_pending_saturation_keep_loss_sticky_and_resume_at_actual_time() {
        let mut timeline = CaptureTimeline::<2>::new().unwrap();
        let mut record = QualityRecord::observed();
        let mut retained = 0;
        apply_descriptors(
            &mut record,
            &mut retained,
            &accepted(&mut timeline, block(0, 48_000)),
        );
        for (frame, host) in [(48_000, 2_000_000_000), (96_000, 4_000_000_000)] {
            let mut input = block(frame, 48_000);
            input.timestamp.source_offset_ns = host;
            rejected(&mut timeline, input, 0, 0);
        }
        assert_eq!(timeline.pending_len, 2);
        let mut resumed = block(144_000, 48_000);
        resumed.timestamp.source_offset_ns = 5_000_000_000;
        let descriptors = accepted(&mut timeline, resumed);
        apply_descriptors(&mut record, &mut retained, &descriptors);
        record.validate().unwrap();
        assert_eq!(
            record.epochs.last().unwrap().source_start_ns,
            Some(5_000_000_000)
        );
        assert_eq!(record.epochs.last().unwrap().source_frame_base, 144_000);
        assert_eq!(record.overflow.omitted_epochs, 1);
        assert_eq!(
            record.overflow.omitted_spans[QualityClass::SourceLoss.index()],
            3
        );
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
        assert_eq!(timeline.dropped_counts().frames, 96_000);
    }

    #[test]
    fn invalid_timestamp_flags_and_values_never_fabricate_a_jump_or_offset() {
        for sample_time in [f64::NAN, f64::INFINITY, 0.5, 9_007_199_254_740_994.0] {
            let mut timeline = CaptureTimeline::<64>::new().unwrap();
            let mut input = block(0, 100);
            input.timestamp.sample_time = sample_time;
            input.timestamp.host_time_valid = false;
            let first = accepted(&mut timeline, input);
            let CaptureDescriptor::Epoch(epoch) = first[0] else {
                unreachable!()
            };
            assert_eq!(epoch.source_start_ns, None);
            input.timestamp.sample_time = -1_000_000.0;
            input.timestamp.sample_time_valid = false;
            let next = accepted(&mut timeline, input);
            assert_eq!(next.len(), 1);
            let CaptureDescriptor::Audio(audio) = next[0] else {
                unreachable!()
            };
            assert_eq!(audio.start_frame, 100);
            assert_eq!(audio.epoch, 0);
        }
    }

    #[test]
    fn backwards_sample_clock_records_unknown_source_loss_without_inventing_interval() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        let mut input = block(0, 100);
        input.timestamp.host_time_valid = false;
        let descriptors = accepted(&mut timeline, input);
        assert!(
            descriptors
                .iter()
                .all(|descriptor| !matches!(descriptor, CaptureDescriptor::Loss(_)))
        );
        let epoch = descriptors
            .iter()
            .find_map(|descriptor| match descriptor {
                CaptureDescriptor::Epoch(epoch) => Some(epoch),
                _ => None,
            })
            .unwrap();
        assert_eq!(epoch.source_start_ns, None);
        let overflow = descriptors
            .iter()
            .find_map(|descriptor| match descriptor {
                CaptureDescriptor::Overflow(overflow) => Some(overflow),
                _ => None,
            })
            .unwrap();
        assert_eq!(overflow.omitted_source_spans, 1);
    }

    #[test]
    fn layout_and_rate_epochs_bind_the_accepted_block_including_first_block() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        let mut first = block(0, 100);
        first.format.mic_channels = 0;
        first.format.tap_channels = 2;
        let descriptors = accepted(&mut timeline, first);
        let CaptureDescriptor::Audio(audio) = descriptors[1] else {
            unreachable!()
        };
        assert_eq!(audio.format, first.format);
        let mut layout = block(100, 100);
        layout.format.generation = 1;
        let descriptors = accepted(&mut timeline, layout);
        let CaptureDescriptor::Epoch(epoch) = descriptors[0] else {
            unreachable!()
        };
        assert_eq!(epoch.reason, CaptureEpochReason::LayoutChange);
        assert_eq!(epoch.id, 1);
        let mut rate = block(200, 100);
        rate.format.generation = 2;
        rate.format.source_rate_hz = 44_100;
        let descriptors = accepted(&mut timeline, rate);
        let CaptureDescriptor::Epoch(epoch) = descriptors[0] else {
            unreachable!()
        };
        let CaptureDescriptor::Audio(audio) = descriptors[1] else {
            unreachable!()
        };
        assert_eq!(epoch.reason, CaptureEpochReason::RateChange);
        assert_eq!(epoch.id, 2);
        assert_eq!(audio.format.source_rate_hz, 44_100);
        let mapped = epoch.with_retained_base(200, 48_000).unwrap();
        assert_eq!(mapped.retained_rate_hz, 48_000);
        assert_eq!(mapped.retained_start_frame, 200);
    }

    #[test]
    fn descriptor_saturation_across_more_than_64_epochs_does_not_relabel_stale_format() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        for generation in 0..80 {
            let mut input = block(generation * 100, 100);
            input.format.generation = generation;
            input.format.source_rate_hz = if generation % 2 == 0 { 48_000 } else { 44_100 };
            input.format.tap_channels = if generation % 2 == 0 { 1 } else { 2 };
            input.timestamp.host_time_valid = generation == 0;
            rejected(&mut timeline, input, usize::MAX, 0);
        }
        assert_eq!(timeline.pending_len, 64);
        let mut final_input = block(8_000, 100);
        final_input.format.generation = 80;
        final_input.format.tap_channels = 4;
        final_input.timestamp.host_time_valid = false;
        let descriptors = accepted(&mut timeline, final_input);
        assert_eq!(descriptors.len(), 67);
        let CaptureDescriptor::Overflow(overflow) = descriptors[64] else {
            unreachable!()
        };
        let CaptureDescriptor::Epoch(epoch) = descriptors[65] else {
            unreachable!()
        };
        let CaptureDescriptor::Audio(audio) = descriptors[66] else {
            unreachable!()
        };
        assert_eq!(overflow.omitted_epochs, 49);
        assert_eq!(overflow.omitted_source_spans, 48);
        assert_eq!(epoch.id, 81);
        assert_eq!(epoch.format, final_input.format);
        assert_eq!(audio.format, final_input.format);
        assert_eq!(audio.epoch, epoch.id);
        assert_eq!(audio.start_frame, 8_000);
        assert_eq!(audio.samples, 500);
        let mut record = QualityRecord::observed();
        overflow.apply_to(&mut record).unwrap();
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn large_gap_emits_new_epoch_and_never_requests_more_than_ten_seconds_fill() {
        assert_eq!(
            LossFillPlan::for_gap(480_000, 48_000).unwrap(),
            LossFillPlan {
                fill_source_frames: 480_000,
                epoch_break: false
            }
        );
        assert_eq!(
            LossFillPlan::for_gap(480_001, 48_000).unwrap(),
            LossFillPlan {
                fill_source_frames: 0,
                epoch_break: true
            }
        );
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        rejected(
            &mut timeline,
            block(100, 960_000),
            0,
            HAL_DESCRIPTOR_CAPACITY,
        );
        let descriptors = accepted(&mut timeline, block(960_100, 100));
        let [
            CaptureDescriptor::Loss(loss),
            CaptureDescriptor::Epoch(epoch),
            CaptureDescriptor::Audio(audio),
        ] = descriptors.as_slice()
        else {
            panic!("expected large loss followed by remapped epoch");
        };
        assert_eq!(loss.end_frame - loss.start_frame, 960_000);
        assert_eq!(epoch.source_frame_base, 960_100);
        assert_eq!(epoch.source_start_ns, Some(20_002_083_333));
        assert_eq!(audio.epoch, epoch.id);
        assert_eq!(
            epoch
                .with_retained_base(100, 48_000)
                .unwrap()
                .retained_start_frame,
            100
        );
    }

    #[test]
    fn terminal_drain_keeps_last_loss_and_prohibits_restart() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        rejected(&mut timeline, block(100, 100), 0, 0);
        let terminal: Vec<_> = timeline.finish().unwrap().collect();
        let [CaptureDescriptor::Loss(loss)] = terminal.as_slice() else {
            unreachable!()
        };
        assert_eq!((loss.start_frame, loss.end_frame), (100, 200));
        assert!(timeline.finish().unwrap().next().is_none());
        assert!(matches!(
            timeline.plan_callback(block(200, 100), 200, 2),
            Err(AccountingError::Stopped)
        ));
    }

    #[test]
    fn unknown_format_is_dropped_and_following_audio_gets_fresh_mapping() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        timeline.record_unknown_format_loss(100, 700).unwrap();
        let descriptors = accepted(&mut timeline, block(200, 100));
        let [
            CaptureDescriptor::Epoch(epoch),
            CaptureDescriptor::Overflow(overflow),
            CaptureDescriptor::Audio(audio),
        ] = descriptors.as_slice()
        else {
            panic!("expected unknown loss summary and fresh mapping");
        };
        assert_eq!(overflow.omitted_source_spans, 1);
        assert_eq!(overflow.omitted_epochs, 1);
        assert_eq!(epoch.source_start_ns, Some(4_166_666));
        assert_eq!(audio.start_frame, 200);
        assert_eq!(timeline.dropped_counts().samples, 700);

        timeline.record_unknown_format_loss(100, 700).unwrap();
        let mut input = block(400, 100);
        input.timestamp.host_time_valid = false;
        let descriptors = accepted(&mut timeline, input);
        assert!(descriptors.iter().any(|descriptor| matches!(descriptor, CaptureDescriptor::Epoch(epoch) if epoch.source_start_ns.is_none())));
    }

    fn apply_descriptors(
        record: &mut QualityRecord,
        retained_frame: &mut u64,
        descriptors: &[CaptureDescriptor],
    ) {
        for descriptor in descriptors {
            match descriptor {
                CaptureDescriptor::Epoch(epoch) => {
                    record
                        .insert_epoch(epoch.with_retained_base(*retained_frame, 48_000).unwrap())
                        .unwrap();
                }
                CaptureDescriptor::Loss(loss) => {
                    record.insert_span(*loss).unwrap();
                    *retained_frame += loss.end_frame - loss.start_frame;
                }
                CaptureDescriptor::Overflow(overflow) => overflow.apply_to(record).unwrap(),
                CaptureDescriptor::Audio(audio) => *retained_frame += audio.frames,
            }
        }
    }

    #[test]
    fn rejected_known_format_after_unknown_interval_has_valid_source_epoch() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        let mut record = QualityRecord::observed();
        let mut retained_frame = 0;
        let first = accepted(&mut timeline, block(0, 100));
        apply_descriptors(&mut record, &mut retained_frame, &first);
        // The unobserved format was actually 96 kHz: these 100 frames took 1.041666... ms.
        timeline.record_unknown_format_loss(100, 200).unwrap();
        let mut lost = block(200, 100);
        lost.timestamp.source_offset_ns = 3_125_000;
        rejected(&mut timeline, lost, 0, 0);
        let mut resumed = block(300, 100);
        resumed.timestamp.source_offset_ns = 5_208_333;
        let descriptors = accepted(&mut timeline, resumed);
        apply_descriptors(&mut record, &mut retained_frame, &descriptors);
        record.validate().unwrap();
        let loss = record.spans.last().unwrap();
        let epoch = record
            .epochs
            .iter()
            .find(|epoch| epoch.id == loss.epoch)
            .unwrap();
        assert_eq!((loss.start_frame, loss.end_frame), (200, 300));
        assert_eq!(
            epoch.source_time_ns(loss.start_frame).unwrap(),
            Some(3_125_000)
        );
        assert_eq!(
            epoch.source_time_ns(loss.end_frame).unwrap(),
            Some(5_208_333)
        );
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn terminal_rejection_after_unknown_format_never_uses_the_prior_epoch() {
        for host_valid in [true, false] {
            let mut timeline = CaptureTimeline::<64>::new().unwrap();
            accepted(&mut timeline, block(0, 100));
            timeline.record_unknown_format_loss(100, 200).unwrap();
            let mut lost = block(200, 100);
            lost.timestamp.source_offset_ns = 3_125_000;
            lost.timestamp.host_time_valid = host_valid;
            rejected(&mut timeline, lost, 0, 0);
            let descriptors: Vec<_> = timeline.finish().unwrap().collect();
            let loss = descriptors
                .iter()
                .find_map(|descriptor| match descriptor {
                    CaptureDescriptor::Loss(loss) => Some(loss),
                    _ => None,
                })
                .unwrap();
            assert_ne!(loss.epoch, 0);
            let epoch = descriptors
                .iter()
                .find_map(|descriptor| match descriptor {
                    CaptureDescriptor::Epoch(epoch) if epoch.id == loss.epoch => Some(epoch),
                    _ => None,
                })
                .unwrap();
            assert_eq!(epoch.source_frame_base, 200);
            assert_eq!(epoch.source_start_ns, host_valid.then_some(3_125_000));
        }
    }

    #[test]
    fn unknown_format_rebinding_with_full_pending_storage_summarizes_unmapped_loss() {
        let mut timeline = CaptureTimeline::<2>::new().unwrap();
        let mut record = QualityRecord::observed();
        let mut retained_frame = 0;
        let first = accepted(&mut timeline, block(0, 100));
        apply_descriptors(&mut record, &mut retained_frame, &first);
        timeline.record_unknown_format_loss(100, 200).unwrap();
        let mut first_lost = block(200, 100);
        first_lost.timestamp.source_offset_ns = 3_125_000;
        rejected(&mut timeline, first_lost, 0, 0);
        assert_eq!(timeline.pending_len, 2);
        timeline.record_unknown_format_loss(100, 200).unwrap();
        let mut second_lost = block(400, 100);
        second_lost.timestamp.source_offset_ns = 6_250_000;
        rejected(&mut timeline, second_lost, 0, 0);
        let mut resumed = block(500, 100);
        resumed.timestamp.source_offset_ns = 8_333_333;
        let descriptors = accepted(&mut timeline, resumed);
        apply_descriptors(&mut record, &mut retained_frame, &descriptors);
        record.validate().unwrap();
        assert_eq!(record.spans.len(), 1);
        assert_eq!(record.spans[0].epoch, 1);
        assert_eq!(record.epochs[0].source_start_ns, Some(0));
        assert_eq!(record.epochs.last().unwrap().source_frame_base, 500);
        assert_eq!(
            record.epochs.last().unwrap().source_start_ns,
            Some(8_333_333)
        );
        assert_eq!(record.overflow.omitted_epochs, 3);
        assert_eq!(
            record.overflow.omitted_spans[QualityClass::SourceLoss.index()],
            3
        );
        assert_eq!(record.source_completeness(), SourceCompleteness::Incomplete);
    }

    #[test]
    fn counts_saturate_and_epoch_ids_are_never_reused_after_exhaustion() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        timeline.dropped.frames = u64::MAX;
        rejected(&mut timeline, block(100, 100), 0, 0);
        assert_eq!(timeline.dropped_counts().frames, u64::MAX);
        assert!(timeline.dropped_counts().counter_saturated);
        let terminal: Vec<_> = timeline.finish().unwrap().collect();
        assert!(terminal.iter().any(|descriptor| matches!(descriptor, CaptureDescriptor::Overflow(overflow) if overflow.counter_saturated)));

        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        timeline.current.as_mut().unwrap().id = u32::MAX;
        let mut changed = block(100, 100);
        changed.format.generation = 1;
        assert!(matches!(
            timeline.plan_callback(changed, 200, 2),
            Err(AccountingError::EpochIdExhausted)
        ));
        assert!(timeline.finish().unwrap().any(|descriptor| matches!(descriptor, CaptureDescriptor::Overflow(overflow) if overflow.omitted_epochs > 0)));
    }

    #[test]
    fn fractional_mapping_is_chunk_invariant_and_unchanged_rate_is_identity() {
        for (source, retained) in [(44_100, 48_000), (48_000, 44_100), (48_000, 48_000)] {
            let mut one_frame = FractionalFrameMapper::new(source, retained).unwrap();
            for _ in 0..source {
                one_frame.push(1).unwrap();
            }
            let mut chunks = FractionalFrameMapper::new(source, retained).unwrap();
            let mut left = u64::from(source);
            while left > 0 {
                let frames = left.min(137);
                chunks.push(frames).unwrap();
                left -= frames;
            }
            assert_eq!(one_frame, chunks);
            assert_eq!(chunks.retained_frames(), u64::from(retained));
            assert_eq!(chunks.remainder(), 0);
        }
        let mut identity = FractionalFrameMapper::new(48_000, 48_000).unwrap();
        for frames in [0, 1, 137, 8_192, 1_000_000] {
            assert_eq!(identity.push(frames).unwrap(), frames);
        }
    }

    #[test]
    fn checked_mapping_overflow_is_transactional() {
        assert_eq!(
            FractionalFrameMapper::new(0, 48_000),
            Err(AccountingError::InvalidFormat)
        );
        let mut mapper = FractionalFrameMapper::new(1, u32::MAX).unwrap();
        let previous = mapper;
        assert_eq!(
            mapper.push(u64::MAX),
            Err(AccountingError::ArithmeticOverflow)
        );
        assert_eq!(mapper, previous);
        let mut mapper = FractionalFrameMapper::new(1, 1).unwrap();
        assert_eq!(mapper.push(u64::MAX).unwrap(), u64::MAX);
        let previous = mapper;
        assert_eq!(mapper.push(1), Err(AccountingError::ArithmeticOverflow));
        assert_eq!(mapper, previous);
        assert!(matches!(
            CaptureTimeline::<1>::new(),
            Err(AccountingError::InvalidCapacity)
        ));
        assert!(matches!(
            CaptureTimeline::<4094>::new(),
            Err(AccountingError::InvalidCapacity)
        ));
    }

    #[test]
    fn layout_transition_does_not_hide_a_simultaneous_sample_gap() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        let mut changed = block(300, 100);
        changed.format.generation = 1;
        changed.format.tap_channels = 2;
        let descriptors = accepted(&mut timeline, changed);
        let [
            CaptureDescriptor::Loss(loss),
            CaptureDescriptor::Epoch(epoch),
            CaptureDescriptor::Audio(audio),
        ] = descriptors.as_slice()
        else {
            panic!("expected gap despite concurrent layout transition");
        };
        assert_eq!((loss.start_frame, loss.end_frame), (100, 300));
        assert_eq!(loss.cause, QualityCause::HostDiscontinuity);
        assert_eq!(epoch.reason, CaptureEpochReason::LayoutChange);
        assert_eq!(audio.start_frame, 300);
        assert_eq!(audio.format.tap_channels, 2);
    }

    #[test]
    fn rate_clock_change_records_unknown_mapping_without_using_stale_frame_units() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        let mut changed = block(300, 100);
        changed.format.generation = 1;
        changed.format.source_rate_hz = 44_100;
        changed.timestamp.host_time_valid = false;
        let descriptors = accepted(&mut timeline, changed);
        assert!(
            descriptors
                .iter()
                .all(|descriptor| !matches!(descriptor, CaptureDescriptor::Loss(_)))
        );
        let mut record = QualityRecord::observed();
        for descriptor in descriptors {
            if let CaptureDescriptor::Overflow(overflow) = descriptor {
                assert_eq!(overflow.omitted_epochs, 1);
                assert_eq!(overflow.omitted_source_spans, 0);
                overflow.apply_to(&mut record).unwrap();
            }
            if let CaptureDescriptor::Epoch(epoch) = descriptor {
                assert_eq!(epoch.source_start_ns, None);
            }
        }
        assert_eq!(record.source_completeness(), SourceCompleteness::Unknown);
    }

    #[test]
    fn backward_valid_host_offsets_remain_unknown_across_sample_resets() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        let mut first = block(0, 100);
        first.timestamp.source_offset_ns = 1_000_000_000;
        accepted(&mut timeline, first);
        let reset = block(0, 100);
        let descriptors = accepted(&mut timeline, reset);
        let epoch = descriptors
            .iter()
            .find_map(|descriptor| match descriptor {
                CaptureDescriptor::Epoch(epoch) => Some(epoch),
                _ => None,
            })
            .unwrap();
        assert_eq!(epoch.source_start_ns, None);
        let mut layout = block(100, 100);
        layout.format.generation = 1;
        let descriptors = accepted(&mut timeline, layout);
        let CaptureDescriptor::Epoch(epoch) = descriptors[0] else {
            unreachable!()
        };
        assert_eq!(epoch.source_start_ns, None);
    }

    #[test]
    fn arithmetic_failure_stops_admission_and_preserves_terminal_unknown_loss() {
        for input in [
            CallbackInput {
                frames: u64::MAX,
                ..block(100, 100)
            },
            CallbackInput {
                timestamp: CaptureTimestamp {
                    source_offset_ns: u64::MAX,
                    ..block(0, 100).timestamp
                },
                ..block(0, 100)
            },
        ] {
            let mut timeline = CaptureTimeline::<64>::new().unwrap();
            assert!(matches!(
                timeline.plan_callback(input, usize::MAX, HAL_DESCRIPTOR_CAPACITY),
                Err(AccountingError::ArithmeticOverflow)
            ));
            assert!(matches!(
                timeline.plan_callback(block(0, 100), 200, 2),
                Err(AccountingError::Stopped)
            ));
            assert!(timeline.finish().unwrap().any(|descriptor| matches!(descriptor, CaptureDescriptor::Overflow(overflow) if overflow.omitted_source_spans > 0)));
        }
    }

    #[test]
    fn invalid_callback_format_cannot_resume_without_recording_source_loss() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        let mut invalid = block(100, 100);
        invalid.format.source_rate_hz = 0;
        assert!(matches!(
            timeline.plan_callback(invalid, 200, 2),
            Err(AccountingError::InvalidFormat)
        ));
        assert!(matches!(
            timeline.plan_callback(block(200, 100), 200, 2),
            Err(AccountingError::Stopped)
        ));
        assert_eq!(timeline.dropped_counts().samples, 200);
        assert!(timeline.finish().unwrap().any(|descriptor| matches!(descriptor, CaptureDescriptor::Overflow(overflow) if overflow.omitted_source_spans == 1)));
    }

    #[test]
    fn callback_state_and_descriptors_have_no_heap_owned_fields() {
        fn copy_descriptor<T: Copy>() {}
        copy_descriptor::<CaptureDescriptor>();
        assert!(!std::mem::needs_drop::<CaptureTimeline<64>>());
        assert!(
            std::mem::size_of::<CaptureTimeline<64>>()
                <= 67 * std::mem::size_of::<CaptureDescriptor>() + 256
        );
    }

    #[test]
    fn adjacent_loss_causes_share_one_fill_cap_without_coalescing_quality_markers() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        rejected(&mut timeline, block(100, 384_000), 0, 0);
        let descriptors = accepted(&mut timeline, block(768_100, 100));
        let [
            CaptureDescriptor::Loss(ring),
            CaptureDescriptor::Loss(host),
            CaptureDescriptor::Epoch(epoch),
            CaptureDescriptor::Audio(audio),
        ] = descriptors.as_slice()
        else {
            panic!("expected distinct loss causes sharing one unavailable run");
        };
        assert_eq!(ring.cause, QualityCause::RingOverflow);
        assert_eq!(host.cause, QualityCause::HostDiscontinuity);
        assert_eq!(ring.end_frame, host.start_frame);
        assert_eq!(host.end_frame, epoch.source_frame_base);
        let plan = LossFillPlan::for_gap(host.end_frame - ring.start_frame, 48_000).unwrap();
        assert_eq!(plan.fill_source_frames, 0);
        assert!(plan.epoch_break);
        assert_eq!(audio.epoch, epoch.id);
    }

    #[test]
    fn unavailable_run_crossing_rates_gets_new_mapping_before_later_audio() {
        let mut timeline = CaptureTimeline::<64>::new().unwrap();
        accepted(&mut timeline, block(0, 100));
        rejected(&mut timeline, block(100, 384_000), 0, 0);
        let mut changed = block(384_100, 192_000);
        changed.format.generation = 1;
        changed.format.source_rate_hz = 24_000;
        rejected(&mut timeline, changed, 0, 0);
        let mut later = block(576_100, 100);
        later.format = changed.format;
        later.timestamp.source_offset_ns = 16_002_083_333;
        let descriptors = accepted(&mut timeline, later);
        let [
            CaptureDescriptor::Loss(first),
            CaptureDescriptor::Epoch(middle),
            CaptureDescriptor::Loss(second),
            CaptureDescriptor::Epoch(resumed),
            CaptureDescriptor::Audio(audio),
        ] = descriptors.as_slice()
        else {
            panic!("expected fresh mapping after a multi-rate unavailable run");
        };
        assert_eq!(first.end_frame - first.start_frame, 384_000);
        assert_eq!(middle.format.source_rate_hz, 24_000);
        assert_eq!(second.end_frame - second.start_frame, 192_000);
        assert_eq!(resumed.id, 2);
        assert_eq!(resumed.source_start_ns, Some(16_002_083_333));
        assert_eq!(audio.epoch, resumed.id);
        let mapped = resumed.with_retained_base(100, 48_000).unwrap();
        assert_eq!(mapped.retained_frame(audio.start_frame).unwrap(), 100);
    }
}
