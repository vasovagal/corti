use std::fmt;

use corti_postprocess::{
    CredentialSourceKind, CredentialState, ErrorCode, Lane, MonotonicDeadline, RequestFence,
    VERTEX_UNARMED_WARNING,
};
use thiserror::Error;

use crate::transport::Clock;

/// Vertex ADC resolution cadence while credentials are unarmed.
pub const VERTEX_CREDENTIAL_POLL_INTERVAL_MICROS: u64 = 5_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexResolutionKind {
    Resolve,
    Refresh,
}

/// Opaque identity for one resolver operation. The caller gives this value back with the sanitized result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VertexResolutionAttempt {
    id: u64,
    episode: u64,
    kind: VertexResolutionKind,
    started_at_micros: u64,
}

impl VertexResolutionAttempt {
    pub const fn id(self) -> u64 {
        self.id
    }

    pub const fn episode(self) -> u64 {
        self.episode
    }

    pub const fn kind(self) -> VertexResolutionKind {
        self.kind
    }

    pub const fn started_at_micros(self) -> u64 {
        self.started_at_micros
    }
}

/// Secret-free state of the deterministic Vertex ADC resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexCredentialState {
    Unarmed {
        episode: u64,
        next_poll_at_micros: u64,
    },
    Resolving {
        attempt: VertexResolutionAttempt,
    },
    Ready {
        episode: u64,
        expires_at_unix_ms: Option<i64>,
    },
    Refreshing {
        attempt: VertexResolutionAttempt,
        expires_at_unix_ms: Option<i64>,
    },
    Rejected {
        episode: u64,
    },
    Error {
        episode: u64,
        code: ErrorCode,
    },
}

impl VertexCredentialState {
    pub const fn episode(self) -> u64 {
        match self {
            Self::Unarmed { episode, .. }
            | Self::Ready { episode, .. }
            | Self::Rejected { episode }
            | Self::Error { episode, .. } => episode,
            Self::Resolving { attempt } | Self::Refreshing { attempt, .. } => attempt.episode,
        }
    }

    pub fn credential_state(self) -> CredentialState {
        match self {
            Self::Unarmed { .. } => CredentialState::Absent,
            Self::Resolving { .. } => CredentialState::Resolving,
            Self::Ready {
                expires_at_unix_ms, ..
            } => CredentialState::Ready {
                expires_at_unix_ms,
                source: CredentialSourceKind::ApplicationDefaultCredentials,
            },
            Self::Refreshing { .. } => CredentialState::Refreshing,
            Self::Rejected { .. } => CredentialState::Rejected,
            Self::Error { code, .. } => CredentialState::Error { code },
        }
    }
}

/// Sanitized completion supplied by the injected ADC resolver driver. Access-token bytes do not enter this
/// state machine; the driver retains them in the injected in-memory [`crate::AdcAccessTokenSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexResolutionOutcome {
    Ready { expires_at_unix_ms: Option<i64> },
    Unarmed,
    Rejected,
    Error { code: ErrorCode },
}

/// The only Vertex warning payload. It has no `Display` implementation, preventing accidental decoration by
/// this layer; its visible message is the exact domain constant.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VertexUnarmedNotice {
    episode: u64,
}

impl VertexUnarmedNotice {
    pub const fn episode(self) -> u64 {
        self.episode
    }

    pub const fn role(self) -> &'static str {
        "alert"
    }

    pub const fn visible_message(self) -> &'static str {
        VERTEX_UNARMED_WARNING
    }
}

impl fmt::Debug for VertexUnarmedNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexUnarmedNotice")
            .field("episode", &self.episode)
            .finish_non_exhaustive()
    }
}

/// Content-free identity of the newest automatic-lane snapshot waiting for Vertex credentials. The actual
/// prompt/transcript remains owned by the caller and is recovered by matching this fence and sequence.
#[derive(Clone, PartialEq, Eq)]
pub struct VertexAutoPending {
    lane: Lane,
    sequence: u64,
    fence: RequestFence,
    deadline: MonotonicDeadline,
}

