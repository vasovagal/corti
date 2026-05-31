//! Pure debounce/coalesce state machine for the mic-in-use trigger.
//!
//! This is the timing logic factored out of any platform code so it is deterministic and
//! unit-testable: feed it raw mic-on/off signals and timer ticks with an explicit `now`, and it returns
//! the capture [`Action`]s to take. It depends only on [`std::time`] — no CoreAudio, no `chrono` — so it
//! compiles and its tests run on any host.
//!
//! ## Behaviour
//! - A rising edge must persist for `debounce` before a recording starts (drops notification chirps and
//!   brief device reacquisitions).
//! - A falling edge must persist for `coalesce` before a recording stops; a mic blip shorter than
//!   `coalesce` is absorbed into the ongoing recording, so one recording spans a brief mid-call gap.
//! - On stop, recordings whose mic-open span was shorter than `min_recording` are flagged for discard.
//!
//! The platform worker (`crate::platform`) calls [`Machine::next_deadline`] to know when to wake, then
//! drives [`Machine::on_signal`] / [`Machine::on_tick`] and acts on the returned [`Action`]. All edge
//! *confirmations* happen on a tick (after the relevant window elapses); [`Machine::on_signal`] only
//! advances state.

use std::time::{Duration, Instant};

/// A confirmed edge the worker must act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// A rising edge was confirmed — start a recording.
    Start,
    /// A falling edge was confirmed — stop the recording. `keep` is `false` when the mic-open span was
    /// shorter than the minimum floor (an accidental blip to discard); `duration` is that span.
    Stop { keep: bool, duration: Duration },
}

/// Internal trigger state.
#[derive(Debug, Clone, Copy)]
enum State {
    /// Mic idle; nothing pending.
    Idle,
    /// Mic went on at `since`; waiting for it to persist `debounce` before starting.
    Arming { since: Instant },
    /// Recording in progress since `started`.
    Recording { started: Instant },
    /// Recording in progress (`started`) but the mic went off at `since`; waiting out the `coalesce`
    /// window before stopping. A mic-on within the window cancels the stop and resumes recording.
    Coasting { started: Instant, since: Instant },
}

/// The debounce/coalesce state machine. Construct with [`Machine::new`]; drive with [`on_signal`],
/// [`on_tick`], and [`next_deadline`].
///
/// [`on_signal`]: Machine::on_signal
/// [`on_tick`]: Machine::on_tick
/// [`next_deadline`]: Machine::next_deadline
pub struct Machine {
    debounce: Duration,
    coalesce: Duration,
    min_recording: Duration,
    state: State,
}

impl Machine {
    /// Create a machine with the rising-edge (`debounce`), falling-edge/gap (`coalesce`), and
    /// minimum-recording windows.
    pub fn new(debounce: Duration, coalesce: Duration, min_recording: Duration) -> Self {
        Self {
            debounce,
            coalesce,
            min_recording,
            state: State::Idle,
        }
    }

    /// Feed a raw mic-in-use sample (`on`) observed at `now`. This only advances state — confirmations
    /// are emitted by [`on_tick`](Machine::on_tick) once the relevant window elapses. Redundant
    /// same-direction signals are idempotent (they never re-stamp a pending deadline).
    pub fn on_signal(&mut self, on: bool, now: Instant) {
        self.state = match (self.state, on) {
            // Rising edge from idle: begin debouncing.
            (State::Idle, true) => State::Arming { since: now },
            // Mic released before the rising edge confirmed: it was a chirp, cancel.
            (State::Arming { .. }, false) => State::Idle,
            // Mic released while recording: begin the coalesce window before stopping.
            (State::Recording { started }, false) => State::Coasting {
                started,
                since: now,
            },
            // Mic came back within the coalesce window: cancel the stop, resume the same recording.
            (State::Coasting { started, .. }, true) => State::Recording { started },
            // Everything else is a no-op (idempotent duplicates / irrelevant edges): keep state and any
            // pending deadline unchanged.
            (state, _) => state,
        };
    }

