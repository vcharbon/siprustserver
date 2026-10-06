//! The admission outcome of every new call the worker is offered: accepted
//! (the call is born and runs the whole call path), cancelled (its caller
//! CANCELed it before its turn ran: the call is born and ends at once, with
//! no decision and no limiter admit) or refused by a rung of the admission
//! ladder ([`crate::admission`]) before any call state exists, and why.
//!
//! A new call is a new initial INVITE (no To-tag). Its first refusal is
//! counted once, on `b2bua_new_calls_total`, in the class it was judged in;
//! a copy of it refused again — at the ingress brake or the transaction
//! layer, which share one memo of the INVITEs refused in the last 64·T1
//! ([`sip_txn::InviteRefusals`]) — is counted on
//! `b2bua_new_call_refused_copies_total`, never as a new call, as is a copy
//! of a call already here (live, or admitted and not born yet, on the same
//! CSeq) the router answers 482 or discards unrun. A copy a live server
//! transaction holds is that transaction's to answer. An INVITE carrying a
//! To-tag the backlog refuses is counted under class `in_dialog`.
//!
//! Three sources compose the count ([`NewCallCounts::read`]): the router's
//! [`NewCallTally`] (its rungs, the store-fault 500, the discards, every
//! accept and cancelled setup), the transaction layer's backlog refusals and copies, and the
//! ingress brake's ([`IngressBrakeCounters`]). An accepted call may still end
//! with a final the call path authors (a routing reject, a malformed-INVITE
//! 400): that is the call's outcome, not an admission one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sip_txn::TransactionMetrics;

use crate::admission::Class;
use crate::ingress_brake::IngressBrakeCounters;
use crate::metrics::catalogue;

/// Why a new call was refused: the `reason` label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Refusal {
    /// The CPS token bucket was empty.
    BucketEmpty,
    /// The worker's EWMA-ELU was above the panic backstop.
    PanicElu,
    /// The live-call ceiling (ADR-0037).
    CapacityCalls,
    /// The live-transaction ceiling (ADR-0037).
    CapacityTransactions,
    /// The RSS ceiling (ADR-0037).
    CapacityRss,
    /// The ingress queue brake.
    IngressBrake,
    /// The global per-call queue cap, or for a normal new INVITE the cap
    /// less the new-call headroom (ADR-0022, ADR-0037 item 10).
    CapShed,
    /// The transaction layer's deferred backlog ceiling (ADR-0037 item 6).
    DeferredBacklog,
    /// The call store failed the initial-INVITE probe (ADR-0023): a 500.
    StoreFault,
    /// The per-call dispatcher discarded the INVITE unrun (its queue full,
    /// or the cap reached) and answered it 503.
    DispatchDiscard,
    /// A call on the INVITE's Call-ID and From-tag, with another CSeq, is
    /// still here: answered 500 with a Retry-After (RFC 3261 §14.2).
    IdentityInUse,
}

impl Refusal {
    /// Every reason, in exposition order.
    pub const ALL: [Refusal; 11] = [
        Refusal::BucketEmpty,
        Refusal::PanicElu,
        Refusal::CapacityCalls,
        Refusal::CapacityTransactions,
        Refusal::CapacityRss,
        Refusal::IngressBrake,
        Refusal::CapShed,
        Refusal::DeferredBacklog,
        Refusal::StoreFault,
        Refusal::DispatchDiscard,
        Refusal::IdentityInUse,
    ];

    /// Stable `reason` label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Refusal::BucketEmpty => "bucket_empty",
            Refusal::PanicElu => "panic_elu",
            Refusal::CapacityCalls => "capacity_calls",
            Refusal::CapacityTransactions => "capacity_transactions",
            Refusal::CapacityRss => "capacity_rss",
            Refusal::IngressBrake => "ingress_brake",
            Refusal::CapShed => "cap_shed",
            Refusal::DeferredBacklog => "deferred_backlog",
            Refusal::StoreFault => "store_fault",
            Refusal::DispatchDiscard => "dispatch_discard",
            Refusal::IdentityInUse => "identity_in_use",
        }
    }

    /// The reason's position in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }
}

const REASONS: usize = Refusal::ALL.len();
const CLASSES: usize = Class::ALL.len();

/// The new-call classes an accept is counted in.
const NEW_CALL_CLASSES: [Class; 2] = [Class::Normal, Class::Emergency];

