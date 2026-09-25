//! Never-drop accumulation of finalized live rows into bounded hosted rewrite batches.
//!
//! The coordinator used to target only the newest row batch and superseded anything queued, so a
//! rewrite that took longer than the gap between two VAD regions was thrown away and its rows never
//! rewritten. [`LiveBatcher`] replaces that with a pure, clock-driven accumulator:
//!
//! - rows accumulate while a Live call is outstanding and form the *next* batch when it completes;
//! - a batch dispatches once the transcript has been quiet for `quiet_micros`, or the oldest pending
//!   row has waited `max_wait_micros`, or the batch is full — and never more often than
//!   `min_interval_micros`;
//! - a batch always carries at least its head row, even when that row alone exceeds `max_bytes`, so an
//!   oversized row is sent alone rather than stalling everything behind it;
//! - lag is bounded: when the backlog exceeds `max_backlog_rows` or `max_backlog_age_micros`, the oldest
//!   pending rows are released to the caller as raw so the live view never falls minutes behind.
//!
//! The batcher owns no rows, only caller-supplied keys (the app uses ledger indices), and knows nothing
//! about providers, so it tests with a manual clock.

use std::collections::VecDeque;

/// Timing and size policy for one session's live rewrite batching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveBatchPolicy {
    /// Dispatch once no new row has arrived for this long.
    pub quiet_micros: u64,
    /// Dispatch regardless of quiet once the oldest pending row has waited this long.
    pub max_wait_micros: u64,
    /// Minimum spacing between two dispatches.
    pub min_interval_micros: u64,
    /// Maximum rows per batch.
    pub max_rows: usize,
    /// Byte budget per batch (the head row is always included even when it alone exceeds it).
    pub max_bytes: usize,
    /// Backlog beyond which the oldest pending rows are released as raw.
    pub max_backlog_rows: usize,
    /// Pending age beyond which rows are released as raw.
    pub max_backlog_age_micros: u64,
}

impl Default for LiveBatchPolicy {
    fn default() -> Self {
        Self {
            quiet_micros: 400_000,
            max_wait_micros: 2_000_000,
            min_interval_micros: 250_000,
            max_rows: 8,
            max_bytes: 4 * 1024,
            max_backlog_rows: 32,
            max_backlog_age_micros: 45_000_000,
        }
    }
}

impl LiveBatchPolicy {
    /// Dispatch on every push with no timing, keeping the size caps and the outstanding-batch rule.
    /// Tests that drive the service synchronously use this; production uses [`Default`].
    pub fn immediate() -> Self {
        Self {
            quiet_micros: 0,
            max_wait_micros: 0,
            min_interval_micros: 0,
            ..Self::default()
        }
    }

    /// Whether any timing applies at all.
    pub const fn is_immediate(&self) -> bool {
        self.quiet_micros == 0 && self.max_wait_micros == 0 && self.min_interval_micros == 0
    }
}

#[derive(Debug, Clone)]
struct Pending<T> {
    item: T,
    bytes: usize,
    pushed_at_micros: u64,
}

/// Rows that could not be kept pending and were released to the caller as raw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedBacklog<T> {
    pub items: Vec<T>,
    pub reason: BacklogReleaseReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacklogReleaseReason {
    TooManyRows,
    TooOld,
}

/// See the module documentation.
#[derive(Debug)]
pub struct LiveBatcher<T> {
    policy: LiveBatchPolicy,
    pending: VecDeque<Pending<T>>,
    last_push_at_micros: Option<u64>,
    last_dispatch_at_micros: Option<u64>,
    dispatched_batches: u64,
    released_rows: u64,
}

impl<T> LiveBatcher<T> {
    pub fn new(policy: LiveBatchPolicy) -> Self {
        Self {
            policy,
            pending: VecDeque::new(),
            last_push_at_micros: None,
            last_dispatch_at_micros: None,
            dispatched_batches: 0,
            released_rows: 0,
        }
    }

