//! What the guard counts and exposes on `/metrics`: whether it reads its node
//! withdrawn, each withdrawal by cause, each return, the readings with no
//! answer and the rulesets it could not write.

use std::sync::atomic::{AtomicU64, Ordering};

use metric_catalogue::{Catalogue, Dim, Family, Labels};

use crate::switch::Cause;

/// Why a node was withdrawn, as [`Cause::label`] names it.
pub const CAUSE: Dim = Dim::new("cause", &["unfit", "expired", "start"]);

pub const WITHDRAWN: Family = Family::gauge(
    "route_guard_withdrawn",
    Labels::None,
    "1 while the guard reads its node withdrawn: the node's BFD and BGP to its routers are dropped, so the routers hold no route through it",
);

pub const WITHDRAWALS: Family = Family::counter(
    "route_guard_withdrawals_total",
    Labels::Product(&[CAUSE]),
    "node withdrawals: unfit (the proxy answered unfit), expired (no refresh within ROUTE_GUARD_DEAD_MS: no fit answer, or none written), start (the guard started on a proxy not fit)",
);

pub const RESTORES: Family = Family::counter(
    "route_guard_restores_total",
    Labels::None,
    "withdrawn nodes let out again after the proxy read fit for ROUTE_GUARD_HOLD_MS",
);

pub const NO_ANSWER: Family = Family::counter(
    "route_guard_no_answer_total",
    Labels::None,
    "fitness readings with no answer within ROUTE_GUARD_POLL_MS: the proxy is gone, hung or slow",
);

pub const WRITE_FAILURES: Family = Family::counter(
    "route_guard_write_failures_total",
    Labels::None,
    "rulesets nft did not apply; a refresh not written lets the node's last one run out",
);

/// Every family [`GuardMetrics::prometheus_text`] renders, in order.
pub const FAMILIES: &[Family] = &[WITHDRAWN, WITHDRAWALS, RESTORES, NO_ANSWER, WRITE_FAILURES];

/// The `/metrics` catalogue of the guard: its families, then the
/// process-wide observability counters.
pub const CATALOGUE: Catalogue =
    Catalogue { binary: "route-guard", sections: &[FAMILIES, observe::counters::FAMILIES] };

/// The guard's counters and its withdrawn gauge.
#[derive(Debug, Default)]
pub struct GuardMetrics {
    withdrawn: AtomicU64,
    withdrawals: [AtomicU64; 3],
    restores: AtomicU64,
    no_answer: AtomicU64,
    write_failures: AtomicU64,
}

impl GuardMetrics {
    /// A guard's metrics before its first write: the node reads withdrawn.
    pub fn starting() -> Self {
        let metrics = Self::default();
        metrics.set_withdrawn(true);
        metrics
    }

    /// Whether the guard reads its node withdrawn.
    pub fn withdrawn(&self) -> bool {
        self.withdrawn.load(Ordering::Relaxed) != 0
    }

    pub fn set_withdrawn(&self, withdrawn: bool) {
        self.withdrawn.store(u64::from(withdrawn), Ordering::Relaxed);
    }

    pub fn withdrawal(&self, cause: &Cause) {
        let index = match cause {
            Cause::Unfit(_) => 0,
            Cause::Expired => 1,
            Cause::Start => 2,
        };
        if let Some(n) = self.withdrawals.get(index) {
            n.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn restore(&self) {
        self.restores.fetch_add(1, Ordering::Relaxed);
    }

    pub fn no_answer(&self) {
        self.no_answer.fetch_add(1, Ordering::Relaxed);
    }

    pub fn write_failure(&self) {
        self.write_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// The exposition of [`FAMILIES`].
    pub fn prometheus_text(&self) -> String {
        let load = |n: &AtomicU64| n.load(Ordering::Relaxed);
        let mut s = String::new();
        WITHDRAWN.render_value(&mut s, load(&self.withdrawn));
        WITHDRAWALS
            .render(&mut s, |series| self.withdrawals.get(series.index(&CAUSE)).map_or(0, load));
        RESTORES.render_value(&mut s, load(&self.restores));
        NO_ANSWER.render_value(&mut s, load(&self.no_answer));
        WRITE_FAILURES.render_value(&mut s, load(&self.write_failures));
        s
    }

    /// The `/metrics` body: the guard's families, then the dropped log lines
    /// and trace-admission denials (ADR-0026).
    pub fn body(&self) -> String {
        let mut text = self.prometheus_text();
        text.push_str(&observe::counters::prometheus_text());
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_holds_its_catalogue_exactly() {
        let m = GuardMetrics::default();
        m.withdrawal(&Cause::Expired);
        m.set_withdrawn(true);
        let text = m.body();
        if let Err(mismatches) = CATALOGUE.check(&text) {
            panic!("{mismatches:#?}\n{text}");
        }
        assert!(text.contains("route_guard_withdrawals_total{cause=\"expired\"} 1\n"), "{text}");
        assert!(text.contains("route_guard_withdrawn 1\n"), "{text}");
    }
}
