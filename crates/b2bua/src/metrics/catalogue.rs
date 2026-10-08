//! The worker's catalogued metric families. Here: the dispatch drop counts,
//! the overload signal's inputs, the ingress brake's emergency
//! bypass, and the new-call outcome every admission refusal is counted on;
//! the other sources' families sit in the submodules. Each renderer writes
//! its families from these entries; [`SECTIONS`] lists them all, in
//! `/metrics` order.

pub mod capacity;
pub mod limiter;
pub mod udp;
pub mod worker;

pub use sip_txn::catalogue as txn;

use metric_catalogue::{assert_exposition_order, label_values, Dim, Family, Labels};

use super::{discard_label, DISCARD_SITES};
use crate::admission::Class;
use crate::new_calls::Refusal;

const NEW_CALL_CLASS_VALUES: [&str; 2] = [Class::Normal.label(), Class::Emergency.label()];
/// A new call's class: normal or emergency.
pub const CLASS: Dim = Dim::new("class", &NEW_CALL_CLASS_VALUES);

const IN_DIALOG_VALUES: [&str; 1] = [Class::InDialog.label()];
/// The class of an INVITE carrying a To-tag, refused by the backlog alone.
pub const IN_DIALOG: Dim = Dim::new("class", &IN_DIALOG_VALUES);

const BACKLOG_REASON_VALUES: [&str; 1] = [Refusal::DeferredBacklog.as_str()];
/// The one reason an in-dialog INVITE is refused for.
pub const BACKLOG_REASON: Dim = Dim::new("reason", &BACKLOG_REASON_VALUES);

const SITE_VALUES: [&str; DISCARD_SITES.len()] = label_values!(DISCARD_SITES, discard_label);
/// Where a discarded INVITE was answered, indexed like `DISCARD_SITES`.
pub const SITE: Dim = Dim::new("site", &SITE_VALUES);

const REFUSAL_VALUES: [&str; Refusal::ALL.len()] = label_values!(Refusal::ALL, Refusal::as_str);
/// Why a new call was rejected, indexed like [`Refusal::ALL`].
pub const REFUSAL: Dim = Dim::new("reason", &REFUSAL_VALUES);

// Every enum whose `ALL` names a label dimension above lists all its
// variants in declaration order, so no variant goes unlabelled and storage
// indexed by declaration order matches the exposition order.
assert_exposition_order!(
    Refusal: BucketEmpty,
    PanicElu,
    CapacityCalls,
    CapacityTransactions,
    CapacityRss,
    IngressBrake,
    CapShed,
    DeferredBacklog,
    StoreFault,
    DispatchDiscard,
    IdentityInUse,
);
assert_exposition_order!(Class: Normal, Emergency, InDialog);

// ── Per-call dispatcher ──

pub const DISPATCH_CAPPED_REFUSALS: Family = Family::counter(
    "b2bua_dispatch_capped_refusals_total",
    Labels::None,
    "events refused on a call past its lifetime cap: a BYE is answered 200 where refused, any other request but ACK 481 (a CANCEL matching no transaction statelessly); responses and Cancelled notices keep their room and are never refused; a new INVITE is counted on b2bua_new_calls_total instead",
);

pub const DISPATCH_CAPPED_REQUEST_ANSWERED: Family = Family::counter(
    "b2bua_dispatch_capped_request_answered_total",
    Labels::None,
    "non-INVITE requests refused on a call past its lifetime cap and answered where refused: BYE 200, any other 481",
);

pub const DISPATCH_QUEUE_DROPS: Family = Family::counter(
    "b2bua_dispatch_queue_drops_total",
    Labels::None,
    "events dropped: per-call queue full (a non-INVITE request's transaction is forgotten so its retransmission is admitted again; an in-dialog INVITE is answered with a Retry-After); a new INVITE is counted on b2bua_new_calls_total instead",
);

pub const DISPATCH_CAP_DROPS: Family = Family::counter(
    "b2bua_dispatch_cap_drops_total",
    Labels::None,
    "events hitting the global call cap with no queue for their call: a non-INVITE request's transaction is forgotten so its retransmission is admitted again, an in-dialog INVITE is answered 500 with a Retry-After; a new INVITE is counted on b2bua_new_calls_total instead",
);