impl VertexAutoPending {
    pub fn new(
        lane: Lane,
        sequence: u64,
        fence: RequestFence,
        deadline: MonotonicDeadline,
    ) -> Result<Self, VertexPendingError> {
        if !lane.is_automatic() {
            return Err(VertexPendingError::NotAutomatic);
        }
        Ok(Self {
            lane,
            sequence,
            fence,
            deadline,
        })
    }

    pub const fn lane(&self) -> Lane {
        self.lane
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn fence(&self) -> &RequestFence {
        &self.fence
    }

    pub const fn deadline(&self) -> MonotonicDeadline {
        self.deadline
    }
}

impl fmt::Debug for VertexAutoPending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexAutoPending")
            .field("lane", &self.lane)
            .field("sequence", &self.sequence)
            .field("fence", &self.fence)
            .field("deadline", &self.deadline)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VertexPendingError {
    #[error("only automatic lanes have newest-state catch-up slots")]
    NotAutomatic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexPendingRetention {
    Stored,
    Replaced,
    IgnoredOlder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VertexDispatchDisposition {
    DispatchNow,
    Arming(VertexPendingRetention),
    WaitingForRefresh(VertexPendingRetention),
    Blocked(ErrorCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VertexDispatchIntent {
    pub disposition: VertexDispatchDisposition,
    pub warning: Option<VertexUnarmedNotice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexResolutionUpdate {
    pub state: VertexCredentialState,
    /// At most one still-valid newest snapshot for each automatic lane, in live/final/pinned order.
    pub catch_up: Vec<VertexAutoPending>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum VertexResolverError {
    #[error("resolver completion does not match the active attempt")]
    StaleAttempt,
    #[error("credential refresh requires a ready token")]
    NotReady,
}

/// Pure, clock-injected Vertex credential state machine.
///
/// [`Self::drive`] emits a command rather than touching ADC itself. Exactly one command can be outstanding;
/// repeated calls while resolving/refreshing are no-ops. An unarmed failure schedules the next attempt from
/// the previous attempt's start, so slow resolution never introduces cadence drift.
pub struct VertexCredentialResolver {
    clock: Box<dyn Clock>,
    state: VertexCredentialState,
    next_attempt_id: u64,
    warned_episode: Option<u64>,
    live_pending: Option<VertexAutoPending>,
    final_pending: Option<VertexAutoPending>,
    pinned_pending: Option<VertexAutoPending>,
}

impl VertexCredentialResolver {
    pub fn new(clock: Box<dyn Clock>) -> Self {
        let now = clock.monotonic_micros();
        Self {
            clock,
            state: VertexCredentialState::Unarmed {
                episode: 1,
                next_poll_at_micros: now,
            },
            next_attempt_id: 1,
            warned_episode: None,
            live_pending: None,
            final_pending: None,
            pinned_pending: None,
        }
    }

    pub const fn state(&self) -> VertexCredentialState {
        self.state
    }

    /// Starts a due unarmed resolution attempt. At 4.999 seconds after the last start this returns `None`;
    /// at exactly 5.000 seconds it returns one attempt. It never starts while another attempt is outstanding.
    pub fn drive(&mut self) -> Option<VertexResolutionAttempt> {
        let VertexCredentialState::Unarmed {
            episode,
            next_poll_at_micros,
        } = self.state
        else {
            return None;
        };
        let now = self.clock.monotonic_micros();
        if now < next_poll_at_micros {
            return None;
        }
        let attempt = self.new_attempt(episode, VertexResolutionKind::Resolve, now);
        self.state = VertexCredentialState::Resolving { attempt };
        Some(attempt)
    }

    /// Begins one explicit refresh. While it is outstanding, neither `drive` nor another refresh can create
    /// an overlapping resolver operation.
    pub fn start_refresh(&mut self) -> Result<VertexResolutionAttempt, VertexResolverError> {
        let VertexCredentialState::Ready {
            episode,
            expires_at_unix_ms,
        } = self.state
        else {
            return Err(VertexResolverError::NotReady);
        };
        let attempt = self.new_attempt(
            episode,
            VertexResolutionKind::Refresh,
            self.clock.monotonic_micros(),
        );
        self.state = VertexCredentialState::Refreshing {
            attempt,
            expires_at_unix_ms,
        };
        Ok(attempt)
    }

    /// Records a cache miss that intends to dispatch. This is separate from pending-snapshot retention so
    /// explicit ad-hoc queues can use the same once-per-episode warning without being coalesced here.
    pub fn observe_dispatch_intent(&mut self) -> Option<VertexUnarmedNotice> {
        if !matches!(
            self.state,
            VertexCredentialState::Unarmed { .. } | VertexCredentialState::Resolving { .. }
        ) {
            return None;
        }
        let episode = self.state.episode();
        if self.warned_episode == Some(episode) {
            return None;
        }
        self.warned_episode = Some(episode);
        Some(VertexUnarmedNotice { episode })
    }

    /// Retains only the latest sequence for this automatic lane and returns the corresponding arming signal.
    pub fn intend_auto_dispatch(&mut self, pending: VertexAutoPending) -> VertexDispatchIntent {
        match self.state {
            VertexCredentialState::Ready { .. } => VertexDispatchIntent {
                disposition: VertexDispatchDisposition::DispatchNow,
                warning: None,
            },
            VertexCredentialState::Unarmed { .. } | VertexCredentialState::Resolving { .. } => {
                let retention = self.retain_pending(pending);
                VertexDispatchIntent {
                    disposition: VertexDispatchDisposition::Arming(retention),
                    warning: self.observe_dispatch_intent(),
                }
            }
            VertexCredentialState::Refreshing { .. } => {
                let retention = self.retain_pending(pending);
                VertexDispatchIntent {
                    disposition: VertexDispatchDisposition::WaitingForRefresh(retention),
                    warning: None,
                }
            }
            VertexCredentialState::Rejected { .. } => VertexDispatchIntent {
                disposition: VertexDispatchDisposition::Blocked(ErrorCode::AuthRejected),
                warning: None,
            },
            VertexCredentialState::Error { code, .. } => VertexDispatchIntent {
                disposition: VertexDispatchDisposition::Blocked(code),
                warning: None,
            },
        }
    }

    pub fn complete(
        &mut self,
        attempt: VertexResolutionAttempt,
        outcome: VertexResolutionOutcome,
    ) -> Result<VertexResolutionUpdate, VertexResolverError> {
        let active = match self.state {
            VertexCredentialState::Resolving { attempt }
            | VertexCredentialState::Refreshing { attempt, .. } => attempt,
            _ => return Err(VertexResolverError::StaleAttempt),
        };
        if active != attempt {
            return Err(VertexResolverError::StaleAttempt);
        }

        let was_refresh = attempt.kind == VertexResolutionKind::Refresh;
        let mut catch_up = Vec::new();
        match outcome {
            VertexResolutionOutcome::Ready { expires_at_unix_ms } => {
                self.state = VertexCredentialState::Ready {
                    episode: attempt.episode,
                    expires_at_unix_ms,
                };
                let now = self.clock.monotonic_micros();
                catch_up = self.take_valid_pending(now);
            }
            VertexResolutionOutcome::Unarmed => {
                let episode = if was_refresh {
                    next_episode(attempt.episode)
                } else {
                    attempt.episode
                };
                if was_refresh {
                    self.warned_episode = None;
                }
                self.state = VertexCredentialState::Unarmed {
                    episode,
                    next_poll_at_micros: attempt
                        .started_at_micros
                        .saturating_add(VERTEX_CREDENTIAL_POLL_INTERVAL_MICROS),
                };
            }
            VertexResolutionOutcome::Rejected => {
                self.state = VertexCredentialState::Rejected {
                    episode: attempt.episode,
                };
            }
            VertexResolutionOutcome::Error { code } => {
                self.state = VertexCredentialState::Error {
                    episode: attempt.episode,
                    code,
                };
            }
        }
        Ok(VertexResolutionUpdate {
            state: self.state,
            catch_up,
        })
    }

    /// Moves a ready/refreshing/rejected/error state into a fresh unarmed episode. Repeated loss signals while
    /// already unarmed or resolving do not create warning spam or invalidate the active poll.
    pub fn mark_token_lost(&mut self) -> VertexCredentialState {
        if matches!(
            self.state,
            VertexCredentialState::Unarmed { .. } | VertexCredentialState::Resolving { .. }
        ) {
            return self.state;
        }
        let episode = next_episode(self.state.episode());
        self.warned_episode = None;
        self.state = VertexCredentialState::Unarmed {
            episode,
            next_poll_at_micros: self.clock.monotonic_micros(),
        };
        self.state
    }

    pub fn clear_pending(&mut self) {
        self.live_pending = None;
        self.final_pending = None;
        self.pinned_pending = None;
    }

    fn new_attempt(
        &mut self,
        episode: u64,
        kind: VertexResolutionKind,
        started_at_micros: u64,
    ) -> VertexResolutionAttempt {
        let id = self.next_attempt_id;
        self.next_attempt_id = self.next_attempt_id.wrapping_add(1).max(1);
        VertexResolutionAttempt {
            id,
            episode,
            kind,
            started_at_micros,
        }
    }

    fn retain_pending(&mut self, pending: VertexAutoPending) -> VertexPendingRetention {
        let slot = match pending.lane {
            Lane::Live => &mut self.live_pending,
            Lane::Final => &mut self.final_pending,
            Lane::PinnedQuestion => &mut self.pinned_pending,
            Lane::AdHocQuestion => unreachable!("VertexAutoPending rejects ad-hoc lanes"),
        };
        match slot {
            Some(existing) if existing.sequence > pending.sequence => {
                VertexPendingRetention::IgnoredOlder
            }
            Some(_) => {
                *slot = Some(pending);
                VertexPendingRetention::Replaced
            }
            None => {
                *slot = Some(pending);
                VertexPendingRetention::Stored
            }
        }
    }

    fn take_valid_pending(&mut self, now_micros: u64) -> Vec<VertexAutoPending> {
        [
            self.live_pending.take(),
            self.final_pending.take(),
            self.pinned_pending.take(),
        ]
        .into_iter()
        .flatten()
        .filter(|pending| !pending.deadline.is_expired_at(now_micros))
        .collect()
    }
}

impl fmt::Debug for VertexCredentialResolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexCredentialResolver")
            .field("state", &self.state)
            .field("warning_emitted", &self.warned_episode.is_some())
            .field(
                "live_pending",
                &self.live_pending.as_ref().map(|p| p.sequence),
            )
            .field(
                "final_pending",
                &self.final_pending.as_ref().map(|p| p.sequence),
            )
            .field(
                "pinned_pending",
                &self.pinned_pending.as_ref().map(|p| p.sequence),
            )
            .finish()
    }
}

fn next_episode(episode: u64) -> u64 {
    episode.wrapping_add(1).max(1)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use corti_postprocess::{ProcessEpoch, RequestFence};

    use super::*;

    #[derive(Clone)]
    struct ExactClock(Arc<AtomicU64>);

    impl ExactClock {
        fn new(now: u64) -> Self {
            Self(Arc::new(AtomicU64::new(now)))
        }

        fn set(&self, now: u64) {
            self.0.store(now, Ordering::SeqCst);
        }
    }

    impl Clock for ExactClock {
        fn monotonic_micros(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn pending(lane: Lane, sequence: u64, deadline: u64) -> VertexAutoPending {
        VertexAutoPending::new(
            lane,
            sequence,
            RequestFence {
                process_epoch: ProcessEpoch(1),
                session_generation: 1,
                transcript_revision: sequence,
                control_revision: 1,
                lane_revision: 1,
                steering_revision: 1,
                bank_revision: 1,
                question_revision: None,
            },
            MonotonicDeadline(deadline),
        )
        .unwrap()
    }

    #[test]
    fn polls_at_exact_five_second_boundary_without_overlap() {
        let clock = ExactClock::new(10);
        let mut resolver = VertexCredentialResolver::new(Box::new(clock.clone()));
        let first = resolver.drive().unwrap();
        assert!(
            resolver.drive().is_none(),
            "an in-flight poll must not overlap"
        );
        resolver
            .complete(first, VertexResolutionOutcome::Unarmed)
            .unwrap();

        clock.set(5_000_009);
        assert!(resolver.drive().is_none());
        clock.set(5_000_010);
        let second = resolver.drive().unwrap();
        assert_ne!(first.id(), second.id());
        assert!(resolver.drive().is_none());
    }

    #[test]
    fn warning_occurs_once_per_unarmed_episode_and_rearms_after_token_loss() {
        let clock = ExactClock::new(0);
        let mut resolver = VertexCredentialResolver::new(Box::new(clock.clone()));
        let first = resolver
            .intend_auto_dispatch(pending(Lane::Live, 1, 20_000_000))
            .warning
            .unwrap();
        assert_eq!(first.role(), "alert");
        assert_eq!(first.visible_message(), "gcloud token isn't armed");
        assert!(
            resolver
                .intend_auto_dispatch(pending(Lane::Live, 2, 20_000_000))
                .warning
                .is_none()
        );

        let attempt = resolver.drive().unwrap();
        resolver
            .complete(attempt, VertexResolutionOutcome::Unarmed)
            .unwrap();
        assert!(
            resolver.observe_dispatch_intent().is_none(),
            "a failed poll remains in the same warned episode"
        );
        clock.set(VERTEX_CREDENTIAL_POLL_INTERVAL_MICROS);
        let attempt = resolver.drive().unwrap();
        resolver
            .complete(
                attempt,
                VertexResolutionOutcome::Ready {
                    expires_at_unix_ms: Some(123),
                },
            )
            .unwrap();
        assert!(resolver.observe_dispatch_intent().is_none());
        resolver.mark_token_lost();
        let rearmed = resolver.observe_dispatch_intent().unwrap();
        assert_ne!(first.episode(), rearmed.episode());
        assert_eq!(rearmed.visible_message(), VERTEX_UNARMED_WARNING);
        assert!(resolver.observe_dispatch_intent().is_none());
    }

    #[test]
    fn successful_resolution_returns_only_newest_valid_auto_state() {
        let clock = ExactClock::new(100);
        let mut resolver = VertexCredentialResolver::new(Box::new(clock.clone()));
        let attempt = resolver.drive().unwrap();
        resolver.intend_auto_dispatch(pending(Lane::Live, 7, 50_000_000));
        resolver.intend_auto_dispatch(pending(Lane::Live, 9, 50_000_000));
        assert_eq!(
            resolver
                .intend_auto_dispatch(pending(Lane::Live, 8, 50_000_000))
                .disposition,
            VertexDispatchDisposition::Arming(VertexPendingRetention::IgnoredOlder)
        );
        resolver.intend_auto_dispatch(pending(Lane::Final, 3, 99));
        resolver.intend_auto_dispatch(pending(Lane::PinnedQuestion, 4, 50_000_000));

        let update = resolver
            .complete(
                attempt,
                VertexResolutionOutcome::Ready {
                    expires_at_unix_ms: None,
                },
            )
            .unwrap();
        assert_eq!(
            update
                .catch_up
                .iter()
                .map(|pending| (pending.lane(), pending.sequence()))
                .collect::<Vec<_>>(),
            [(Lane::Live, 9), (Lane::PinnedQuestion, 4)]
        );
        assert!(matches!(update.state, VertexCredentialState::Ready { .. }));
    }

    #[test]
    fn refresh_failure_creates_a_new_unarmed_episode() {
        let clock = ExactClock::new(0);
        let mut resolver = VertexCredentialResolver::new(Box::new(clock.clone()));
        let resolve = resolver.drive().unwrap();
        resolver
            .complete(
                resolve,
                VertexResolutionOutcome::Ready {
                    expires_at_unix_ms: Some(1000),
                },
            )
            .unwrap();
        let old_episode = resolver.state().episode();
        let refresh = resolver.start_refresh().unwrap();
        assert!(resolver.drive().is_none());
        resolver
            .complete(refresh, VertexResolutionOutcome::Unarmed)
            .unwrap();
        assert_ne!(resolver.state().episode(), old_episode);
    }
}