    pub const fn policy(&self) -> &LiveBatchPolicy {
        &self.policy
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub const fn dispatched_batches(&self) -> u64 {
        self.dispatched_batches
    }

    pub const fn released_rows(&self) -> u64 {
        self.released_rows
    }

    /// Append newly finalized rows (in transcript order) with their UTF-8 byte sizes.
    pub fn push(&mut self, items: impl IntoIterator<Item = (T, usize)>, now_micros: u64) {
        let mut pushed = false;
        for (item, bytes) in items {
            self.pending.push_back(Pending {
                item,
                bytes,
                pushed_at_micros: now_micros,
            });
            pushed = true;
        }
        if pushed {
            self.last_push_at_micros = Some(now_micros);
        }
    }

    /// Return rows from a batch that could not be submitted for a transient reason to the *front* of the
    /// queue, in their original order, keeping their original arrival time so max-wait still applies.
    pub fn give_back(
        &mut self,
        items: impl IntoIterator<Item = (T, usize)>,
        pushed_at_micros: u64,
    ) {
        let returned: Vec<_> = items
            .into_iter()
            .map(|(item, bytes)| Pending {
                item,
                bytes,
                pushed_at_micros,
            })
            .collect();
        for pending in returned.into_iter().rev() {
            self.pending.push_front(pending);
        }
    }

    /// The next batch, if one is due. `outstanding` says whether a Live call is already queued, active,
    /// or awaiting application; while it is, nothing dispatches and rows keep accumulating.
    ///
    /// The returned batch's `pushed_at_micros` is the arrival time of its head row, for `give_back`.
    pub fn take_due(&mut self, now_micros: u64, outstanding: bool) -> Option<Batch<T>> {
        if outstanding || self.pending.is_empty() {
            return None;
        }
        let quiet = self
            .last_push_at_micros
            .is_none_or(|at| now_micros.saturating_sub(at) >= self.policy.quiet_micros);
        let oldest = self
            .pending
            .front()
            .map(|pending| pending.pushed_at_micros)
            .unwrap_or(now_micros);
        let waited_max = now_micros.saturating_sub(oldest) >= self.policy.max_wait_micros;
        let full = self.pending.len() >= self.policy.max_rows
            || self
                .pending
                .iter()
                .map(|pending| pending.bytes)
                .sum::<usize>()
                >= self.policy.max_bytes;
        let spaced = self
            .last_dispatch_at_micros
            .is_none_or(|at| now_micros.saturating_sub(at) >= self.policy.min_interval_micros);
        if !(quiet || waited_max || full) || !spaced {
            return None;
        }
        let mut items = Vec::new();
        let mut bytes = 0usize;
        let head_pushed_at = oldest;
        while let Some(front) = self.pending.front() {
            let next_bytes = bytes.saturating_add(front.bytes);
            if !items.is_empty()
                && (items.len() >= self.policy.max_rows || next_bytes > self.policy.max_bytes)
            {
                break;
            }
            let pending = self.pending.pop_front().expect("front was present");
            bytes = next_bytes;
            items.push(pending.item);
        }
        self.last_dispatch_at_micros = Some(now_micros);
        self.dispatched_batches = self.dispatched_batches.saturating_add(1);
        Some(Batch {
            items,
            bytes,
            pushed_at_micros: head_pushed_at,
        })
    }

    /// Enforce the backlog bounds: rows beyond `max_backlog_rows`, or older than
    /// `max_backlog_age_micros`, are released from the front so the caller can leave them raw.
    pub fn release_overdue(&mut self, now_micros: u64) -> Option<ReleasedBacklog<T>> {
        let mut items = Vec::new();
        let mut reason = None;
        while self.pending.len() > self.policy.max_backlog_rows {
            let pending = self.pending.pop_front().expect("len > 0");
            items.push(pending.item);
            reason = Some(BacklogReleaseReason::TooManyRows);
        }
        while let Some(front) = self.pending.front() {
            if now_micros.saturating_sub(front.pushed_at_micros)
                < self.policy.max_backlog_age_micros
            {
                break;
            }
            let pending = self.pending.pop_front().expect("front was present");
            items.push(pending.item);
            reason.get_or_insert(BacklogReleaseReason::TooOld);
        }
        let reason = reason?;
        self.released_rows = self.released_rows.saturating_add(items.len() as u64);
        Some(ReleasedBacklog { items, reason })
    }

    /// Drop everything pending (session end); returns the released rows for accounting.
    pub fn drain_all(&mut self) -> Vec<T> {
        self.last_push_at_micros = None;
        self.pending.drain(..).map(|pending| pending.item).collect()
    }

    /// When the next timing edge could make a batch due, for callers that pick a wake-up timeout.
    pub fn next_due_at(&self) -> Option<u64> {
        let oldest = self.pending.front()?.pushed_at_micros;
        let quiet_due = self
            .last_push_at_micros
            .map_or(oldest, |at| at.saturating_add(self.policy.quiet_micros));
        let wait_due = oldest.saturating_add(self.policy.max_wait_micros);
        let earliest = quiet_due.min(wait_due);
        let spaced = self
            .last_dispatch_at_micros
            .map_or(0, |at| at.saturating_add(self.policy.min_interval_micros));
        Some(earliest.max(spaced))
    }
}

/// One dispatchable batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch<T> {
    pub items: Vec<T>,
    pub bytes: usize,
    /// Arrival time of the head row, to hand back on `give_back`.
    pub pushed_at_micros: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(ids: impl IntoIterator<Item = usize>) -> Vec<(usize, usize)> {
        ids.into_iter().map(|id| (id, 100)).collect()
    }