pub const DISPATCH_RELEASE_DISCARDS: Family = Family::counter(
    "b2bua_dispatch_release_discards_total",
    Labels::None,
    "events offered behind a call's release with no new call waiting there, discarded unrun; a new INVITE, and every event after it, waits for the call's next queue instead",
);

pub const DISPATCH_SATURATION: Family = Family::counter(
    "b2bua_dispatch_saturation_total",
    Labels::None,
    "handler bodies that waited for a permit of the shared pool (global handler concurrency)",
);

pub const DISPATCH_NEW_CALL_SHARE_WAITS: Family = Family::counter(
    "b2bua_dispatch_new_call_share_waits_total",
    Labels::None,
    "new-call handler bodies that waited for a permit of the new-call share",
);

pub const DISPATCH_INVITE_DISCARD_ANSWERED: Family = Family::counter(
    "b2bua_dispatch_invite_discard_answered_total",
    Labels::Product(&[SITE]),
    "in-dialog INVITEs whose handler body was discarded unrun, answered at the discard site: 481 behind a terminated call's release or past the call's lifetime cap, else 500 with Retry-After (no room, a self-release, an orphan release)",
);

// ── The overload signal: rung inputs ──

pub const OVERLOAD_TOKEN_BUCKET_LEVEL: Family = Family::gauge(
    "b2bua_overload_token_bucket_level",
    Labels::None,
    "Current CPS token-bucket level (tokens remaining, never below 0).",
);

pub const OVERLOAD_ELU_EWMA: Family = Family::gauge(
    "b2bua_overload_elu_ewma",
    Labels::None,
    "Decision INPUT: EWMA-smoothed Event Loop Utilization (0..1) published on X-Overload; the panic-ELU backstop fires above the threshold.",
);

pub const OVERLOAD_GC_FRACTION: Family = Family::gauge(
    "b2bua_overload_gc_fraction",
    Labels::None,
    "Decision INPUT: EWMA-smoothed GC pause fraction (0..1) published on X-Overload (structurally 0 on Rust — no managed GC).",
);

// ── Ingress brake ──

pub const UDP_INGRESS_BRAKE_EMERGENCY_BYPASSED: Family = Family::counter(
    "b2bua_udp_ingress_brake_emergency_bypassed_total",
    Labels::None,
    "New emergency INVITEs that crossed the ingress brake's threshold and passed it (the brake admits them). Emergency traffic skipping the gate under flood.",
);

// ── Every new call's admission outcome ──

/// Block 0: accepted, by class. Block 1: cancelled, by class. Block 2:
/// rejected, by reason and class. Block 3: an INVITE carrying a To-tag
/// refused by the backlog.
pub const NEW_CALLS: Family = Family::counter(
    "b2bua_new_calls_total",
    Labels::Union(&[
        &[Dim::new("outcome", &["accepted"]), CLASS],
        &[Dim::new("outcome", &["cancelled"]), CLASS],
        &[Dim::new("outcome", &["rejected"]), REFUSAL, CLASS],
        &[Dim::new("outcome", &["rejected"]), BACKLOG_REASON, IN_DIALOG],
    ]),
    "New initial INVITEs by admission outcome, once per INVITE (a copy is not counted): accepted (the call is born), cancelled (its caller CANCELed it before its turn ran: the call is born and ends with no decision and no limiter admit) or rejected (reason: a rung of the admission ladder, the store-fault 500, or a dispatch discard of the INVITE's turn); class emergency carries an RFC 4412 Resource-Priority, class in_dialog is an INVITE carrying a To-tag the deferred backlog refused.",
);

pub const NEW_CALL_REFUSED_COPIES: Family = Family::counter(
    "b2bua_new_call_refused_copies_total",
    Labels::None,
    "Copies answered with a refusal and never counted as a new call: a copy of a refused new INVITE answered that refusal again at the ingress brake or the transaction layer, or a copy of a call already here (live, or admitted and not born yet: same Call-ID, From-tag and CSeq) that the router answered 482 or discarded unrun.",
);

