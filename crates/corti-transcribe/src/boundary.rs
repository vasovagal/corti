//! Pure scalar policy for live filing boundaries. The caller owns the source clock, PCM, native
//! state and filing. A selected source fence is neither a decoder flush nor proof of settlement.
//!
//! Natural cuts require native-certified whole-region ownership intervals after both channels have
//! been pushed and drained. Ordinary natural filing must preserve native VAD/resampler state. The
//! supported fixed source-rate range here is 8–192 kHz; this is not an arbitrary-rate proof.

use std::{fmt, time::Duration};

const MAX_ADMISSION_FRAMES: u64 = 4096;
const MAX_SPEECH_AND_SLACK: Duration = Duration::from_secs(21);

/// Invalid inputs fail before changing a due latch or selected fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryError {
    UnsupportedRate,
    EmptyWindow,
    Overflow,
    InvalidObservation,
    EmptyTextCut,
}

impl fmt::Display for BoundaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedRate => "boundary source rate must be between 8000 and 192000 Hz",
            Self::EmptyWindow => "boundary window must contain at least one source frame",
            Self::Overflow => "boundary frame arithmetic overflow",
            Self::InvalidObservation => "invalid boundary source observation",
            Self::EmptyTextCut => "text cap cannot select an empty source window",
        })
    }
}

impl std::error::Error for BoundaryError {}

/// Inclusive range of admissible source-frame cuts, not an audio span or word timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceInterval {
    pub first: u64,
    pub last: u64,
}

/// The caller translates native track-local positions using the existing track origin exactly once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackBoundary {
    /// No input has ever established this track. Temporarily absent established tracks are not this.
    NeverPresent,
    Established {
        /// First owned source position for this track in the current window (at least its origin).
        start_frame: u64,
        /// Actual source delivery to the channel; received input held by AEC does not count yet.
        delivered_through: u64,
        /// Supply only while native has neither an open nor a queued region AND proves ownership.
        /// The lower bound includes every attempted REGION end, including failed/empty attempts,
        /// independently of report eviction/draining. The upper bound proves no future region can
        /// start earlier. Boolean idle, dwell time, empty polls and word times are insufficient.
        natural: Option<SourceInterval>,
    },
}

impl TrackBoundary {
    fn validate(self, received: u64) -> Result<(), BoundaryError> {
        if let Self::Established {
            start_frame,
            delivered_through,
            natural,
        } = self
            && (start_frame > delivered_through
                || delivered_through > received
                || natural.is_some_and(|range| range.first > range.last))
        {
            return Err(BoundaryError::InvalidObservation);
        }
        Ok(())
    }

