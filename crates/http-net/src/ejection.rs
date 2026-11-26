//! Connection ejection (ADR-0043 §14): a backend client closes the one
//! connection that misbehaves instead of pooling it.
//!
//! A pooled connection keeps the backend a layer-4 balancer chose for it,
//! and the pool hands out the most recently returned connection first: a
//! connection is a client's only handle on one backend behind a shared
//! address. Closing it makes the next request take another pooled
//! connection or a new one, which the balancer spreads again. An answer is
//! never sent again (the retry rule stays: never after an HTTP answer).
//!
//! - **A broken backend**: the first answer whose status is in
//!   [`EjectionRules::close_statuses`] closes its connection.
//! - **A backend shedding load**: [`EjectionRules::shed_close_after`]
//!   answers in a row on one connection whose status is in
//!   [`EjectionRules::shed_statuses`] close it.
//! - Both within the client's budget ([`crate::ErrorCloses`]): at a total
//!   outage every answer would close its connection, each leaving a socket in
//!   `TIME_WAIT` for the kernel's 60 s; past the budget the connection is
//!   pooled as any other.
//! - **A request abandoned** before its answer was read (a timeout, or its
//!   caller gone) closes its connection always, outside the budget: the
//!   answer may still come on it.
//!
//! Latency alone never closes a connection. A 200 whose body does not parse
//! is the caller's to judge and pools as usual.

mod rules;

use std::sync::atomic::{AtomicU32, Ordering};

pub use rules::{
    EjectionRules, StatusSet, DEFAULT_CLOSE_STATUSES, DEFAULT_SHED_CLOSE_AFTER,
    DEFAULT_SHED_STATUSES,
};

use crate::backend::metrics;
use crate::error_close::ErrorCloses;

/// Why a client closed a connection: the `cause` label of
/// `http_request_connection_closes_total`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseCause {
    /// An answer whose status is in the close statuses.
    Status,
    /// A run of answers whose status is in the shed statuses.
    Shed,
    /// A request abandoned before its answer was read.
    Timeout,
    /// Its client stopped taking requests: rotated out, or dropped.
    Rotation,
    /// Idle in the pool past the idle timeout, or past the idle cap.
    Idle,
}

impl CloseCause {
    /// Every cause, in declaration order.
    pub const ALL: [CloseCause; 5] = [
        CloseCause::Status,
        CloseCause::Shed,
        CloseCause::Timeout,
        CloseCause::Rotation,
        CloseCause::Idle,
    ];

    /// The `cause` label value.
    pub const fn label(self) -> &'static str {
        match self {
            CloseCause::Status => "status",
            CloseCause::Shed => "shed",
            CloseCause::Timeout => "timeout",
            CloseCause::Rotation => "rotation",
            CloseCause::Idle => "idle",
        }
    }
}

metric_catalogue::assert_exposition_order!(CloseCause: Status, Shed, Timeout, Rotation, Idle);

/// What a client keeps about one of its connections.
#[derive(Debug, Default)]
pub struct ConnectionRecord {
    /// Shed answers in a row on the connection.
    shed_run: AtomicU32,
}

/// One client's ejection: its rules and its budget of closes.
pub struct Ejector {
    rules: EjectionRules,
    budget: ErrorCloses,
}

impl Ejector {
    /// An ejector under `rules`, its budget full now.
    pub fn new(rules: &EjectionRules) -> Self {
        Self { rules: rules.clone(), budget: ErrorCloses::new(rules.closes_per_s) }
    }

    /// The verdict on the connection `record` keeps, which carried an answer
    /// of `status`: the record's run moved on, a close taken from the
    /// budget, nothing counted yet.
    pub fn answered(&self, record: &ConnectionRecord, status: u16) -> Verdict {
        let wanted = self.wanted(record, status);
        let wanted = wanted.filter(|_| self.rules.closes_per_s > 0);
        let closed = wanted.filter(|_| self.budget.take());
        let refused = wanted.is_some() && closed.is_none();
        Verdict { status, closed, refused, ends: closed.is_some() }
    }

    /// What the rules ask of the connection after an answer of `status`,
    /// the record's run of shed answers moved on.
    fn wanted(&self, record: &ConnectionRecord, status: u16) -> Option<CloseCause> {
        if !self.rules.shed_statuses.contains(status) {
            record.shed_run.store(0, Ordering::Relaxed);
            return self.rules.close_statuses.contains(status).then_some(CloseCause::Status);
        }
        let run = record.shed_run.load(Ordering::Relaxed).saturating_add(1);
        record.shed_run.store(run, Ordering::Relaxed);
        (run >= self.rules.shed_close_after).then_some(CloseCause::Shed)
    }

    /// A request on a connection to `peer` was abandoned before its answer
    /// was read: the connection closes, whatever the budget.
    pub fn abandoned(&self, peer: &str) {
        metrics::connection_closed(peer, CloseCause::Timeout);
    }
}

