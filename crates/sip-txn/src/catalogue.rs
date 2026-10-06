//! The transaction layer's metric families, under the `b2bua_txn_` names the
//! worker serves them by: queue and timer gauges, backlog and sweep counts,
//! the transaction-ladder rungs, the datagrams it could not parse or send.

use metric_catalogue::{label_values, Dim, Family, Labels};
use sip_message::method::Method;
use sip_retransmit::Class;

use crate::event::EventQueueClass;

const EVENT_QUEUE_CLASS_VALUES: [&str; 6] =
    label_values!(EventQueueClass::ALL, EventQueueClass::label);
/// The class of an event the inbound channel dropped or deferred, indexed
/// like [`EventQueueClass::ALL`].
pub const EVENT_QUEUE_CLASS: Dim = Dim::new("reason", &EVENT_QUEUE_CLASS_VALUES);

const REQUEST_LADDER_VALUES: [&str; 4] = [
    Class::InviteClient.as_str(),
    Class::NonInviteClient.as_str(),
    Class::NonInviteProceeding.as_str(),
    Class::CancelClient.as_str(),
];

/// A transaction-ladder rung's labels: a request ladder's rows, one per
/// ladder and native method, declared; a final's or a replay's rows (with
/// `code`), and an extension method's, appear at their first rung.
const RETRANSMIT_LABELS: Labels = Labels::Union(&[
    &[Dim::new("ladder", &REQUEST_LADDER_VALUES), Dim::new("method", &Method::NATIVE_TOKENS)],
    &[Dim::new("ladder", &[]), Dim::new("method", &[]), Dim::new("code", &[])],
]);

pub const RETRANSMITS_OVERFLOW: Family = Family::counter(
    "b2bua_txn_retransmits_overflow_total",
    Labels::None,
    "rungs of b2bua_txn_retransmits_total past its cap of extension-method rows, each counted on its series whose method is _overflow",
);

pub const ACTIVE_TRANSACTIONS: Family = Family::gauge(
    "b2bua_txn_active_transactions",
    Labels::None,
    "In-flight client+server transactions.",
);

pub const TIMER_QUEUE_LEN: Family = Family::gauge(
    "b2bua_txn_timer_queue_len",
    Labels::None,
    "Live entries in the txn-layer DelayQueue (retransmit/timeout/cleanup); a climb vs flat active_transactions is a timer/slab leak.",
);

pub const RETRANSMIT_BUF_BYTES: Family = Family::gauge(
    "b2bua_txn_retransmit_buf_bytes",
    Labels::None,
    "Sum of per-txn retransmit-buffer bytes retained for retransmission.",
);

pub const SERVER_FINAL_UNSEEN_BRANCH: Family = Family::counter(
    "b2bua_txn_server_final_unseen_branch_total",
    Labels::None,
    "Non-2xx INVITE finals dropped because no server transaction held their branch (RFC 3261 section 17.2.1, one final per transaction); expected 0.",
);

pub const PARSE_ERRORS: Family = Family::counter(
    "b2bua_txn_parse_errors_total",
    Labels::None,
    "Inbound datagrams the SIP parser rejected and the transaction layer dropped: a malformed-traffic flood or a parser regression.",
);

pub const SEND_ERRORS: Family = Family::counter(
    "b2bua_txn_send_errors_total",
    Labels::None,
    "Outbound datagrams the socket refused for a reason other than a full send buffer (that one is b2bua_udp_send_would_block_total): ENOBUFS, a filter's EPERM, an unreachable peer.",
);

pub const EVENT_QUEUE_DEPTH: Family = Family::gauge(
    "b2bua_txn_event_queue_depth",
    Labels::None,
    "Inbound->app events channel current depth.",
);

pub const EVENT_QUEUE_CAPACITY: Family = Family::gauge(
    "b2bua_txn_event_queue_capacity",
    Labels::None,
    "Inbound->app events channel capacity.",
);

pub const EVENT_QUEUE_DROPS: Family = Family::counter(
    "b2bua_txn_event_queue_drops_total",
    Labels::Product(&[EVENT_QUEUE_CLASS]),
    "Ordinary events the full inbound->app channel dropped, by class, counted per wire copy (a dropped non-INVITE request is readmitted on each retransmission).",
);

pub const EVENT_QUEUE_DEFERRALS: Family = Family::counter(
    "b2bua_txn_event_queue_deferred_total",
    Labels::Product(&[EVENT_QUEUE_CLASS]),
    "Critical events the full inbound->app channel deferred for later delivery, by class; a deferral is no loss.",
);

pub const EVENT_QUEUE_DEFERRED: Family = Family::gauge(
    "b2bua_txn_event_queue_deferred",
    Labels::None,
    "Critical events waiting for room in the inbound->app channel.",
);

pub const DEFERRED_SWEPT: Family = Family::counter(
    "b2bua_txn_deferred_swept_total",
    Labels::None,
    "Deferred requests removed with the server transaction that left unanswered (at its backstop, or reaped by the sweep) before the router took them.",
);

pub const SWEEP_REAPED: Family = Family::counter(
    "b2bua_txn_sweep_reaped_total",
    Labels::None,
    "Transactions the safety-net sweep found still resident more than one sweep interval (10 s) past their lifetime deadline, and removed: each had no cleanup timer left. Expected 0.",
);

pub const UNANSWERED_FORGOTTEN: Family = Family::counter(
    "b2bua_txn_unanswered_forgotten_total",
    Labels::None,
    "Non-INVITE server transactions forgotten because the router discarded their request unrun (queue full, call cap, behind a release), so the retransmission is admitted again.",
);

pub const FORGET_REFUSED: Family = Family::counter(
    "b2bua_txn_forget_refused_total",
    Labels::None,
    "Forget requests the full txn command queue refused; the transaction absorbs its retransmissions until its backstop. Expected 0.",
);

pub const RELEASED_UNANSWERED_INVITES_ANSWERED: Family = Family::counter(
    "b2bua_txn_released_unanswered_invites_answered_total",
    Labels::None,
    "In-dialog INVITEs left without a final at their ended call's release, answered 481 there: a handler that died after the 100 Trying, or a re-INVITE queued behind the call's last turn (whose discard answer then finds it answered).",
);

pub const RELEASED_UNANSWERED_FORGOTTEN: Family = Family::counter(
    "b2bua_txn_released_unanswered_forgotten_total",
    Labels::None,
    "Non-INVITE server transactions with no final forgotten at their call's release (the handler died or its answer never came), so the retransmission reaches the orphan path.",
);

pub const RETRANSMITS: Family = Family::counter(
    "b2bua_txn_retransmits_total",
    RETRANSMIT_LABELS,
    "transaction-ladder rungs the txn layer put on the wire (Timer A/E, the CANCEL sub-ladder, Timer G), by what paced them (ladder), the request's method, and the final's status on a Timer G row; the dialog-level ladders are b2bua_retransmits_total.",
)
.capped(&RETRANSMITS_OVERFLOW, &["method"]);