#[derive(Debug, Default)]
struct Rows {
    accepted: [AtomicU64; CLASSES],
    cancelled: [AtomicU64; CLASSES],
    rejected: [[AtomicU64; CLASSES]; REASONS],
    refused_copies: AtomicU64,
}

/// The router's new-call decisions. Clone-cheap; clones share the counts.
#[derive(Debug, Clone, Default)]
pub struct NewCallTally {
    rows: Arc<Rows>,
}

impl NewCallTally {
    /// Count one new call admitted: its call is born.
    pub fn accept(&self, class: Class) {
        self.rows.accepted[class.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one new call its caller CANCELed before its turn ran: its call
    /// is born and ends without a decision.
    pub fn cancel(&self, class: Class) {
        self.rows.cancelled[class.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one new call refused for `reason`.
    pub fn reject(&self, reason: Refusal, class: Class) {
        self.rows.rejected[reason.index()][class.index()].fetch_add(1, Ordering::Relaxed);
    }

    /// Count one copy of a call already here refused: no new call.
    pub fn refuse_copy(&self) {
        self.rows.refused_copies.fetch_add(1, Ordering::Relaxed);
    }
}

/// The first refusals and refused copies of the stages that judge an INVITE
/// before any transaction holds it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StatelessCounts {
    /// First refusals by class, in [`Class::ALL`] order.
    pub refused: [u64; CLASSES],
    /// Copies refused again.
    pub copies: u64,
}

/// A snapshot of every new-call outcome, all sources composed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NewCallCounts {
    accepted: [u64; CLASSES],
    cancelled: [u64; CLASSES],
    rejected: [[u64; CLASSES]; REASONS],
    refused_copies: u64,
}

impl NewCallCounts {
    /// The router's tally, the transaction layer's backlog and, when the
    /// ingress brake is installed, the brake's.
    pub fn read(
        tally: &NewCallTally,
        txn: &TransactionMetrics,
        brake: Option<&IngressBrakeCounters>,
    ) -> Self {
        let backlog = StatelessCounts {
            refused: Class::ALL.map(|class| txn.deferred_refused(class)),
            copies: txn.refused_copies(),
        };
        Self::compose(tally, backlog, brake.map(IngressBrakeCounters::counts).unwrap_or_default())
    }

    /// The router's tally plus the backlog's and the brake's counts.
    pub fn compose(tally: &NewCallTally, backlog: StatelessCounts, brake: StatelessCounts) -> Self {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut counts = Self {
            accepted: tally.rows.accepted.each_ref().map(load),
            cancelled: tally.rows.cancelled.each_ref().map(load),
            rejected: tally.rows.rejected.each_ref().map(|row| row.each_ref().map(load)),
            refused_copies: load(&tally.rows.refused_copies) + backlog.copies + brake.copies,
        };
        for (reason, stage) in [(Refusal::DeferredBacklog, backlog), (Refusal::IngressBrake, brake)]
        {
            for (slot, n) in counts.rejected[reason.index()].iter_mut().zip(stage.refused) {
                *slot += n;
            }
        }
        counts
    }

    /// New calls of this class admitted.
    pub fn accepted(&self, class: Class) -> u64 {
        self.accepted[class.index()]
    }

    /// New calls of this class CANCELed before their turn ran.
    pub fn cancelled(&self, class: Class) -> u64 {
        self.cancelled[class.index()]
    }

    /// INVITEs of this class refused for `reason`.
    pub fn rejected(&self, reason: Refusal, class: Class) -> u64 {
        self.rejected[reason.index()][class.index()]
    }

    /// Copies of a refused INVITE refused again.
    pub fn refused_copies(&self) -> u64 {
        self.refused_copies
    }

    /// Every INVITE refused, all reasons and classes.
    pub fn rejected_sum(&self) -> u64 {
        self.rejected.iter().flatten().sum()
    }

    /// Every INVITE counted: accepted, cancelled and refused.
    pub fn total(&self) -> u64 {
        self.accepted.iter().chain(&self.cancelled).sum::<u64>() + self.rejected_sum()
    }

    /// `b2bua_new_calls_total{outcome,reason,class}`
    /// ([`catalogue::NEW_CALLS`]) and
    /// `b2bua_new_call_refused_copies_total`
    /// ([`catalogue::NEW_CALL_REFUSED_COPIES`]): every series, zeros
    /// included, so a rate reads from the first scrape. An accepted or a
    /// cancelled series carries no `reason`.
    pub fn prometheus_text(&self) -> String {
        let mut s = catalogue::NEW_CALLS.text(|series| match series.block() {
            0 => self.accepted(NEW_CALL_CLASSES[series.index(&catalogue::CLASS)]),
            1 => self.cancelled(NEW_CALL_CLASSES[series.index(&catalogue::CLASS)]),
            2 => self.rejected(
                Refusal::ALL[series.index(&catalogue::REFUSAL)],
                NEW_CALL_CLASSES[series.index(&catalogue::CLASS)],
            ),
            _ => self.rejected(Refusal::DeferredBacklog, Class::InDialog),
        });
        catalogue::NEW_CALL_REFUSED_COPIES.render_value(&mut s, self.refused_copies);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(normal: u64, emergency: u64, in_dialog: u64, copies: u64) -> StatelessCounts {
        StatelessCounts { refused: [normal, emergency, in_dialog], copies }
    }

    /// Every series is published at 0 before any call: one per class for the
    /// accepts and for the cancelled setups, one per reason and class for the refusals, the backlog's
    /// in-dialog refusals, and the refused copies.
    #[test]
    fn every_series_is_published_at_zero() {
        let text = NewCallCounts::default().prometheus_text();
        let series: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(series.len(), 2 + 2 + 2 * Refusal::ALL.len() + 1 + 1);
        assert!(series.iter().all(|l| l.ends_with(" 0")), "{text}");
        assert!(text.contains("b2bua_new_calls_total{outcome=\"accepted\",class=\"normal\"} 0\n"));
        assert!(
            text.contains("b2bua_new_calls_total{outcome=\"cancelled\",class=\"emergency\"} 0\n")
        );
        for reason in Refusal::ALL {
            for class in ["normal", "emergency"] {
                let line = format!(
                    "b2bua_new_calls_total{{outcome=\"rejected\",reason=\"{}\",class=\"{class}\"}} 0\n",
                    reason.as_str()
                );
                assert!(text.contains(&line), "missing {line}");
            }
        }
        assert!(text.contains(
            "b2bua_new_calls_total{outcome=\"rejected\",reason=\"deferred_backlog\",class=\"in_dialog\"} 0\n"
        ));
        assert!(text.contains("b2bua_new_call_refused_copies_total 0\n"));
    }

    /// The backlog's and the brake's first refusals land by class, their
    /// copies on the copies count, beside the router's own counts.
    #[test]
    fn the_sources_compose_into_one_count() {
        let tally = NewCallTally::default();
        tally.accept(Class::Normal);
        tally.accept(Class::Emergency);
        tally.cancel(Class::Normal);
        tally.reject(Refusal::CapacityRss, Class::Emergency);
        tally.reject(Refusal::BucketEmpty, Class::Normal);
        let counts = NewCallCounts::compose(&tally, stage(3, 1, 2, 4), stage(2, 0, 0, 5));
        assert_eq!(counts.accepted(Class::Normal), 1);
        assert_eq!(counts.accepted(Class::Emergency), 1);
        assert_eq!(counts.cancelled(Class::Normal), 1);
        assert_eq!(counts.rejected(Refusal::DeferredBacklog, Class::Normal), 3);
        assert_eq!(counts.rejected(Refusal::DeferredBacklog, Class::Emergency), 1);
        assert_eq!(counts.rejected(Refusal::DeferredBacklog, Class::InDialog), 2);
        assert_eq!(counts.rejected(Refusal::IngressBrake, Class::Normal), 2);
        assert_eq!(counts.rejected(Refusal::IngressBrake, Class::Emergency), 0);
        assert_eq!(counts.rejected(Refusal::CapacityRss, Class::Emergency), 1);
        assert_eq!(counts.rejected_sum(), 10);
        assert_eq!(counts.total(), 13);
        assert_eq!(counts.refused_copies(), 9);
        let text = counts.prometheus_text();
        assert!(text.contains(
            "b2bua_new_calls_total{outcome=\"rejected\",reason=\"deferred_backlog\",class=\"normal\"} 3\n"
        ));
        assert!(text.contains(
            "b2bua_new_calls_total{outcome=\"rejected\",reason=\"deferred_backlog\",class=\"in_dialog\"} 2\n"
        ));
        assert!(text.contains(
            "b2bua_new_calls_total{outcome=\"rejected\",reason=\"ingress_brake\",class=\"normal\"} 2\n"
        ));
        assert!(text.contains("b2bua_new_calls_total{outcome=\"cancelled\",class=\"normal\"} 1\n"));
        assert!(text.contains("b2bua_new_call_refused_copies_total 9\n"));
    }
}
