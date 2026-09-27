//! The admission outcome of every new call the worker is offered: accepted
//! (the call is born and runs the whole call path) or rejected by one of the
//! admission tiers before any call state exists, and why.
//!
//! A new call is a new initial INVITE (no To-tag). It is counted once, where
//! its outcome becomes final; a retransmission of it is not a new call and is
//! never counted again. Each tier counts its own decisions:
//!
//! - the router ([`NewCallTally`], on [`B2buaMetrics`](crate::metrics::B2buaMetrics)):
//!   the store-fault 500, the global per-call queue cap, the memory bounds
//!   ([`crate::capacity`]), the CPS bucket and the panic-ELU backstop
//!   ([`crate::overload`]), every accept, and an INVITE the dispatcher
//!   discarded unrun and answered 503. It sees only INVITEs that created a
//!   server transaction, which absorbs their retransmissions;
//! - the ingress brake ([`Tier1BrakeCounters::new_calls_shed`]), which
//!   remembers the INVITEs it shed for 64·T1 so a copy is answered, below its
//!   threshold too, but not counted;
//! - the transaction layer's deferred backlog
//!   ([`sip_txn::TransactionMetrics::deferred_refused`]), which counts each
//!   refused INVITE once; its `in_dialog` class carries a To-tag and is no
//!   new call.
//!
//! [`NewCallCounts::read`] composes the three into `b2bua_new_calls_total`.
//!
//! FIXME(tier1_brake): the brake sheds a copy of an INVITE it let through
//! below its threshold and the router admitted, so that INVITE counts accepted
//! and `tier1_brake`: at most two counts per INVITE, stated in the HELP text.
//! Fix: the brake spares the INVITE identities the transaction layer holds.
//! An accepted call may still end with a final the call path authors (a
//! routing reject, a malformed-INVITE 400): that is the call's outcome, not
//! an admission one.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sip_txn::{RefusedClass, TransactionMetrics};

use crate::capacity::Bound;
use crate::overload::AdmitReason;
use crate::tier1_brake::Tier1BrakeCounters;

/// Why an admission tier rejected a new call: the `reason` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// The CPS token bucket was empty (Tier-3).
    BucketEmpty,
    /// The worker's EWMA-ELU was above the panic backstop (Tier-3).
    PanicElu,
    /// The live-call ceiling (ADR-0037).
    CapacityCalls,
    /// The live-transaction ceiling (ADR-0037).
    CapacityTransactions,
    /// The RSS ceiling (ADR-0037).
    CapacityRss,
    /// The ingress queue brake (Tier-1).
    Tier1Brake,
    /// The global per-call queue cap (ADR-0022).
    CapShed,
    /// The transaction layer's deferred backlog ceiling (ADR-0037 item 6).
    DeferredBacklog,
    /// The call store failed the initial-INVITE probe (ADR-0023): a 500.
    StoreFault,
    /// The per-call dispatcher discarded the INVITE unrun (its queue full,
    /// the cap reached, or queued behind a release) and answered it 503.
    DispatchDiscard,
}

impl Refusal {
    /// Every reason, in exposition order.
    pub const ALL: [Refusal; 10] = [
        Refusal::BucketEmpty,
        Refusal::PanicElu,
        Refusal::CapacityCalls,
        Refusal::CapacityTransactions,
        Refusal::CapacityRss,
        Refusal::Tier1Brake,
        Refusal::CapShed,
        Refusal::DeferredBacklog,
        Refusal::StoreFault,
        Refusal::DispatchDiscard,
    ];

