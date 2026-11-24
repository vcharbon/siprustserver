//! One initial INVITE's setup budget (ADR-0022 X1): a worst-case bound on the
//! time from the INVITE's receipt to its final, `call_control_timeout` long.
//! It covers the wait for the turn's permits, the decision round trips and the
//! limiter's admits of the failover chain; one check as the turn starts and one
//! deadline for every await after it. A caller past her 100 sends nothing more
//! (RFC 3261 §17.1.1.2), so when it runs out she gets the bare 503 of an
//! unavailable decision. A new call's budget also holds its start bound
//! (ADR-0070): the turn's wait for its permits ends there, and a turn that
//! starts past it creates no call. A re-asked call's budget runs from the
//! re-ask, its start bound from the receipt its record states. A value is
//! `Copy`: every await of one turn reads the same deadline.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// One turn's setup budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupBudget {
    /// The instant it runs from.
    received: Option<Instant>,
    /// The instant it runs out; `None` for [`NONE`](Self::NONE).
    due: Option<Instant>,
    /// The instant past which the turn starts no call; `None` when no start
    /// bound holds it.
    start_by: Option<Instant>,
}

impl SetupBudget {
    /// The budget of a turn that is no setup: nothing to bound, nothing to read.
    /// No consult runs under it: a copy of an INVITE is answered 482 unasked.
    pub const NONE: Self = Self { received: None, due: None, start_by: None };

    /// The budget running from `received`, `length` long, with no start bound.
    pub fn from_receipt(received: Instant, length: Duration) -> Self {
        let due = sip_clock::deadline::after(received, length);
        Self { received: Some(received), due: Some(due), start_by: None }
    }

    /// This budget, its turn held to start within `bound` of the receipt.
    pub fn with_start_bound(self, bound: Duration) -> Self {
        let start_by = self.received.map(|received| sip_clock::deadline::after(received, bound));
        Self { start_by, ..self }
    }

    /// This budget, its turn held to start within `bound` of an INVITE
    /// received `ago` before now: a receipt read from a record, not this
    /// budget's own.
    pub fn with_start_bound_since(self, ago: Duration, bound: Duration) -> Self {
        let start_by = sip_clock::deadline::from_now(bound.saturating_sub(ago));
        Self { start_by: Some(start_by), ..self }
    }

    /// Its receipt as epoch ms on `clock`; `None` for [`NONE`](Self::NONE).
    pub fn received_ms(&self, clock: &sip_clock::Clock) -> Option<i64> {
        let ago = self.elapsed()?;
        Some(clock.now_ms().saturating_sub(sip_clock::wall::millis_i64(ago)))
    }

    /// The instant it runs out, when it does.
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// The instant the turn stops waiting for its permits: its start bound,
    /// else the instant the budget runs out.
    pub fn wait_due(&self) -> Option<Instant> {
        self.start_by.or(self.due)
    }

    /// Whether it ran out.
    pub fn spent(&self) -> bool {
        self.due.is_some_and(|due| Instant::now() >= due)
    }

    /// Whether the turn starts at or past its start bound.
    pub fn started_late(&self) -> bool {
        self.start_by.is_some_and(|start_by| Instant::now() >= start_by)
    }

    /// What is left of it, zero once spent; `None` for [`NONE`](Self::NONE).
    pub fn remaining(&self) -> Option<Duration> {
        self.due.map(|due| due.saturating_duration_since(Instant::now()))
    }

    /// How long ago it started; `None` for [`NONE`](Self::NONE).
    pub fn elapsed(&self) -> Option<Duration> {
        self.received.map(|received| received.elapsed())
    }