/// The core counter set, as `B2buaMetrics::prometheus_text` writes it.
pub const WORKER: &[Family] = &[
    worker::MESSAGE_CAP_LIFETIME_CROSSED,
    DISPATCH_CAPPED_REFUSALS,
    DISPATCH_CAPPED_REQUEST_ANSWERED,
    worker::MESSAGE_CAP_TERMINATED,
    DISPATCH_QUEUE_DROPS,
    DISPATCH_CAP_DROPS,
    DISPATCH_RELEASE_DISCARDS,
    DISPATCH_SATURATION,
    DISPATCH_NEW_CALL_SHARE_WAITS,
    worker::CALL_CREATIONS,
    worker::CALL_REMOVALS,
    worker::HANDLER_TIMEOUTS,
    worker::FORCE_PURGE,
    worker::FAST_REJECT_TERMINATING,
    worker::CDR_WRITTEN,
    worker::CDR_DROPPED,
    worker::DECISION_DROPPED_CANCELLED,
    worker::TERMINATION_UNRECORDED,
    worker::SECOND_FINAL_REFUSED,
    worker::PROVISIONAL_AFTER_FINAL_REFUSED,
    worker::GOING_AWAY_ABSORBED,
    worker::LATE_PRACK_ANSWERED,
    worker::OTHER_INCARNATION_DROPPED,
    worker::STORE_FAULT_REJECTED,
    worker::STORE_FAULT_AUDIT_SKIPPED,
    worker::HANDLER_PANICS,
    worker::REAPER_SWEEPS,
    worker::REAPER_SWEEP_PANICS,
    worker::REPLICA_REAP_PANICS,
    worker::STORE_POISONED_LOCK_RECOVERIES,
    worker::REAPER_VERDICTS,
    worker::REAPER_DISCHARGED,
    worker::REPL_FLUSH_PROPAGATED,
    worker::REPL_TAKEOVER_RESOLVED,
    worker::REPL_TAKEOVER_HYDRATED,
    worker::REPL_REVERSE_FLUSH_REFUSED,
    worker::REPL_TAKEOVER_REFUSED_TERMINATED,
    worker::REPL_RECLAIMED,
    worker::REPL_SELF_RELEASE,
    worker::REPL_TERMINAL_LOST,
    worker::REPL_BOOTSTRAP_SEEDED,
    worker::REPL_BOOTSTRAP_STALLED,
    worker::REQUESTS,
    worker::REQUESTS_OVERFLOW,
    worker::RESPONSES,
    worker::RESPONSES_OVERFLOW,
    worker::REQUESTS_OUT,
    worker::REQUESTS_OUT_OVERFLOW,
    worker::RETRANSMITS,
    worker::UNROUTABLE_DROPPED,
    worker::UNROUTABLE_REFUSED,
    worker::UNROUTABLE_REFUSED_OVERFLOW,
    worker::UNROUTABLE_INTERNAL,
    worker::UNROUTABLE_INTERNAL_OVERFLOW,
    worker::REPEAT_GIVE_UPS,
    worker::REPL_QUIET_TURNS,
    worker::REPL_APPLIED,
    worker::REPL_NOOPS_SENT,
    worker::REPL_FORWARD_FLUSH_REFUSED,
    worker::DRAIN_EXITS,
    worker::DRAIN_SECONDS,
    limiter::REQUESTS,
    limiter::FAILURES,
    limiter::ADMIT_RELEASED,
    limiter::UNCOUNTED_CALLS,
    limiter::REFRESH_ANSWERS,
    limiter::REFRESH_ANSWERS_DISCARDED,
    limiter::REFRESH_KEYS_SENT,
    limiter::REFRESH_RETRIES,
    limiter::REFRESH_GIVEN_UP,
    limiter::REFRESH_DUE,
    limiter::RELEASE_RETRIES,
    limiter::RELEASE_GIVEN_UP,
    limiter::RELEASE_QUEUE_DEPTH,
    limiter::RELEASE_FLUSHES,
    limiter::RELEASE_FLUSH_SECONDS,
    limiter::TASK_RESTARTS,
    limiter::BREAKER_OPEN,
    limiter::BREAKER_TRANSITIONS,
    limiter::LEASE_SECONDS,
    limiter::REFRESH_PERIOD_SECONDS,
    limiter::LEASE_TOO_SHORT,
    limiter::REFRESH_PERIOD_CLAMPED,
    worker::CALL_REMOVALS_BY_CLASS,
    worker::DISPATCH_PAST_BOUND,
    worker::DISPATCH_OVERFLOW_REFUSED,
    worker::DISPATCH_OVERFLOW_DEPTH,
    worker::CALLS_NEAR_LIFETIME_CAP,
    DISPATCH_INVITE_DISCARD_ANSWERED,
    worker::ACTIVE_CALLS,
    worker::TIMER_QUEUE_LEN,
    worker::TIMER_LIVE,
    worker::CLOCK_WALL_DIVERGENCE_MS,
    worker::STORE_CALLS,
    worker::STORE_SIP_INDEX,
    worker::STORE_INDEXED,
    worker::STORE_LOCKS,
    worker::STORE_TAKEOVER_AT,
    worker::STORE_TOUCHED,
    worker::STORE_BODIES,
    worker::STORE_IDX_ENTRIES,
    worker::STORE_TOMBSTONES,
    worker::REPL_META,
    worker::REPL_META_BACKUP,
    worker::REPL_CHANGELOG_ENTRIES,
    worker::REPL_CHANGELOG_PEERS,
    worker::WITHDRAWN_RUNNING,
    worker::REPL_PEERS_PULLED_NOT_READY,
    worker::REPL_BOOTSTRAP_LAST_APPLIED,
    worker::REPL_RECLAIM_SCANNED,
    worker::REPL_RECLAIM_MATERIALIZED,
    worker::CENSUS_CDR_EVENTS,
    worker::CENSUS_PENDING_REQUESTS,
    worker::CENSUS_PENDING_REQUESTS_MAX,
    worker::CENSUS_DIALOGS,
    worker::CENSUS_ROUTE_SET,
    worker::CENSUS_TIMERS,
    worker::CENSUS_TAG_MAP,
    worker::CENSUS_B_LEGS,
    worker::SM_CURSORS,
    worker::PEER_FAILURES,
    worker::PEER_FAILURES_OVERFLOW,
];