    /// Stable `reason` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Refusal::BucketEmpty => "bucket_empty",
            Refusal::PanicElu => "panic_elu",
            Refusal::CapacityCalls => "capacity_calls",
            Refusal::CapacityTransactions => "capacity_transactions",
            Refusal::CapacityRss => "capacity_rss",
            Refusal::Tier1Brake => "tier1_brake",
            Refusal::CapShed => "cap_shed",
            Refusal::DeferredBacklog => "deferred_backlog",
            Refusal::StoreFault => "store_fault",
            Refusal::DispatchDiscard => "dispatch_discard",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

impl From<AdmitReason> for Refusal {
    fn from(r: AdmitReason) -> Self {
        match r {
            AdmitReason::BucketEmpty => Refusal::BucketEmpty,
            AdmitReason::PanicElu => Refusal::PanicElu,
        }
    }
}

impl From<Bound> for Refusal {
    fn from(b: Bound) -> Self {
        match b {
            Bound::Calls => Refusal::CapacityCalls,
            Bound::Transactions => Refusal::CapacityTransactions,
            Bound::Rss => Refusal::CapacityRss,
        }
    }
}

const REASONS: usize = Refusal::ALL.len();

/// `[normal, emergency]`, indexed by `usize::from(is_emergency)`.
type ByClass<T> = [T; 2];

#[derive(Debug, Default)]
struct Rows {
    accepted: ByClass<AtomicU64>,
    rejected: [ByClass<AtomicU64>; REASONS],
}

/// The router's new-call decisions. Clone-cheap; clones share the counts.
#[derive(Debug, Clone, Default)]
pub struct NewCallTally {
    rows: Arc<Rows>,
}

impl NewCallTally {
    /// Count one new call admitted: its call is born.
    pub fn accept(&self, is_emergency: bool) {
        self.rows.accepted[usize::from(is_emergency)].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one new call rejected for `reason`.
    pub fn reject(&self, reason: Refusal, is_emergency: bool) {
        self.rows.rejected[reason.index()][usize::from(is_emergency)]
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// A snapshot of every new-call outcome, all tiers composed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NewCallCounts {
    accepted: ByClass<u64>,
    rejected: [ByClass<u64>; REASONS],
}

impl NewCallCounts {
    /// The router's tally, the transaction layer's deferred-backlog refusals
    /// and, when the ingress brake is installed, its shed new calls.
    pub fn read(
        tally: &NewCallTally,
        txn: &TransactionMetrics,
        brake: Option<&Tier1BrakeCounters>,
    ) -> Self {
        Self::compose(
            tally,
            [
                txn.deferred_refused(RefusedClass::Normal),
                txn.deferred_refused(RefusedClass::Emergency),
            ],
            brake.map_or(0, Tier1BrakeCounters::new_calls_shed),
        )
    }

    /// The router's tally plus the deferred-backlog refusals by class
    /// (`[normal, emergency]`) and the brake's sheds, which are never
    /// emergency.
    pub fn compose(tally: &NewCallTally, deferred_backlog: [u64; 2], tier1_brake: u64) -> Self {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut counts = Self {
            accepted: tally.rows.accepted.each_ref().map(load),
            rejected: tally.rows.rejected.each_ref().map(|row| row.each_ref().map(load)),
        };
        for (slot, n) in
            counts.rejected[Refusal::DeferredBacklog.index()].iter_mut().zip(deferred_backlog)
        {
            *slot += n;
        }
        counts.rejected[Refusal::Tier1Brake.index()][0] += tier1_brake;
        counts
    }

    /// New calls of this class admitted.
    pub fn accepted(&self, is_emergency: bool) -> u64 {
        self.accepted[usize::from(is_emergency)]
    }

    /// New calls of this class rejected for `reason`.
    pub fn rejected(&self, reason: Refusal, is_emergency: bool) -> u64 {
        self.rejected[reason.index()][usize::from(is_emergency)]
    }

    /// Every new call rejected, all reasons and classes.
    pub fn rejected_sum(&self) -> u64 {
        self.rejected.iter().flatten().sum()
    }

    /// Every new call counted: accepted plus rejected.
    pub fn total(&self) -> u64 {
        self.accepted.iter().sum::<u64>() + self.rejected_sum()
    }

    /// `b2bua_new_calls_total{outcome,reason,class}`: every series, zeros
    /// included, so a rate reads from the first scrape. An accepted series
    /// carries no `reason`.
    pub fn prometheus_text(&self) -> String {
        let mut s = String::with_capacity(2048);
        s.push_str(
            "# HELP b2bua_new_calls_total New initial INVITEs by admission outcome, once per INVITE \
             (a retransmission is not counted): accepted (the call is born) or rejected by an \
             admission tier (reason); class emergency carries an RFC 4412 Resource-Priority. \
             An INVITE admitted and then shed as a copy by the ingress brake counts accepted and \
             tier1_brake: at most two counts per INVITE.\n\
             # TYPE b2bua_new_calls_total counter\n",
        );
        for (class, emergency) in [("normal", false), ("emergency", true)] {
            let _ = writeln!(
                s,
                "b2bua_new_calls_total{{outcome=\"accepted\",class=\"{class}\"}} {}",
                self.accepted(emergency)
            );
        }
        for reason in Refusal::ALL {
            for (class, emergency) in [("normal", false), ("emergency", true)] {
                let _ = writeln!(
                    s,
                    "b2bua_new_calls_total{{outcome=\"rejected\",reason=\"{}\",class=\"{class}\"}} {}",
                    reason.as_str(),
                    self.rejected(reason, emergency)
                );
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every series is published at 0 before any call, one per class for the
    /// accepts and one per reason and class for the rejects.
    #[test]
    fn every_series_is_published_at_zero() {
        let text = NewCallCounts::compose(&NewCallTally::default(), [0, 0], 0).prometheus_text();
        let series: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(series.len(), 2 + 2 * Refusal::ALL.len());
        assert!(series.iter().all(|l| l.ends_with(" 0")), "{text}");
        assert!(text.contains("b2bua_new_calls_total{outcome=\"accepted\",class=\"normal\"} 0\n"));
        for reason in Refusal::ALL {
            for class in ["normal", "emergency"] {
                let line = format!(
                    "b2bua_new_calls_total{{outcome=\"rejected\",reason=\"{}\",class=\"{class}\"}} 0\n",
                    reason.as_str()
                );
                assert!(text.contains(&line), "missing {line}");
            }
        }
    }

    /// The deferred-backlog refusals land by class and the brake's sheds as
    /// normal, beside the router's own counts.
    #[test]
    fn the_tiers_compose_into_one_count() {
        let tally = NewCallTally::default();
        tally.accept(false);
        tally.accept(true);
        tally.reject(Refusal::CapacityRss, true);
        tally.reject(Refusal::BucketEmpty, false);
        let counts = NewCallCounts::compose(&tally, [3, 1], 2);
        assert_eq!(counts.accepted(false), 1);
        assert_eq!(counts.accepted(true), 1);
        assert_eq!(counts.rejected(Refusal::DeferredBacklog, false), 3);
        assert_eq!(counts.rejected(Refusal::DeferredBacklog, true), 1);
        assert_eq!(counts.rejected(Refusal::Tier1Brake, false), 2);
        assert_eq!(counts.rejected(Refusal::Tier1Brake, true), 0);
        assert_eq!(counts.rejected(Refusal::CapacityRss, true), 1);
        assert_eq!(counts.rejected_sum(), 8);
        assert_eq!(counts.total(), 10);
        let text = counts.prometheus_text();
        assert!(text.contains(
            "b2bua_new_calls_total{outcome=\"rejected\",reason=\"deferred_backlog\",class=\"normal\"} 3\n"
        ));
        assert!(text.contains(
            "b2bua_new_calls_total{outcome=\"rejected\",reason=\"tier1_brake\",class=\"normal\"} 2\n"
        ));
    }

    /// The tier-3 and capacity reasons map onto their labels one to one.
    #[test]
    fn the_tier_reasons_map_to_their_labels() {
        assert_eq!(
            Refusal::from(AdmitReason::BucketEmpty).as_str(),
            AdmitReason::BucketEmpty.as_str()
        );
        assert_eq!(Refusal::from(AdmitReason::PanicElu).as_str(), AdmitReason::PanicElu.as_str());
        for bound in Bound::ALL {
            assert_eq!(Refusal::from(bound).as_str(), format!("capacity_{}", bound.as_str()));
        }
    }
}
