//! One initial INVITE's setup budget (ADR-0022 X1): a worst-case bound on the
//! time from the INVITE's receipt to its final, `call_control_timeout` long.
//! It covers the wait for the turn's permits, the decision round trips and the
//! limiter's admits of the failover chain; one check as the turn starts and one
//! deadline for every await after it. A caller past her 100 sends nothing more
//! (RFC 3261 §17.1.1.2), so when it runs out she gets the bare 503 of an
//! unavailable decision. A value is `Copy`: every await of one turn reads the
//! same deadline.

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
}

impl SetupBudget {
    /// The budget of a turn that is no setup: nothing to bound, nothing to read.
    /// No consult runs under it: a copy of an INVITE is answered 482 unasked.
    pub const NONE: Self = Self { received: None, due: None };

    /// The budget running from `received`, `length` long.
    pub fn from_receipt(received: Instant, length: Duration) -> Self {
        let due = sip_clock::deadline::after(received, length);
        Self { received: Some(received), due: Some(due) }
    }

    /// The instant it runs out, when it does.
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// Whether it ran out.
    pub fn spent(&self) -> bool {
        self.due.is_some_and(|due| Instant::now() >= due)
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
        assert_eq!(SetupBudget::NONE.due(), None);
        assert_eq!(SetupBudget::NONE.elapsed(), None);
    }
}