/// The transaction layer, as the worker renders it.
pub const TXN: &[Family] = &[
    txn::ACTIVE_TRANSACTIONS,
    txn::TIMER_QUEUE_LEN,
    txn::RETRANSMIT_BUF_BYTES,
    txn::SERVER_FINAL_UNSEEN_BRANCH,
    txn::PARSE_ERRORS,
    txn::SEND_ERRORS,
    txn::EVENT_QUEUE_DEPTH,
    txn::EVENT_QUEUE_CAPACITY,
    txn::EVENT_QUEUE_DROPS,
    txn::EVENT_QUEUE_DEFERRALS,
    txn::EVENT_QUEUE_DEFERRED,
    txn::DEFERRED_SWEPT,
    txn::SWEEP_REAPED,
    txn::UNANSWERED_FORGOTTEN,
    txn::FORGET_REFUSED,
    txn::RELEASED_UNANSWERED_INVITES_ANSWERED,
    txn::RELEASED_UNANSWERED_FORGOTTEN,
    txn::RETRANSMITS,
    txn::RETRANSMITS_OVERFLOW,
];

/// The UDP transport and its ingress brake.
pub const UDP: &[Family] = &[
    UDP_INGRESS_BRAKE_EMERGENCY_BYPASSED,
    udp::QUEUE_DEPTH,
    udp::QUEUE_MAX,
    udp::TAIL_DROPPED,
    udp::SEND_WOULD_BLOCK,
    udp::KERNEL_RX_DROPPED,
];

/// The overload signal: rung inputs.
pub const OVERLOAD: &[Family] =
    &[OVERLOAD_TOKEN_BUCKET_LEVEL, OVERLOAD_ELU_EWMA, OVERLOAD_GC_FRACTION];

/// The capacity gate.
pub const CAPACITY: &[Family] = &[
    capacity::CAPACITY_LEVEL,
    capacity::CAPACITY_RSS_BYTES,
    capacity::CAPACITY_CEILING,
    capacity::REPL_BACKUP_SHED,
];

/// Every new call's admission outcome.
pub const NEW_CALL_OUTCOMES: &[Family] = &[NEW_CALLS, NEW_CALL_REFUSED_COPIES];

/// The admission families: the dispatch drop counts, the ingress brake's
/// bypass, the overload signal and every new call's outcome — what a new
/// INVITE's admission or refusal can move.
pub const ADMISSION: &[Family] = &[
    DISPATCH_CAPPED_REFUSALS,
    DISPATCH_CAPPED_REQUEST_ANSWERED,
    DISPATCH_QUEUE_DROPS,
    DISPATCH_CAP_DROPS,
    DISPATCH_RELEASE_DISCARDS,
    DISPATCH_SATURATION,
    DISPATCH_NEW_CALL_SHARE_WAITS,
    DISPATCH_INVITE_DISCARD_ANSWERED,
    UDP_INGRESS_BRAKE_EMERGENCY_BYPASSED,
    OVERLOAD_TOKEN_BUCKET_LEVEL,
    OVERLOAD_ELU_EWMA,
    OVERLOAD_GC_FRACTION,
    NEW_CALLS,
    NEW_CALL_REFUSED_COPIES,
];