    fn batcher() -> LiveBatcher<usize> {
        LiveBatcher::new(LiveBatchPolicy::default())
    }

    #[test]
    fn quiet_period_dispatches_one_batch() {
        let mut batcher = batcher();
        batcher.push(rows([1, 2]), 0);
        assert!(batcher.take_due(100_000, false).is_none(), "not quiet yet");
        batcher.push(rows([3]), 200_000);
        assert!(
            batcher.take_due(500_000, false).is_none(),
            "quiet timer restarts on push"
        );
        let batch = batcher.take_due(600_000, false).unwrap();
        assert_eq!(batch.items, vec![1, 2, 3]);
        assert_eq!(batch.bytes, 300);
        assert_eq!(batch.pushed_at_micros, 0);
        assert!(batcher.is_empty());
    }

    #[test]
    fn max_wait_forces_dispatch_during_continuous_speech() {
        let mut batcher = batcher();
        let mut now = 0;
        batcher.push(rows([1]), now);
        // Rows keep arriving every 300 ms, so the quiet period never elapses.
        for id in 2..=7 {
            now += 300_000;
            batcher.push(rows([id]), now);
            assert!(batcher.take_due(now, false).is_none() || now >= 2_000_000);
        }
        let batch = batcher.take_due(2_100_000, false).unwrap();
        assert_eq!(batch.items.len(), 7);
    }

    #[test]
    fn full_batch_dispatches_and_overflow_forms_the_next_batch() {
        let mut batcher = batcher();
        batcher.push(rows(1..=11), 0);
        let first = batcher.take_due(0, false).unwrap();
        assert_eq!(first.items, (1..=8).collect::<Vec<_>>());
        assert_eq!(batcher.pending_len(), 3);
        // Spacing: the overflow waits for min_interval, then quiet applies.
        assert!(batcher.take_due(100_000, false).is_none());
        let second = batcher.take_due(500_000, false).unwrap();
        assert_eq!(second.items, vec![9, 10, 11]);
    }

    #[test]
    fn head_row_over_the_byte_cap_dispatches_alone_and_never_stalls_the_rest() {
        let mut batcher = batcher();
        batcher.push([(1usize, 10_000usize), (2, 100), (3, 100)], 0);
        let first = batcher.take_due(500_000, false).unwrap();
        assert_eq!(first.items, vec![1]);
        assert_eq!(first.bytes, 10_000);
        let second = batcher.take_due(1_000_000, false).unwrap();
        assert_eq!(second.items, vec![2, 3]);
    }

