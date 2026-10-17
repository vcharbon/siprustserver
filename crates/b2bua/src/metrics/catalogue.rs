//! The worker's catalogued metric families. Here: the dispatch's answered
//! INVITE discards, the overload signal's inputs, the ingress brake's
//! emergency bypass, and the new-call outcome every admission refusal is
//! counted on; the dispatch drop counts are plain counters ([`plain`]), and
//! the other sources' families sit in the other submodules. Each renderer writes
//! its families from these entries; [`SECTIONS`] lists them all, in
//! `/metrics` order.

pub mod capacity;
pub mod limiter;
pub mod plain;
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
    Cordoned,
);
assert_exposition_order!(Class: Normal, Emergency, InDialog);

// ── Per-call dispatcher ──

pub const DISPATCH_INVITE_DISCARD_ANSWERED: Family = Family::counter(
    "b2bua_dispatch_invite_discard_answered_total",
    Labels::Product(&[SITE]),
    "in-dialog INVITEs whose handler body was discarded unrun, answered at the discard site: 481 behind a terminated call's release or past the call's lifetime cap, else 500 with Retry-After (no room, a self-release, an orphan release)",
);

// ── The overload signal: rung inputs ──

pub const OVERLOAD_TOKEN_BUCKET_LEVEL: Family = Family::gauge(
    "b2bua_overload_token_bucket_level",
    Labels::None,
    "Current CPS token-bucket level (tokens remaining, never below 0); 0 too while b2bua_overload_cps_gate_enabled is 0.",
);

pub const OVERLOAD_CPS_GATE_ENABLED: Family = Family::gauge(
    "b2bua_overload_cps_gate_enabled",
    Labels::None,
    "1 while the CPS bucket rung gates new calls, 0 when B2BUA_CPS_BUCKET_SIZE=0 turns it off (every call passes the bucket).",
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

/// The core counter set, as `B2buaMetrics::prometheus_text` writes it: the
/// plain counters first, in row order, then the rest.
pub const WORKER: &[Family] = &WORKER_FAMILIES;

const WORKER_FAMILIES: [Family; plain::FAMILIES.len() + WORKER_REST.len()] =
    concat(plain::FAMILIES, WORKER_REST);

const WORKER_REST: &[Family] = &[
    worker::CALL_CREATIONS,
    worker::CALL_REMOVALS,
    worker::STORE_POISONED_LOCK_RECOVERIES,
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

/// `a` then `b`.
const fn concat<const N: usize>(a: &[Family], b: &[Family]) -> [Family; N] {
    let mut out = [a[0]; N];
    let mut i = 0;
    while i < N {
        out[i] = if i < a.len() { a[i] } else { b[i - a.len()] };
        i += 1;
    }
    out
}

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
pub const OVERLOAD: &[Family] = &[
    OVERLOAD_TOKEN_BUCKET_LEVEL,
    OVERLOAD_CPS_GATE_ENABLED,
    OVERLOAD_ELU_EWMA,
    OVERLOAD_GC_FRACTION,
];

/// The capacity gate: its level, RSS, ceiling and the backup replicas it shed.
pub const CAPACITY: &[Family] = &[
    capacity::CAPACITY_LEVEL,
    capacity::CAPACITY_RSS_BYTES,
    capacity::CAPACITY_CEILING,
    capacity::REPL_BACKUP_SHED,
];

/// Whether the worker's loops turn.
pub const LOOPS: &[Family] = &[worker::LOOP_STALLED];

/// Whether an operator cordoned the worker.
pub const READINESS: &[Family] = &[worker::CORDONED];

/// Every new call's admission outcome.
pub const NEW_CALL_OUTCOMES: &[Family] = &[NEW_CALLS, NEW_CALL_REFUSED_COPIES];

/// The admission families: the dispatch drop counts, the ingress brake's
/// bypass, the overload signal and every new call's outcome — what a new
/// INVITE's admission or refusal can move.
pub const ADMISSION: &[Family] = &[
    plain::DISPATCH_CAPPED_REFUSALS,
    plain::DISPATCH_CAPPED_REQUEST_ANSWERED,
    plain::DISPATCH_QUEUE_DROPS,
    plain::DISPATCH_CAP_DROPS,
    plain::DISPATCH_RELEASE_DISCARDS,
    plain::DISPATCH_SATURATION,
    plain::DISPATCH_NEW_CALL_SHARE_WAITS,
    DISPATCH_INVITE_DISCARD_ANSWERED,
    UDP_INGRESS_BRAKE_EMERGENCY_BYPASSED,
    OVERLOAD_TOKEN_BUCKET_LEVEL,
    OVERLOAD_ELU_EWMA,
    OVERLOAD_GC_FRACTION,
    NEW_CALLS,
    NEW_CALL_REFUSED_COPIES,
];

/// The worker's sections, in `/metrics` order.
pub const SECTIONS: &[&[Family]] =
    &[WORKER, TXN, UDP, OVERLOAD, CAPACITY, LOOPS, READINESS, NEW_CALL_OUTCOMES];

#[cfg(test)]
mod tests;