    fn covers(self, frame: u64) -> bool {
        match self {
            Self::NeverPresent => true,
            Self::Established {
                start_frame,
                delivered_through,
                ..
            } => frame <= start_frame || delivered_through >= frame,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForceReason {
    Deadline,
    /// Explicit resource escape: a full text cap cannot wait while accumulating more text.
    TextCap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    Natural,
    Forced(ForceReason),
}

/// Immutable ownership end once selected. Data beyond it belongs to the next window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryFence {
    pub frame: u64,
    pub kind: BoundaryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryStep {
    /// Always 1..=4096. Clip further to the available producer chunk before admission.
    Admit { frames: u64 },
    /// Admit zero more frames to THIS owned window. Latch a native forced fence before delivering
    /// later input, whose bounded reserve belongs to the next window. Repeated observations return
    /// the same fence; consume/file it once, then replace this policy for the next window.
    Selected {
        fence: BoundaryFence,
        /// Delivery coverage only. Even true does not certify native settlement or durable filing.
        /// At/before a late track's start there are no old-window samples for that track to deliver.
        delivery_complete: bool,
        /// Reset native state at this ordinary boundary using the separately proved maintenance
        /// settlement handshake. This never changes the fence kind/frame or proves reset success.
        /// The caller records actual maintenance degradation independently of the ordinary kind.
        reset_for_maintenance: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    /// Caller-owned monotonic received-source endpoint, distinct from either channel's delivery.
    pub received_through: u64,
    pub tracks: [TrackBoundary; 2],
    /// The caller must bound text BEFORE insertion; this flag does not authorize dropping output.
    /// Oversized/omitted output needs the existing processing-loss/fallback path separately.
    pub text_full: bool,
    /// Request native maintenance at the next ORDINARY selected boundary. The request is sticky;
    /// it never makes a window due or changes its admission, cut frame or kind. Arm early enough:
    /// caller/native capacity validation must cover the next nominal + deferral boundary and its
    /// delivery/settlement/replay/admission reserve, rejecting impossible capacity before admission.
    /// This policy derives no native capacity/rate math. A selected fence and reset flag are final.
    pub maintenance_armed: bool,
}

/// One window's fixed-size deterministic policy: no heap, clock, queue, PCM or native operations.
/// Nominal frames are the caller's UNCHANGED minimum of configured duration and existing audio cap.
/// Additional deferral/settlement storage must be reserved separately; this policy allocates none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryPolicy {
    start_frame: u64,
    nominal_end: u64,
    deferral_frames: u64,
    due_frame: Option<u64>,
    selected: Option<BoundaryFence>,
    maintenance_armed: bool,
}

impl BoundaryPolicy {
    /// Deferral is ceil(rate * (configured 20 s max-speech + actual min-silence + 1 s slack)),
    /// exactly 22 s at the current 1 s min-silence. The native 20 s setting is not a hard split.
    pub fn new(
        start_frame: u64,
        nominal_frames: u64,
        sample_rate: u32,
        min_silence: Duration,
    ) -> Result<Self, BoundaryError> {
        if !(8000..=192_000).contains(&sample_rate) {
            return Err(BoundaryError::UnsupportedRate);
        }
        if nominal_frames == 0 {
            return Err(BoundaryError::EmptyWindow);
        }
        let nanos = min_silence
            .checked_add(MAX_SPEECH_AND_SLACK)
            .ok_or(BoundaryError::Overflow)?
            .as_nanos();
        let deferral_frames = u64::try_from(
            nanos
                .checked_mul(u128::from(sample_rate))
                .ok_or(BoundaryError::Overflow)?
                .div_ceil(1_000_000_000),
        )
        .map_err(|_| BoundaryError::Overflow)?;
        let nominal_end = start_frame
            .checked_add(nominal_frames)
            .ok_or(BoundaryError::Overflow)?;
        nominal_end
            .checked_add(deferral_frames)
            .ok_or(BoundaryError::Overflow)?;
        Ok(Self {
            start_frame,
            nominal_end,
            deferral_frames,
            due_frame: None,
            selected: None,
            maintenance_armed: false,
        })
    }

    pub fn due_frame(&self) -> Option<u64> {
        self.due_frame
    }

    pub fn deferral_frames(&self) -> u64 {
        self.deferral_frames
    }

    /// Checked maximum old-window ownership, excluding separately bounded settlement/next PCM.
    /// Use this in allocation arithmetic, without reducing the caller's nominal window to fit.
    pub fn maximum_owned_frames(&self) -> u64 {
        // Construction already checked start + nominal + deferral.
        self.nominal_end - self.start_frame + self.deferral_frames
    }

    pub fn selected_fence(&self) -> Option<BoundaryFence> {
        self.selected
    }

    /// Observe only after both channels' available old polls/reports have been drained. Until a
    /// fence is selected, obey each admission limit; do not submit a large chunk past the deadline.
    /// After selection, further input may release AEC/native tails but never moves the old fence.
    pub fn observe(&mut self, observation: Observation) -> Result<BoundaryStep, BoundaryError> {
        let Observation {
            received_through,
            tracks,
            text_full,
            maintenance_armed,
        } = observation;
        if received_through < self.start_frame {
            return Err(BoundaryError::InvalidObservation);
        }
        for track in tracks {
            track.validate(received_through)?;
        }
        if let Some(fence) = self.selected {
            if received_through < fence.frame {
                return Err(BoundaryError::InvalidObservation);
            }
            return Ok(selected_step(fence, tracks, self.maintenance_armed));
        }
        let maintenance_armed = self.maintenance_armed || maintenance_armed;
        let due = self.due_frame.or({
            if received_through >= self.nominal_end {
                Some(self.nominal_end)
            } else if text_full {
                Some(received_through)
            } else {
                None
            }
        });
        let deadline = due
            .map(|frame| {
                frame
                    .checked_add(self.deferral_frames)
                    .ok_or(BoundaryError::Overflow)
            })
            .transpose()?;
        if deadline.is_some_and(|frame| received_through > frame)
            || self.due_frame.is_some_and(|frame| received_through < frame)
        {
            return Err(BoundaryError::InvalidObservation);
        }
        let natural =
            due.and_then(|due| common_natural_cut(self.start_frame, due, received_through, tracks));
        // A proved natural cut wins even when observed exactly at the deadline/text cap.
        // Maintenance only annotates this ordinary decision; it never chooses an earlier cut.
        let fence = if let Some(frame) = natural {
            Some(BoundaryFence {
                frame,
                kind: BoundaryKind::Natural,
            })
        } else if text_full {
            if received_through == self.start_frame {
                return Err(BoundaryError::EmptyTextCut);
            }
            Some(BoundaryFence {
                frame: received_through,
                kind: BoundaryKind::Forced(ForceReason::TextCap),
            })
        } else if deadline == Some(received_through) {
            Some(BoundaryFence {
                frame: received_through,
                kind: BoundaryKind::Forced(ForceReason::Deadline),
            })
        } else {
            None
        };
        let result = if let Some(fence) = fence {
            selected_step(fence, tracks, maintenance_armed)
        } else {
            let frames = deadline
                .unwrap_or(self.nominal_end)
                .checked_sub(received_through)
                .filter(|frames| *frames != 0)
                .ok_or(BoundaryError::InvalidObservation)?
                .min(MAX_ADMISSION_FRAMES);
            BoundaryStep::Admit { frames }
        };
        self.due_frame = due;
        self.selected = fence;
        self.maintenance_armed = maintenance_armed;
        Ok(result)
    }
}

fn common_natural_cut(
    window_start: u64,
    due: u64,
    received: u64,
    tracks: [TrackBoundary; 2],
) -> Option<u64> {
    // No empty cuts. Construction checked that window_start + nominal is representable.
    let mut first = due.max(window_start.checked_add(1)?);
    let mut last = received;
    for track in tracks {
        if let TrackBoundary::Established {
            start_frame,
            delivered_through,
            natural,
        } = track
        {
            let natural = natural?;
            // Conservatively restrict natural cuts to the established track's owned interval.
            // A late origin does not prove silence or ownership of its missing earlier prefix.
            first = first.max(start_frame).max(natural.first);
            last = last.min(delivered_through).min(natural.last);
        }
    }
    (first <= last).then_some(first)
}

fn selected_step(
    fence: BoundaryFence,
    tracks: [TrackBoundary; 2],
    reset_for_maintenance: bool,
) -> BoundaryStep {
    BoundaryStep::Selected {
        fence,
        delivery_complete: tracks.into_iter().all(|track| track.covers(fence.frame)),
        reset_for_maintenance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(start: u64, nominal: u64) -> BoundaryPolicy {
        BoundaryPolicy::new(start, nominal, 8000, Duration::from_secs(1)).unwrap()
    }

    fn present(start: u64, delivered: u64, natural: Option<(u64, u64)>) -> TrackBoundary {
        TrackBoundary::Established {
            start_frame: start,
            delivered_through: delivered,
            natural: natural.map(|(first, last)| SourceInterval { first, last }),
        }
    }

    fn observation(received: u64, tracks: [TrackBoundary; 2]) -> Observation {
        Observation {
            received_through: received,
            tracks,
            text_full: false,
            maintenance_armed: false,
        }
    }

    fn selected(frame: u64, kind: BoundaryKind, delivery_complete: bool) -> BoundaryStep {
        BoundaryStep::Selected {
            fence: BoundaryFence { frame, kind },
            delivery_complete,
            reset_for_maintenance: false,
        }
    }

    #[test]
    fn nominal_audio_cap_due_defers_instead_of_old_immediate_checkpoint() {
        let mut p = policy(20, 7);
        let busy = [present(20, 27, None); 2];
        assert_eq!(
            p.observe(observation(27, busy)),
            Ok(BoundaryStep::Admit { frames: 4096 })
        );
        assert_eq!(p.due_frame(), Some(27));
        assert_eq!(p.maximum_owned_frames(), 7 + 176_000);
        assert_eq!(p.selected_fence(), None);
    }

    #[test]
    fn boolean_idle_and_quiet_dwell_without_onset_proof_cannot_cut() {
        let mut p = policy(0, 100);
        // Rejected proposed baseline: both idle flags plus arbitrary elapsed quiet certify NOW.
        // Native may still hide a min-speech onset before that frame. No proof is supplied here.
        let boolean_idle = [true, true];
        let dwell_elapsed = true;
        assert!(boolean_idle.into_iter().all(|idle| idle) && dwell_elapsed);
        assert_eq!(
            p.observe(observation(4000, [present(0, 4000, None); 2])),
            Ok(BoundaryStep::Admit { frames: 4096 })
        );
        assert_eq!(p.due_frame(), Some(100));
    }

    #[test]
    fn historical_common_interval_includes_attempted_empty_failed_and_capped_extents() {
        let mut p = policy(100, 100);
        // Native lower bounds already include all attempts, not merely surviving words/reports.
        let tracks = [
            present(100, 360, Some((230, 320))),
            present(100, 310, Some((250, 300))),
        ];
        assert_eq!(
            p.observe(observation(400, tracks)),
            Ok(selected(250, BoundaryKind::Natural, true))
        );
    }

    #[test]
    fn disjoint_intervals_and_unequal_aec_delivery_wait_for_common_coverage() {
        let mut p = policy(0, 100);
        let disjoint = [
            present(0, 400, Some((100, 199))),
            present(0, 400, Some((200, 300))),
        ];
        assert!(matches!(
            p.observe(observation(400, disjoint)),
            Ok(BoundaryStep::Admit { .. })
        ));
        let delayed = [
            present(0, 400, Some((200, 350))),
            present(0, 150, Some((100, 350))),
        ];
        assert!(matches!(
            p.observe(observation(400, delayed)),
            Ok(BoundaryStep::Admit { .. })
        ));
        let delivered = [delayed[0], present(0, 220, Some((100, 350)))];
        assert_eq!(
            p.observe(observation(400, delivered)),
            Ok(selected(200, BoundaryKind::Natural, true))
        );
    }

    #[test]
    fn never_present_is_neutral_but_late_origin_does_not_certify_a_missing_prefix() {
        let mut p = policy(0, 100);
        assert_eq!(
            p.observe(observation(
                150,
                [
                    present(0, 150, Some((120, 150))),
                    TrackBoundary::NeverPresent
                ]
            )),
            Ok(selected(120, BoundaryKind::Natural, true))
        );
        let mut late = policy(0, 100);
        let tracks = [
            present(0, 400, Some((100, 350))),
            present(300, 400, Some((0, 350))),
        ];
        assert_eq!(
            late.observe(observation(400, tracks)),
            Ok(selected(300, BoundaryKind::Natural, true))
        );
    }

    #[test]
    fn natural_cut_wins_coincident_deadline_and_does_not_admit_one_frame() {
        let mut p = policy(0, 100);
        let deadline = 100 + p.deferral_frames();
        let tracks = [present(0, deadline, Some((deadline, deadline))); 2];
        assert_eq!(
            p.observe(observation(deadline, tracks)),
            Ok(selected(deadline, BoundaryKind::Natural, true))
        );
    }

    #[test]
    fn forced_fence_is_immutable_while_delivery_waits_without_an_eof_input() {
        let mut p = policy(0, 100);
        let deadline = 100 + p.deferral_frames();
        let held = [present(0, deadline, None), present(0, deadline - 10, None)];
        let force = BoundaryKind::Forced(ForceReason::Deadline);
        assert_eq!(
            p.observe(observation(deadline, held)),
            Ok(selected(deadline, force, false))
        );
        for _ in 0..3 {
            assert_eq!(
                p.observe(observation(deadline, held)),
                Ok(selected(deadline, force, false))
            );
        }
        // Actual finish-only AEC delivery may complete with no new source; the policy has no EOF
        // shortcut, cannot flush native state, and does not claim this coverage is settlement.
        let ready = [present(0, deadline, Some((100, deadline))); 2];
        let mut changed = observation(deadline, ready);
        changed.text_full = true;
        assert_eq!(p.observe(changed), Ok(selected(deadline, force, true)));
        assert_eq!(
            p.observe(observation(
                deadline + 40,
                [present(0, deadline + 40, Some((100, deadline + 40))); 2]
            )),
            Ok(selected(deadline, force, true))
        );
        assert_eq!(p.due_frame(), Some(100));
    }

    #[test]
    fn selected_force_before_late_track_origin_has_empty_old_track_ownership() {
        let mut p = policy(0, 100);
        let mut full = observation(20, [present(0, 20, None), TrackBoundary::NeverPresent]);
        full.text_full = true;
        let force = BoundaryKind::Forced(ForceReason::TextCap);
        assert_eq!(p.observe(full), Ok(selected(20, force, true)));
        let late = [present(0, 60, None), present(50, 50, None)];
        assert_eq!(
            p.observe(observation(60, late)),
            Ok(selected(20, force, true))
        );
    }

    #[test]
    fn text_cap_forces_immediately_unless_a_natural_cut_is_already_proved() {
        let mut p = policy(0, 100);
        let mut full = observation(20, [present(0, 20, None); 2]);
        full.text_full = true;
        assert_eq!(
            p.observe(full),
            Ok(selected(
                20,
                BoundaryKind::Forced(ForceReason::TextCap),
                true
            ))
        );
        assert_eq!(p.due_frame(), Some(20));
        let mut natural = policy(0, 100);
        full.tracks = [present(0, 20, Some((0, 20))); 2];
        assert_eq!(
            natural.observe(full),
            Ok(selected(20, BoundaryKind::Natural, true))
        );
        let mut empty = policy(0, 1);
        full.received_through = 0;
        full.tracks = [TrackBoundary::NeverPresent; 2];
        assert_eq!(empty.observe(full), Err(BoundaryError::EmptyTextCut));
        assert_eq!(empty.due_frame(), None);
    }

    #[test]
    fn text_cap_during_deferral_keeps_original_due_and_one_selected_fence() {
        let mut p = policy(10, 10);
        assert!(matches!(
            p.observe(observation(20, [present(10, 20, None); 2])),
            Ok(BoundaryStep::Admit { .. })
        ));
        let mut full = observation(30, [present(10, 30, None); 2]);
        full.text_full = true;
        let force = selected(30, BoundaryKind::Forced(ForceReason::TextCap), true);
        assert_eq!(p.observe(full), Ok(force));
        assert_eq!(p.due_frame(), Some(20));
        // Neither a new due crossing, a cleared cap nor a new native proof creates another fence.
        assert_eq!(
            p.observe(observation(
                1_000_000,
                [present(10, 1_000_000, Some((100, 200))); 2]
            )),
            Ok(force)
        );
        let selected_state = p.clone();
        assert_eq!(
            p.observe(observation(29, [TrackBoundary::NeverPresent; 2])),
            Err(BoundaryError::InvalidObservation)
        );
        assert_eq!(p, selected_state);
    }

    #[test]
    fn continuous_speech_huge_and_irregular_chunks_have_bounded_progress_one_force_per_window() {
        let mut start = 0;
        for window in 0..4 {
            let mut p = policy(start, 701);
            let end = start + p.maximum_owned_frames();
            let mut received = start;
            let mut operations = 0;
            loop {
                operations += 1;
                match p
                    .observe(observation(received, [present(start, received, None); 2]))
                    .unwrap()
                {
                    BoundaryStep::Admit { frames } => {
                        assert!((1..=4096).contains(&frames));
                        let producer = if window % 2 == 0 {
                            u64::MAX
                        } else {
                            [7000, 17, 991][operations % 3]
                        };
                        received += frames.min(producer);
                    }
                    BoundaryStep::Selected { fence, .. } => {
                        assert_eq!(
                            fence,
                            BoundaryFence {
                                frame: end,
                                kind: BoundaryKind::Forced(ForceReason::Deadline)
                            }
                        );
                        assert!(operations < 200, "no max(1) spin after nominal due");
                        assert_eq!(p.due_frame(), Some(start + 701));
                        start = end;
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn fractional_actual_min_silence_is_ceiled_once_for_all_supported_rates() {
        for rate in [8000, 16_000, 44_100, 48_000, 192_000] {
            let default = BoundaryPolicy::new(0, 1, rate, Duration::from_secs(1)).unwrap();
            assert_eq!(default.deferral_frames(), 22 * u64::from(rate));
            let fractional =
                BoundaryPolicy::new(0, 1, rate, Duration::new(0, 250_000_001)).unwrap();
            assert_eq!(
                fractional.deferral_frames(),
                u64::from(rate) * 21 + u64::from(rate) / 4 + 1
            );
        }
    }

    #[test]
    fn tiny_nominal_is_preserved_and_checked_limits_do_not_saturate_or_shorten() {
        let mut tiny = policy(9, 1);
        assert_eq!(
            tiny.observe(observation(9, [TrackBoundary::NeverPresent; 2])),
            Ok(BoundaryStep::Admit { frames: 1 })
        );
        assert_eq!(
            tiny.observe(observation(10, [TrackBoundary::NeverPresent; 2])),
            Ok(selected(10, BoundaryKind::Natural, true))
        );
        for rate in [0, 7999, 192_001, u32::MAX] {
            assert_eq!(
                BoundaryPolicy::new(0, 1, rate, Duration::ZERO),
                Err(BoundaryError::UnsupportedRate)
            );
        }
        assert_eq!(
            BoundaryPolicy::new(0, 0, 8000, Duration::ZERO),
            Err(BoundaryError::EmptyWindow)
        );
        for (start, nominal, silence) in [
            (u64::MAX, 1, Duration::ZERO),
            (0, u64::MAX, Duration::ZERO),
            (0, 1, Duration::MAX),
        ] {
            assert_eq!(
                BoundaryPolicy::new(start, nominal, 8000, silence),
                Err(BoundaryError::Overflow)
            );
        }
    }

    #[test]
    fn invalid_observations_do_not_mutate_latch_or_fence() {
        let mut p = policy(10, 10);
        let initial = p.clone();
        for bad in [
            observation(9, [TrackBoundary::NeverPresent; 2]),
            observation(20, [present(0, 21, None); 2]),
            observation(20, [present(15, 14, None); 2]),
            observation(20, [present(10, 20, Some((20, 19))); 2]),
            observation(20 + 176_001, [TrackBoundary::NeverPresent; 2]),
        ] {
            assert_eq!(p.observe(bad), Err(BoundaryError::InvalidObservation));
            assert_eq!(p, initial);
        }
    }

    fn with_maintenance(step: BoundaryStep) -> BoundaryStep {
        match step {
            BoundaryStep::Selected {
                fence,
                delivery_complete,
                ..
            } => BoundaryStep::Selected {
                fence,
                delivery_complete,
                reset_for_maintenance: true,
            },
            admit => admit,
        }
    }

    #[test]
    fn maintenance_cannot_shorten_before_due() {
        let mut ordinary = policy(0, 60 * 8000);
        let mut maintenance = ordinary.clone();
        let mut observed = observation(42, [present(0, 42, None); 2]);
        let ordinary_step = ordinary.observe(observed).unwrap();
        assert_eq!(ordinary_step, BoundaryStep::Admit { frames: 4096 });
        observed.maintenance_armed = true;
        assert_eq!(maintenance.observe(observed).unwrap(), ordinary_step);
        assert_eq!(maintenance.due_frame(), None);
        assert_eq!(maintenance.selected_fence(), None);
    }

    #[test]
    fn maintenance_at_empty_window_arms_without_empty_cut_or_one_frame_spin() {
        let mut p = policy(20, 100);
        let mut now = observation(20, [TrackBoundary::NeverPresent; 2]);
        now.maintenance_armed = true;
        assert_eq!(p.observe(now), Ok(BoundaryStep::Admit { frames: 100 }));
        assert_eq!(p.due_frame(), None);
        assert_eq!(p.selected_fence(), None);
        // A late track with no earlier ownership does not change the ordinary common cut.
        let ready = observation(
            120,
            [
                present(20, 120, Some((120, 120))),
                present(120, 120, Some((120, 120))),
            ],
        );
        assert_eq!(
            p.observe(ready),
            Ok(with_maintenance(selected(120, BoundaryKind::Natural, true)))
        );
    }

    #[test]
    fn maintenance_arm_is_sticky_but_preserves_historical_natural_cut() {
        let mut ordinary = policy(0, 100);
        let mut maintenance = ordinary.clone();
        let before_due = observation(20, [present(0, 20, None); 2]);
        let mut armed = before_due;
        armed.maintenance_armed = true;
        assert_eq!(maintenance.observe(armed), ordinary.observe(before_due));
        let historical = observation(
            400,
            [
                present(0, 360, Some((230, 320))),
                present(0, 310, Some((250, 300))),
            ],
        );
        let expected = ordinary.observe(historical).unwrap();
        assert_eq!(expected, selected(250, BoundaryKind::Natural, true));
        assert_eq!(
            maintenance.observe(historical),
            Ok(with_maintenance(expected))
        );
        assert_eq!(maintenance.due_frame(), ordinary.due_frame());
        assert_eq!(
            maintenance.maximum_owned_frames(),
            ordinary.maximum_owned_frames()
        );
    }

    #[test]
    fn maintenance_preserves_ordinary_natural_deadline_and_text_priority() {
        let deadline = 100 + policy(0, 100).deferral_frames();
        for (received, proof, text_full) in [
            (deadline, Some((deadline, deadline)), false),
            (deadline, None, false),
            (20, None, true),
            (20, Some((0, 20)), true),
        ] {
            let mut ordinary = policy(0, 100);
            let mut maintenance = ordinary.clone();
            let mut now = observation(received, [present(0, received, proof); 2]);
            now.text_full = text_full;
            let expected = ordinary.observe(now).unwrap();
            now.maintenance_armed = true;
            assert_eq!(maintenance.observe(now), Ok(with_maintenance(expected)));
            assert_eq!(maintenance.due_frame(), ordinary.due_frame());
            assert_eq!(maintenance.deferral_frames(), 176_000);
        }
    }

    #[test]
    fn maintenance_never_changes_admission_or_rearms_due_on_irregular_chunks() {
        let mut ordinary = policy(0, 701);
        let mut maintenance = ordinary.clone();
        let mut received = 0;
        let mut steps = 0;
        loop {
            steps += 1;
            let now = observation(received, [present(0, received, None); 2]);
            let expected = ordinary.observe(now).unwrap();
            let mut armed = now;
            armed.maintenance_armed = steps == 1;
            assert_eq!(maintenance.observe(armed), Ok(with_maintenance(expected)));
            assert_eq!(maintenance.due_frame(), ordinary.due_frame());
            match expected {
                BoundaryStep::Admit { frames } => {
                    received += frames.min([7000, 17, 991][steps % 3])
                }
                BoundaryStep::Selected { fence, .. } => {
                    assert_eq!(fence.frame, 701 + 176_000);
                    assert_eq!(fence.kind, BoundaryKind::Forced(ForceReason::Deadline));
                    assert!(steps < 200);
                    break;
                }
            }
        }
    }

    #[test]
    fn maintenance_selection_and_reset_flag_are_immutable_while_delivery_finishes() {
        let deadline = 100 + policy(0, 100).deferral_frames();
        for initially_armed in [false, true] {
            let mut p = policy(0, 100);
            let mut now = observation(
                deadline,
                [present(0, deadline, None), present(0, deadline - 1, None)],
            );
            now.maintenance_armed = initially_armed;
            let mut expected =
                selected(deadline, BoundaryKind::Forced(ForceReason::Deadline), false);
            if initially_armed {
                expected = with_maintenance(expected);
            }
            assert_eq!(p.observe(now), Ok(expected));
            now.maintenance_armed = !initially_armed;
            assert_eq!(p.observe(now), Ok(expected));
            now.tracks = [present(0, deadline, Some((100, deadline))); 2];
            let mut delivered =
                selected(deadline, BoundaryKind::Forced(ForceReason::Deadline), true);
            if initially_armed {
                delivered = with_maintenance(delivered);
            }
            assert_eq!(p.observe(now), Ok(delivered));

            let mut natural = policy(0, 10);
            let mut ready = observation(10, [present(0, 10, Some((10, 10))); 2]);
            ready.maintenance_armed = initially_armed;
            let selected = natural.observe(ready).unwrap();
            ready.maintenance_armed = !initially_armed;
            assert_eq!(natural.observe(ready), Ok(selected));
        }
    }

    #[test]
    fn invalid_observation_does_not_arm_maintenance() {
        let mut p = policy(10, 10);
        let original = p.clone();
        let mut invalid = observation(9, [TrackBoundary::NeverPresent; 2]);
        invalid.maintenance_armed = true;
        assert_eq!(p.observe(invalid), Err(BoundaryError::InvalidObservation));
        assert_eq!(p, original);
        assert_eq!(
            p.observe(observation(20, [TrackBoundary::NeverPresent; 2])),
            Ok(selected(20, BoundaryKind::Natural, true))
        );
    }
}