/// What becomes of a connection after one answer on it. The caller acts on
/// it first ([`Verdict::closes`]) and counts it after ([`Verdict::count`]),
/// so a connection to close leaves the pool as soon as its answer resolves.
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub struct Verdict {
    status: u16,
    /// The client closes the connection, and why.
    closed: Option<CloseCause>,
    /// The rules would close it and the budget refused.
    refused: bool,
    /// The connection ends, the client's close or its peer's.
    ends: bool,
}

impl Verdict {
    /// The verdict on an answer of `status` whose connection its peer ends:
    /// no close of the client's, no budget taken.
    pub fn peer_ends(status: u16) -> Self {
        Self { status, closed: None, refused: false, ends: true }
    }

    /// Whether the client closes the connection, and why.
    pub fn closes(&self) -> Option<CloseCause> {
        self.closed
    }

    /// The connection's end was counted already, by another answer on it or
    /// otherwise: this verdict counts no close.
    pub fn close_counted(mut self) -> Self {
        self.closed = None;
        self
    }

    /// Count it under `peer`: its close by cause, a close the budget
    /// refused, and a 5xx by what became of its connection.
    pub fn count(self, peer: &str) {
        if self.refused {
            metrics::closes_budget_exhausted(peer);
        }
        if (500..=599).contains(&self.status) {
            metrics::error_answered(peer, self.ends);
        }
        if let Some(cause) = self.closed {
            metrics::connection_closed(peer, cause);
        }
    }
}

/// A client closed a connection to `peer` for `cause` outside any answer:
/// [`CloseCause::Idle`] or [`CloseCause::Rotation`].
pub fn closed(peer: &str, cause: CloseCause) {
    metrics::connection_closed(peer, cause);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn ejector(closes_per_s: u32) -> Ejector {
        Ejector::new(&EjectionRules { closes_per_s, ..EjectionRules::default() })
    }

    /// The close `e` decides on an answer of `status` on `conn`, counted
    /// under `peer`.
    fn answer(e: &Ejector, peer: &str, conn: &ConnectionRecord, status: u16) -> Option<CloseCause> {
        let verdict = e.answered(conn, status);
        let closes = verdict.closes();
        verdict.count(peer);
        closes
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_status_closes_on_its_first_answer_and_a_shed_status_after_three_in_a_row() {
        let e = ejector(20);
        let conn = ConnectionRecord::default();
        let answers = [200, 500, 502, 504, 404, 429, 503, 503, 200, 503, 503, 408, 408];
        let verdicts: Vec<_> = answers.iter().map(|s| answer(&e, "ej-a", &conn, *s)).collect();
        use CloseCause::{Shed, Status};
        assert_eq!(
            verdicts,
            [
                None,
                Some(Status),
                Some(Status),
                Some(Status),
                None,
                None,
                None,
                Some(Shed),
                None,
                None,
                None,
                Some(Shed),
                Some(Shed),
            ]
        );
        assert_eq!(metrics::connection_closes("ej-a", Status), 3);
        assert_eq!(metrics::connection_closes("ej-a", Shed), 3);
        assert_eq!(metrics::error_answers("ej-a", true), 4);
        assert_eq!(metrics::error_answers("ej-a", false), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn past_the_budget_a_close_is_refused_and_counted_until_the_budget_refills() {
        let e = ejector(2);
        let conn = ConnectionRecord::default();
        let closes = || answer(&e, "ej-b", &conn, 500).is_some();
        assert_eq!([closes(), closes(), closes()], [true, true, false]);
        assert_eq!(metrics::closes_budget_exhausted_count("ej-b"), 1);
        tokio::time::advance(Duration::from_millis(500)).await;
        assert!(closes(), "half a second refills one");
        assert_eq!(metrics::error_answers("ej-b", false), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_budget_of_zero_never_closes_and_counts_no_refusal() {
        let e = ejector(0);
        let conn = ConnectionRecord::default();
        assert_eq!(answer(&e, "ej-c", &conn, 500), None);
        assert_eq!(metrics::closes_budget_exhausted_count("ej-c"), 0);
        assert_eq!(metrics::error_answers("ej-c", false), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_counted_already_or_a_connection_its_peer_ends_counts_no_close() {
        let e = ejector(20);
        let conn = ConnectionRecord::default();
        e.answered(&conn, 500).close_counted().count("ej-d");
        Verdict::peer_ends(502).count("ej-d");
        assert_eq!(metrics::connection_closes("ej-d", CloseCause::Status), 0);
        assert_eq!(metrics::error_answers("ej-d", true), 2, "both connections end");
        assert_eq!(metrics::closes_budget_exhausted_count("ej-d"), 0);
    }
}