    /// Run `work` up to the deadline: its output, or `None` where the budget
    /// cut it, its future dropped.
    pub async fn bound<T>(&self, work: impl Future<Output = T>) -> Option<T> {
        match self.due {
            Some(due) => tokio::time::timeout_at(due, work).await.ok(),
            None => Some(work.await),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[tokio::test(start_paused = true)]
    async fn it_runs_out_at_its_length_from_the_receipt() {
        let budget = SetupBudget::from_receipt(Instant::now(), 100 * MS);
        tokio::time::advance(40 * MS).await;
        assert!(!budget.spent());
        assert_eq!(budget.remaining(), Some(60 * MS));
        assert_eq!(budget.elapsed(), Some(40 * MS));
        tokio::time::advance(60 * MS).await;
        assert!(budget.spent(), "spent at the instant it is due");
        assert_eq!(budget.remaining(), Some(Duration::ZERO));
    }

    #[tokio::test(start_paused = true)]
    async fn a_receipt_older_than_its_length_is_spent_at_once() {
        let received = Instant::now();
        tokio::time::advance(150 * MS).await;
        assert!(SetupBudget::from_receipt(received, 100 * MS).spent());
    }

    #[tokio::test(start_paused = true)]
    async fn work_past_the_deadline_is_cut_and_work_inside_it_answers() {
        let budget = SetupBudget::from_receipt(Instant::now(), 100 * MS);
        let inside = budget.bound(async {
            tokio::time::sleep(99 * MS).await;
            1
        });
        assert_eq!(inside.await, Some(1));
        let past = budget.bound(async {
            tokio::time::sleep(99 * MS).await;
            2
        });
        assert_eq!(past.await, None);
        assert_eq!(Instant::now().duration_since(budget.received.unwrap()), 100 * MS);
    }

    #[test]
    fn none_bounds_nothing_and_reads_nothing() {
        assert!(!SetupBudget::NONE.spent());
        assert!(!SetupBudget::NONE.started_late());
        assert_eq!(SetupBudget::NONE.due(), None);
        assert_eq!(SetupBudget::NONE.wait_due(), None);
        assert_eq!(SetupBudget::NONE.elapsed(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn the_start_bound_runs_from_the_receipt_and_ends_the_wait() {
        let received = Instant::now();
        let budget = SetupBudget::from_receipt(received, 100 * MS);
        assert_eq!(budget.wait_due(), Some(received + 100 * MS), "no start bound: the budget");
        let bounded = budget.with_start_bound(40 * MS);
        assert_eq!(bounded.wait_due(), Some(received + 40 * MS));
        assert_eq!(bounded.due(), Some(received + 100 * MS), "the budget unchanged");
        tokio::time::advance(39 * MS).await;
        assert!(!bounded.started_late());
        tokio::time::advance(MS).await;
        assert!(bounded.started_late(), "late at the instant it is due");
        assert!(!budget.started_late(), "no start bound, never late");
        assert!(!bounded.spent());
    }

    #[tokio::test(start_paused = true)]
    async fn a_start_bound_since_an_earlier_receipt_keeps_its_own_budget() {
        let now = Instant::now();
        let budget = SetupBudget::from_receipt(now, 100 * MS);
        let inside = budget.with_start_bound_since(30 * MS, 40 * MS);
        assert_eq!(inside.wait_due(), Some(now + 10 * MS), "40 ms from a receipt 30 ms ago");
        assert_eq!(inside.due(), Some(now + 100 * MS), "the budget runs from its own receipt");
        assert!(!inside.started_late());
        let past = budget.with_start_bound_since(50 * MS, 40 * MS);
        assert!(past.started_late(), "a receipt older than the bound is late at once");
    }

    #[tokio::test(start_paused = true)]
    async fn its_receipt_reads_on_the_clock_it_elapsed_on() {
        let clock = sip_clock::Clock::test_at(1_000_000);
        let budget = SetupBudget::from_receipt(Instant::now(), 100 * MS);
        tokio::time::advance(70 * MS).await;
        assert_eq!(budget.received_ms(&clock), Some(1_000_000));
        assert_eq!(SetupBudget::NONE.received_ms(&clock), None);
    }
}