/// The worker's sections, in `/metrics` order.
pub const SECTIONS: &[&[Family]] = &[WORKER, TXN, UDP, OVERLOAD, CAPACITY, NEW_CALL_OUTCOMES];

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::capacity::{self, CapacityGate};
    use crate::dispatch::Discard;
    use crate::ingress_brake::IngressBrakeCounters;
    use crate::metrics::{B2buaMetrics, UdpTransportMetrics};
    use crate::new_calls::{NewCallCounts, NewCallTally, StatelessCounts};
    use crate::overload::OverloadSignal;

    fn udp() -> UdpTransportMetrics {
        udp_with(IngressBrakeCounters::new())
    }

    fn udp_with(brake: IngressBrakeCounters) -> UdpTransportMetrics {
        UdpTransportMetrics::new(
            8,
            brake,
            Arc::new(|| 0),
            Arc::new(|| 0),
            Arc::new(|| 0),
            Arc::new(|| 0),
        )
    }

    fn gate() -> CapacityGate {
        CapacityGate::new(Arc::new(capacity::simulated().0))
    }

    /// The value of the sample of `family` carrying exactly `labels`.
    fn value_of(text: &str, family: &Family, labels: &[(&str, &str)]) -> String {
        let pairs: Vec<String> = labels.iter().map(|(n, v)| format!("{n}=\"{v}\"")).collect();
        let prefix = if pairs.is_empty() {
            format!("{} ", family.name)
        } else {
            format!("{}{{{}}} ", family.name, pairs.join(","))
        };
        let line = text.lines().find(|l| l.starts_with(&prefix));
        line.unwrap_or_else(|| panic!("no sample {prefix:?}"))[prefix.len()..].to_owned()
    }

    /// Every family this crate renders is in its renderer's text exactly as
    /// declared, every declared label set at 0 before any event (the
    /// transaction layer's are rendered by the runner).
    #[test]
    fn every_family_renders_as_declared() {
        let core = B2buaMetrics::new();
        let udp = udp();
        let text = [
            core.prometheus_text(),
            udp.prometheus_text(),
            OverloadSignal::new(Arc::new(load_shed::simulated().0)).prometheus_text(),
            gate().prometheus_text(),
            NewCallCounts::compose(
                core.new_calls(),
                StatelessCounts::default(),
                StatelessCounts::default(),
            )
            .prometheus_text(),
        ]
        .concat();
        for family in
            SECTIONS.iter().flat_map(|s| s.iter()).filter(|f| !TXN.iter().any(|t| t.name == f.name))
        {
            if let Err(mismatch) = family.check(&text) {
                panic!("{mismatch}\n{text}");
            }
        }
    }

    /// Each label set of a new call carries the count of its own outcome,
    /// reason and class.
    #[test]
    fn a_new_call_series_carries_its_own_outcome_reason_and_class() {
        let tally = NewCallTally::default();
        tally.accept(Class::Emergency);
        (0..5).for_each(|_| tally.cancel(Class::Normal));
        let mut n = 1;
        for reason in Refusal::ALL {
            for class in [Class::Normal, Class::Emergency] {
                n += 1;
                (0..n).for_each(|_| tally.reject(reason, class));
            }
        }
        let backlog = StatelessCounts { refused: [0, 0, 7], copies: 3 };
        let counts = NewCallCounts::compose(&tally, backlog, StatelessCounts::default());
        let text = counts.prometheus_text();
        for class in [Class::Normal, Class::Emergency] {
            let labels = [("outcome", "accepted"), ("class", class.label())];
            let got = value_of(&text, &NEW_CALLS, &labels);
            assert_eq!(got, counts.accepted(class).to_string(), "{labels:?}");
            let labels = [("outcome", "cancelled"), ("class", class.label())];
            let got = value_of(&text, &NEW_CALLS, &labels);
            assert_eq!(got, counts.cancelled(class).to_string(), "{labels:?}");
            for reason in Refusal::ALL {
                let labels = [
                    ("outcome", "rejected"),
                    ("reason", reason.as_str()),
                    ("class", class.label()),
                ];
                let got = value_of(&text, &NEW_CALLS, &labels);
                assert_eq!(got, counts.rejected(reason, class).to_string(), "{labels:?}");
            }
        }
        let labels =
            [("outcome", "rejected"), ("reason", "deferred_backlog"), ("class", "in_dialog")];
        assert_eq!(value_of(&text, &NEW_CALLS, &labels), "7");
        assert_eq!(value_of(&text, &NEW_CALL_REFUSED_COPIES, &[]), "3");
    }

    /// Each discard site carries the count of its own answered INVITEs.
    #[test]
    fn a_discard_site_series_carries_its_own_count() {
        let m = B2buaMetrics::new();
        for (i, why) in DISCARD_SITES.into_iter().enumerate() {
            (0..=i).for_each(|_| m.bump_invite_discard_answered(why));
        }
        let text = m.prometheus_text();
        for why in DISCARD_SITES {
            let labels = [("site", discard_label(why))];
            let got = value_of(&text, &DISPATCH_INVITE_DISCARD_ANSWERED, &labels);
            assert_eq!(got, m.invite_discard_answered_of_total(why).to_string(), "{labels:?}");
        }
        assert_eq!(m.invite_discard_answered_of_total(Discard::Capped), 4);
    }

    /// Each dispatcher counter carries its own count.
    #[test]
    fn a_core_refusal_counter_carries_its_own_count() {
        let m = B2buaMetrics::new();
        let bumps: [(&Family, &dyn Fn()); 7] = [
            (&DISPATCH_CAPPED_REFUSALS, &|| m.bump_capped_refusal()),
            (&DISPATCH_CAPPED_REQUEST_ANSWERED, &|| m.bump_capped_request_answered()),
            (&DISPATCH_QUEUE_DROPS, &|| m.bump_queue_drop()),
            (&DISPATCH_CAP_DROPS, &|| m.bump_cap_drop()),
            (&DISPATCH_RELEASE_DISCARDS, &|| m.bump_release_discard()),
            (&DISPATCH_SATURATION, &|| m.bump_saturation()),
            (&DISPATCH_NEW_CALL_SHARE_WAITS, &|| m.bump_new_call_share_wait()),
        ];
        for (n, (_, bump)) in bumps.iter().enumerate() {
            (0..=n).for_each(|_| bump());
        }
        let text = m.prometheus_text();
        for (n, (family, _)) in bumps.iter().enumerate() {
            assert_eq!(value_of(&text, family, &[]), (n + 1).to_string(), "{}", family.name);
        }
    }

    /// The ingress brake's bypass counter carries its own count.
    #[test]
    fn the_brake_bypass_counter_carries_its_own_count() {
        let brake = IngressBrakeCounters::new();
        (0..7).for_each(|_| brake.record_emergency_bypass());
        let text = udp_with(brake).prometheus_text();
        assert_eq!(value_of(&text, &UDP_INGRESS_BRAKE_EMERGENCY_BYPASSED, &[]), "7");
    }

    /// Each overload series carries its own count or reading.
    #[tokio::test(start_paused = true)]
    async fn an_overload_series_carries_its_own_value() {
        let (sampler, load) = load_shed::simulated();
        let sig = OverloadSignal::new(Arc::new(sampler));
        sig.configure_admission(&crate::config::B2buaConfig {
            cps_bucket_size: 2,
            cps_bucket_rate: 0,
            ..Default::default()
        });
        load.set_elu(0.9);
        load.set_gc_fraction(0.25);
        sig.sample();
        sig.spend_token();
        let m = sig.metrics();
        assert_ne!(m.elu_ewma, m.gc_fraction_ewma);
        assert_ne!(m.elu_ewma, m.token_bucket_level);
        assert_eq!(m.token_bucket_level, 1.0, "the token spent");
        let text = sig.prometheus_text();
        let level = value_of(&text, &OVERLOAD_TOKEN_BUCKET_LEVEL, &[]);
        assert_eq!(level, m.token_bucket_level.to_string());
        assert_eq!(value_of(&text, &OVERLOAD_ELU_EWMA, &[]), m.elu_ewma.to_string());
        assert_eq!(value_of(&text, &OVERLOAD_GC_FRACTION, &[]), m.gc_fraction_ewma.to_string());
    }
}