    #[test]
    fn rows_accumulate_while_a_batch_is_outstanding() {
        let mut batcher = batcher();
        batcher.push(rows([1]), 0);
        let first = batcher.take_due(500_000, false).unwrap();
        assert_eq!(first.items, vec![1]);
        batcher.push(rows([2]), 600_000);
        batcher.push(rows([3]), 700_000);
        assert!(
            batcher.take_due(2_000_000, true).is_none(),
            "one outstanding batch, ever"
        );
        assert_eq!(batcher.pending_len(), 2);
        let second = batcher.take_due(2_000_000, false).unwrap();
        assert_eq!(second.items, vec![2, 3], "nothing was dropped or reordered");
    }

    #[test]
    fn min_interval_spaces_batches() {
        let mut batcher = batcher();
        batcher.push(rows(1..=8), 0);
        assert!(batcher.take_due(0, false).is_some());
        batcher.push(rows(9..=16), 0);
        assert!(
            batcher.take_due(100_000, false).is_none(),
            "too soon after the last dispatch"
        );
        assert!(batcher.take_due(250_000, false).is_some());
    }

    #[test]
    fn give_back_preserves_order_and_arrival_time() {
        let mut batcher = batcher();
        batcher.push(rows([1, 2]), 0);
        batcher.push(rows([3]), 100_000);
        let batch = batcher.take_due(2_000_000, false).unwrap();
        assert_eq!(batch.items, vec![1, 2, 3]);
        batcher.push(rows([4]), 2_100_000);
        batcher.give_back(
            batch.items.iter().map(|id| (*id, 100)),
            batch.pushed_at_micros,
        );
        let retry = batcher.take_due(2_400_000, false).unwrap();
        assert_eq!(retry.items, vec![1, 2, 3, 4]);
        assert_eq!(retry.pushed_at_micros, 0);
    }

    #[test]
    fn overdue_backlog_is_released_from_the_front() {
        let mut batcher = batcher();
        batcher.push(rows(1..=40), 0);
        let released = batcher.release_overdue(0).unwrap();
        assert_eq!(released.reason, BacklogReleaseReason::TooManyRows);
        assert_eq!(released.items, (1..=8).collect::<Vec<_>>());
        assert_eq!(batcher.pending_len(), 32);
        assert!(batcher.release_overdue(1_000_000).is_none());
        let released = batcher.release_overdue(45_000_000).unwrap();
        assert_eq!(released.reason, BacklogReleaseReason::TooOld);
        assert_eq!(released.items.len(), 32);
        assert!(batcher.is_empty());
        assert_eq!(batcher.released_rows(), 40);
    }

    #[test]
    fn drain_all_empties_the_backlog_at_session_end() {
        let mut batcher = batcher();
        batcher.push(rows([1, 2, 3]), 0);
        assert_eq!(batcher.drain_all(), vec![1, 2, 3]);
        assert!(batcher.is_empty());
        assert!(batcher.take_due(10_000_000, false).is_none());
    }

    #[test]
    fn immediate_policy_dispatches_on_push_but_keeps_caps_and_the_outstanding_rule() {
        let mut batcher = LiveBatcher::new(LiveBatchPolicy::immediate());
        assert!(batcher.policy().is_immediate());
        batcher.push(rows(1..=9), 0);
        let first = batcher.take_due(0, false).unwrap();
        assert_eq!(first.items.len(), 8);
        assert!(batcher.take_due(0, true).is_none());
        let second = batcher.take_due(0, false).unwrap();
        assert_eq!(second.items, vec![9]);
    }

    #[test]
    fn next_due_at_reports_the_earliest_timing_edge() {
        let mut batcher = batcher();
        assert_eq!(batcher.next_due_at(), None);
        batcher.push(rows([1]), 1_000_000);
        assert_eq!(batcher.next_due_at(), Some(1_400_000));
        batcher.push(rows([2]), 1_300_000);
        assert_eq!(batcher.next_due_at(), Some(1_700_000));
        assert!(batcher.take_due(1_700_000, false).is_some());
        batcher.push(rows([3]), 1_750_000);
        // min_interval (1_950_000) is later than quiet (2_150_000)? No: quiet wins here.
        assert_eq!(batcher.next_due_at(), Some(2_150_000));
    }
}