    /// Feed a timer tick observed at `now`. Returns an [`Action`] when a pending window has elapsed.
    /// A tick before the deadline (e.g. a spurious early wakeup) is a no-op, so it is safe to call at any
    /// time.
    pub fn on_tick(&mut self, now: Instant) -> Option<Action> {
        match self.state {
            State::Arming { since } if now.saturating_duration_since(since) >= self.debounce => {
                self.state = State::Recording { started: now };
                Some(Action::Start)
            }
            State::Coasting { started, since }
                if now.saturating_duration_since(since) >= self.coalesce =>
            {
                self.state = State::Idle;
                // The mic-open span is start → the last mic-off (`since`), NOT the stop-confirm time, so
                // the coalesce tail can't inflate a short blip past the floor.
                let duration = since.saturating_duration_since(started);
                Some(Action::Stop {
                    keep: duration >= self.min_recording,
                    duration,
                })
            }
            _ => None,
        }
    }

    /// When the worker should next wake to re-check, or `None` if nothing is pending (`Idle`/`Recording`
    /// wait for the next signal). After [`on_tick`](Machine::on_tick) at or past the returned instant,
    /// this is guaranteed to become `None` or a strictly-later instant — so a timed wakeup never spins.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.state {
            State::Arming { since } => Some(since + self.debounce),
            State::Coasting { since, .. } => Some(since + self.coalesce),
            State::Idle | State::Recording { .. } => None,
        }
    }

    /// Force the machine back to idle. Used by the worker when `Recorder::start` fails after a confirmed
    /// rising edge, so the machine doesn't stay in `Recording` with no live recorder.
    pub fn reset(&mut self) {
        self.state = State::Idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEBOUNCE: Duration = Duration::from_millis(1500);
    const COALESCE: Duration = Duration::from_secs(2);
    const MIN: Duration = Duration::from_secs(3);

    fn machine() -> Machine {
        Machine::new(DEBOUNCE, COALESCE, MIN)
    }

    /// A base instant to build a deterministic relative timeline from (only offsets matter).
    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn chirp_shorter_than_debounce_never_starts() {
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        assert_eq!(m.next_deadline(), Some(t + DEBOUNCE));
        // Mic drops before the debounce elapses → cancel.
        m.on_signal(false, t + Duration::from_millis(500));
        assert!(m.next_deadline().is_none());
        // A tick at what would have been the deadline does nothing.
        assert_eq!(m.on_tick(t + DEBOUNCE), None);
    }

    #[test]
    fn sustained_on_starts_after_debounce() {
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        // Tick before the deadline: nothing yet.
        assert_eq!(m.on_tick(t + Duration::from_millis(1000)), None);
        // Tick at the deadline: Start.
        assert_eq!(m.on_tick(t + DEBOUNCE), Some(Action::Start));
        // Now recording — no pending deadline.
        assert!(m.next_deadline().is_none());
    }

    #[test]
    fn duplicate_on_does_not_restamp_debounce() {
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        // A spurious duplicate `on` later must NOT defer the deadline.
        m.on_signal(true, t + Duration::from_millis(900));
        assert_eq!(m.next_deadline(), Some(t + DEBOUNCE));
        assert_eq!(m.on_tick(t + DEBOUNCE), Some(Action::Start));
    }

    #[test]
    fn brief_gap_is_coalesced_into_one_recording() {
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        assert_eq!(m.on_tick(t + DEBOUNCE), Some(Action::Start));
        let started = t + DEBOUNCE;
        // Mic drops, then returns within the coalesce window.
        let off = started + Duration::from_secs(10);
        m.on_signal(false, off);
        assert_eq!(m.next_deadline(), Some(off + COALESCE));
        m.on_signal(true, off + Duration::from_millis(500));
        // Back to recording, no pending stop.
        assert!(m.next_deadline().is_none());
        // A tick at the old coalesce deadline does nothing — the stop was cancelled.
        assert_eq!(m.on_tick(off + COALESCE), None);
    }

    #[test]
    fn sustained_drop_finishes_and_keeps_long_recording() {
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        m.on_tick(t + DEBOUNCE);
        let started = t + DEBOUNCE;
        let off = started + Duration::from_secs(60);
        m.on_signal(false, off);
        // Tick before the coalesce window elapses: nothing.
        assert_eq!(m.on_tick(off + Duration::from_millis(1000)), None);
        // Tick at the coalesce deadline: Stop, kept (60s ≥ 3s).
        assert_eq!(
            m.on_tick(off + COALESCE),
            Some(Action::Stop {
                keep: true,
                duration: Duration::from_secs(60),
            })
        );
        assert!(m.next_deadline().is_none());
    }

    #[test]
    fn short_recording_is_discarded() {
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        m.on_tick(t + DEBOUNCE);
        let started = t + DEBOUNCE;
        // Mic open only 1s (< MIN) then drops.
        let off = started + Duration::from_secs(1);
        m.on_signal(false, off);
        assert_eq!(
            m.on_tick(off + COALESCE),
            Some(Action::Stop {
                keep: false,
                duration: Duration::from_secs(1),
            })
        );
    }

    #[test]
    fn coalesce_tail_does_not_inflate_duration_past_floor() {
        // Mic open 2.9s (< 3s). Even though the stop confirms COALESCE later, the reported duration must
        // be 2.9s and the recording discarded.
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        m.on_tick(t + DEBOUNCE);
        let started = t + DEBOUNCE;
        let off = started + Duration::from_millis(2900);
        m.on_signal(false, off);
        assert_eq!(
            m.on_tick(off + COALESCE),
            Some(Action::Stop {
                keep: false,
                duration: Duration::from_millis(2900),
            })
        );
    }

    #[test]
    fn next_deadline_tracks_state() {
        let mut m = machine();
        assert!(m.next_deadline().is_none()); // Idle
        let t = base();
        m.on_signal(true, t);
        assert_eq!(m.next_deadline(), Some(t + DEBOUNCE)); // Arming
        m.on_tick(t + DEBOUNCE);
        assert!(m.next_deadline().is_none()); // Recording
        let off = t + DEBOUNCE + Duration::from_secs(5);
        m.on_signal(false, off);
        assert_eq!(m.next_deadline(), Some(off + COALESCE)); // Coasting
    }

    #[test]
    fn tick_at_or_past_deadline_always_clears_it() {
        // The busy-loop invariant: after a tick at/after the deadline, next_deadline is None or future.
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        m.on_tick(t + DEBOUNCE + Duration::from_millis(1)); // slightly past
        assert!(m.next_deadline().is_none()); // moved to Recording
        let off = t + DEBOUNCE + Duration::from_secs(5);
        m.on_signal(false, off);
        m.on_tick(off + COALESCE + Duration::from_millis(1));
        assert!(m.next_deadline().is_none()); // moved to Idle
    }

    #[test]
    fn flapping_faster_than_debounce_never_starts() {
        let mut m = machine();
        let mut t = base();
        for _ in 0..10 {
            m.on_signal(true, t);
            t += Duration::from_millis(200);
            m.on_signal(false, t); // off before debounce → cancels back to Idle
            t += Duration::from_millis(200);
            assert_eq!(m.on_tick(t), None); // a tick in between never starts
        }
        assert!(m.next_deadline().is_none());
    }

    #[test]
    fn reset_forces_idle() {
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        m.on_tick(t + DEBOUNCE); // Recording
        m.reset();
        assert!(m.next_deadline().is_none());
        // After reset, a mic-off is a no-op; a fresh mic-on re-arms.
        m.on_signal(false, t + DEBOUNCE + Duration::from_secs(1));
        assert!(m.next_deadline().is_none());
        let t2 = t + DEBOUNCE + Duration::from_secs(2);
        m.on_signal(true, t2);
        assert_eq!(m.next_deadline(), Some(t2 + DEBOUNCE));
    }

    #[test]
    fn full_cycle_then_rearm() {
        let mut m = machine();
        let t = base();
        m.on_signal(true, t);
        assert_eq!(m.on_tick(t + DEBOUNCE), Some(Action::Start));
        let started = t + DEBOUNCE;
        let off = started + Duration::from_secs(10);
        m.on_signal(false, off);
        assert!(matches!(
            m.on_tick(off + COALESCE),
            Some(Action::Stop { keep: true, .. })
        ));
        // Idle again; a second recording can run.
        let t2 = off + COALESCE + Duration::from_secs(1);
        m.on_signal(true, t2);
        assert_eq!(m.on_tick(t2 + DEBOUNCE), Some(Action::Start));
    }
}
