//! The core counter set's families beyond its plain counters
//! ([`super::plain`]): call lifecycle, the per-method SIP traffic, the drain,
//! the store and call-census gauges, the per-peer failures.

use call::model::ObligationKind;
use metric_catalogue::{assert_exposition_order, label_values, Dim, Family, Labels};
use sip_message::method::Method;
use sip_message::status::STACK_CODES;

use crate::drain::DrainExit;
use crate::effects::QuietTurn;
use crate::metrics::{past_bound_label, RemovalClass, PAST_BOUNDS};
use crate::peer_failures::{PeerFailureKind, PeerScope};

/// A request's method: every method modelled natively; an extension method
/// gets its own series, under the family's cap.
pub const METHOD: Dim = Dim::new("method", &Method::NATIVE_TOKENS);

const ANSWERED_METHOD_VALUES: [&str; 13] = {
    let mut out = [""; 13];
    let mut slots: &mut [&str] = &mut out;
    let mut tokens: &[&str] = &Method::NATIVE_TOKENS;
    while let Some((slot, more)) = slots.split_first_mut() {
        while let Some((token, rest)) = tokens.split_first() {
            tokens = rest;
            if !matches!(token.as_bytes(), b"ACK") {
                *slot = token;
                break;
            }
        }
        slots = more;
    }
    out
};
/// A method a response answers: every native method but ACK, which none
/// does (RFC 3261 §17.1.1.1).
pub const ANSWERED_METHOD: Dim = Dim::new("method", &ANSWERED_METHOD_VALUES);

/// A response's status: every code this stack sends or matches on; another
/// code gets its own series (the parser bounds codes to 100..=699).
pub const CODE: Dim = Dim::new("code", &STACK_CODES);

const INVITE: Dim = Dim::new("method", &["INVITE"]);

/// A peer: no value declared; each one observed gets its own series.
pub const PEER: Dim = Dim::new("peer", &[]);

/// What an unroutable message that drew no answer was: an ACK, or a
/// response by status class.
pub const UNROUTABLE_KIND: Dim =
    Dim::new("kind", &["ACK", "1xx", "2xx", "3xx", "4xx", "5xx", "6xx"]);

const OBLIGATION_VALUES: [&str; 2] = label_values!(ObligationKind::ALL, ObligationKind::label);
/// A dialog-level ladder's obligation, indexed like [`ObligationKind::ALL`].
pub const OBLIGATION: Dim = Dim::new("obligation", &OBLIGATION_VALUES);

const QUIET_TURN_VALUES: [&str; 3] = label_values!(QuietTurn::ALL, QuietTurn::kind);
/// A quiet turn's kind, indexed like [`QuietTurn::ALL`].
pub const QUIET_TURN: Dim = Dim::new("kind", &QUIET_TURN_VALUES);

const DRAIN_REASON_VALUES: [&str; 4] = label_values!(DrainExit::ALL, DrainExit::label);
/// Why a drain returned, indexed like [`DrainExit::ALL`].
pub const DRAIN_REASON: Dim = Dim::new("reason", &DRAIN_REASON_VALUES);

const REMOVAL_CLASS_VALUES: [&str; 4] = label_values!(RemovalClass::ALL, RemovalClass::label);
/// The release that tore a call's queue down, indexed like [`RemovalClass::ALL`].
pub const REMOVAL_CLASS: Dim = Dim::new("class", &REMOVAL_CLASS_VALUES);

const PAST_BOUND_VALUES: [&str; 2] = label_values!(PAST_BOUNDS, past_bound_label);
/// The dispatcher bound an item was queued past, indexed like `PAST_BOUNDS`.
pub const PAST_BOUND: Dim = Dim::new("bound", &PAST_BOUND_VALUES);

const PEER_SCOPE_VALUES: [&str; 2] = label_values!(PeerScope::ALL, PeerScope::label);
/// A peer's scope, indexed like [`PeerScope::ALL`].
pub const PEER_SCOPE: Dim = Dim::new("scope", &PEER_SCOPE_VALUES);

const PEER_FAILURE_KIND_VALUES: [&str; 4] =
    label_values!(PeerFailureKind::ALL, PeerFailureKind::label);
/// A per-peer failure's kind, indexed like [`PeerFailureKind::ALL`].
pub const PEER_FAILURE_KIND: Dim = Dim::new("kind", &PEER_FAILURE_KIND_VALUES);

assert_exposition_order!(ObligationKind: AckOf2xx, AwaitPrack);
assert_exposition_order!(QuietTurn: OwnRung, ReAck, AbsorbedAck);
assert_exposition_order!(DrainExit: Quiescent, CaughtUp, Grace, GracePeersBehind);
assert_exposition_order!(RemovalClass: Terminated, SelfRelease, StandDown, Orphan);
assert_exposition_order!(PeerScope: Internal, External);
assert_exposition_order!(
    PeerFailureKind: ResponseTimeout,
    TransactionTimeout,
    KeepaliveTimeout,
    SendFailure,
);

/// The upper bounds (seconds) of the `b2bua_drain_seconds` buckets, ascending.
pub const DRAIN_BUCKETS: [f64; 8] = [0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 10.0];

pub const CALL_CREATIONS: Family = Family::counter(
    "b2bua_call_creations_total",
    Labels::None,
    "B2BUA calls this worker began serving (one per call_ref / dialog; NOT transactions or SIP messages). Matched 1:1 with removals.",
);

pub const CALL_REMOVALS: Family = Family::counter(
    "b2bua_call_removals_total",
    Labels::None,
    "B2BUA calls this worker stopped serving (one per call_ref teardown). Matched 1:1 with creations.",
);

pub const REQUESTS: Family = Family::counter(
    "b2bua_requests_total",
    Labels::Product(&[METHOD]),
    "inbound SIP requests by method",
)
.capped(&REQUESTS_OVERFLOW, &["method"]);

pub const RESPONSES: Family = Family::counter(
    "b2bua_responses_total",
    Labels::Product(&[ANSWERED_METHOD, CODE]),
    "inbound SIP responses by CSeq method + status code",
)
.capped(&RESPONSES_OVERFLOW, &["method"]);

pub const REQUESTS_OUT: Family = Family::counter(
    "b2bua_requests_out_total",
    Labels::Product(&[METHOD]),
    "outbound SIP requests this worker ORIGINATED/relayed by method (e.g. the in-dialog keepalive OPTIONS); pair with b2bua_responses_total{method=\"OPTIONS\",code=\"200\"} to see the keepalive round-trip",
).capped(&REQUESTS_OUT_OVERFLOW, &["method"]);

pub const RETRANSMITS: Family = Family::counter(
    "b2bua_retransmits_total",
    Labels::Union(&[
        &[Dim::new("ladder", &["trigger"]), Dim::new("method", &["ACK"])],
        &[Dim::new("ladder", &["final-2xx"]), INVITE, Dim::new("code", &["200"])],
        &[Dim::new("ladder", &["reliable-provisional"]), INVITE, Dim::new("code", &["180", "183"])],
    ]),
    "repeats of a retained emission that left this worker, by what paced them (ladder: the sip_retransmit class of a dialog-level ladder, or trigger for a re-send the peer provoked), the CSeq method, and the status for a response (no code label on a request); a climb names a deaf peer or a lossy path",
).semi_open();

pub const OUTBOUND_UNADDRESSABLE: Family = Family::counter(
    "b2bua_outbound_unaddressable_total",
    Labels::Product(&[Dim::new("kind", &["request", "response", "datagram"])]),
    "outbound SIP messages left unsent because their next hop is not an IP literal (the worker resolves no name), by kind; a request that opens a client transaction fails its call's attempt as a transport failure",
);

pub const UNROUTABLE_DROPPED: Family = Family::counter(
    "b2bua_unroutable_dropped_total",
    Labels::Product(&[UNROUTABLE_KIND]),
    "wire messages that resolved to no call and drew no answer, by kind (ACK, or a response's status class); nothing a peer waits on",
);

pub const UNROUTABLE_REFUSED: Family = Family::counter(
    "b2bua_unroutable_refused_total",
    Labels::Product(&[ANSWERED_METHOD, Dim::new("code", &["481"])]),
    "wire requests that resolved to no call, answered on no call's behalf (481 RFC 3261 §12.2.2/§15.1.2/§9.2, 405 §8.2.1) by method and code; a stray CANCEL's 481 is stateless, so each repeat of it counts again",
).capped(&UNROUTABLE_REFUSED_OVERFLOW, &["method"]);

pub const UNROUTABLE_INTERNAL: Family = Family::counter(
    "b2bua_unroutable_internal_total",
    Labels::Union(&[
        &[Dim::new("event", &["timeout"]), ANSWERED_METHOD],
        &[
            Dim::new("event", &["timeout", "timer", "cancelled", "internal-event", "call-quiesced"]),
            Dim::new("method", &[""]),
        ],
    ]),
    "this node's own events that resolved to no call, by event and method (timeout: a client transaction released from its call reached Timer B/F); no wire message and no peer waiting",
).capped(&UNROUTABLE_INTERNAL_OVERFLOW, &["method"]);

pub const REPEAT_GIVE_UPS: Family = Family::counter(
    "b2bua_repeat_give_ups_total",
    Labels::Product(&[OBLIGATION]),
    "dialog-level ladders that ran to their give-up with the obligation still undischarged (ack-of-2xx: RFC 3261 §13.3.1.4; a reliable provisional's ladder arms none, its responder rejects); the rate a peer goes deaf at",
);

/// A model invariant a landing record broke, or a defect a turn reported
/// (`rules::model_invariants::NAMES`).
pub const MODEL_INVARIANT: Dim = Dim::new("invariant", &crate::rules::model_invariants::NAMES);

pub const MODEL_DEFECTS: Family = Family::counter(
    "b2bua_model_defects_total",
    Labels::Product(&[MODEL_INVARIANT]),
    "model invariants a turn's landing record broke and defects a turn met (ADR-0052 §8): each one a defect of this node's own; the turn landed all the same",
);

pub const REPL_QUIET_TURNS: Family = Family::counter(
    "b2bua_repl_quiet_turns_total",
    Labels::Product(&[QUIET_TURN]),
    "dialog-level retransmission turns persisted with no version bump and no flush (own-rung: a rung of this node's own 2xx or reliable-provisional ladder; re-ack: the re-ACK of a repeated inbound 2xx); paired with b2bua_retransmits_total, N rungs against 0 flushes is the rule holding",
);

pub const REPL_APPLIED: Family = Family::counter(
    "b2bua_repl_applied_total",
    Labels::Product(&[
        Dim::new("flow", &["recovery", "backup"]),
        PEER,
        Dim::new("op", &["create", "update", "delete"]),
    ]),
    "inbound replication ops applied per stream+endpoint+op (flow=recovery|backup, peer=endpoint, op=create|update|delete); a reboot's bulk reclaim shows as a recovery/create step",
).semi_open();

pub const REPL_NOOPS_SENT: Family = Family::counter(
    "b2bua_repl_noops_sent_total",
    Labels::Product(&[Dim::new("flow", &["reclaim", "backup"]), PEER]),
    "catch-up/idle Noops sent per serve-side stream (flow=reclaim|backup, peer=caller); climbs continuously on a healthy stream — the backup-holder's 'sent everything in this flow' liveness sign (ADR-0014)",
).semi_open();

pub const REPL_FORWARD_FLUSH_REFUSED: Family = Family::counter(
    "b2bua_repl_forward_flush_refused_total",
    Labels::Product(&[Dim::new("op", &["put", "delete"])]),
    "forward flushes (primary→backup) a backup refused (ADR-0031 D3): op=put, a body behind the Element on a lifecycle axis or behind its b; op=delete, a teardown of an answered Active Element by an authority that never published the answer. A rising count means a primary is flushing a branch of a call one of its backups took over — expected across a partition heal or a drain, sustained means the two views never converge",
);

pub const DRAIN_EXITS: Family = Family::counter(
    "b2bua_drain_exits_total",
    Labels::Product(&[DRAIN_REASON]),
    "drains by why they returned (reason=quiescent|caught_up|grace|grace_peers_behind, ADR-0031 D2); grace_peers_behind means a departing worker abandoned live calls no peer reported holding — a lost flush window, never a clean drain",
);

pub const DRAIN_SECONDS: Family = Family::histogram(
    "b2bua_drain_seconds",
    Labels::None,
    "time a graceful drain spent waiting before it returned (ADR-0031 D2)",
);

pub const CALL_REMOVALS_BY_CLASS: Family = Family::counter(
    "b2bua_call_removals_by_class_total",
    Labels::Product(&[REMOVAL_CLASS]),
    "b2bua_call_removals_total by the release that tore the queue down (terminated: a call that owes its CDR; self_release: an acting backup's takeover copy shed at quiescence; stand_down: a takeover copy stood down on its primary's write; orphan: a queue that never held a call, e.g. a stateless admission shed or an event for a gone call)",
);

pub const DISPATCH_PAST_BOUND: Family = Family::counter(
    "b2bua_dispatch_past_bound_total",
    Labels::Product(&[PAST_BOUND]),
    "items queued past a dispatcher bound instead of dropped (depth: the call's queue still full once its waiting items moved in; cap: the global queue cap): a call's timer fire or a client transaction's outcome most often, else a call's release, a reaper verdict, the answer to a request the call sent or a Cancelled",
);

pub const DISPATCH_OVERFLOW_REFUSED: Family = Family::counter(
    "b2bua_dispatch_overflow_refused_total",
    Labels::None,
    "past-bounds items turned away at a call's overflow ceiling (as many as its queue depth); the call is torn down through the reaper",
);

pub const DISPATCH_OVERFLOW_DEPTH: Family = Family::gauge(
    "b2bua_dispatch_overflow_depth",
    Labels::None,
    "items waiting past a full per-call queue, all calls",
);

pub const CALLS_NEAR_LIFETIME_CAP: Family = Family::gauge(
    "b2bua_calls_near_lifetime_cap",
    Labels::None,
    "live calls offered more than 80% of max_messages_per_call_lifetime",
);

pub const ACTIVE_CALLS: Family = Family::gauge(
    "b2bua_active_calls",
    Labels::None,
    "live calls this worker is serving (creations - removals; now a true gauge since the two are paired)",
);

pub const TIMER_QUEUE_LEN: Family = Family::gauge(
    "b2bua_timer_queue_len",
    Labels::None,
    "physical timer DelayQueue entries, incl. not-yet-expired tombstones from cancelled/rescheduled timers",
);

pub const TIMER_LIVE: Family = Family::gauge(
    "b2bua_timer_live",
    Labels::None,
    "live (schedulable) timers; b2bua_timer_queue_len minus this is the lingering-tombstone backlog",
);

pub const CLOCK_WALL_DIVERGENCE_MS: Family = Family::gauge(
    "b2bua_clock_wall_divergence_ms",
    Labels::None,
    "signed gap (raw SystemTime − monotonic-anchored Clock::now_ms); a large sudden magnitude is a host NTP step that skews cross-node replicated timer deadlines",
);

pub const STORE_CALLS: Family = Family::gauge(
    "b2bua_store_calls",
    Labels::None,
    "live entries in the call map (true gauge; compare to b2bua_active_calls)",
);

pub const STORE_SIP_INDEX: Family = Family::gauge(
    "b2bua_store_sip_index",
    Labels::None,
    "SIP routing index keys (callId/tag -> callRef)",
);

pub const STORE_INDEXED: Family =
    Family::gauge("b2bua_store_indexed", Labels::None, "per-call owned-index-key sets");

pub const STORE_LOCKS: Family = Family::gauge(
    "b2bua_store_locks",
    Labels::None,
    "per-callRef serialization locks held (should track store_calls; a gap is a lock leak)",
);

pub const STORE_TAKEOVER_AT: Family = Family::gauge(
    "b2bua_store_takeover_at",
    Labels::None,
    "live acting-backup takeover copies (ADR-0014; self-released on the served transaction's terminal state)",
);

pub const STORE_TOUCHED: Family = Family::gauge(
    "b2bua_store_touched",
    Labels::None,
    "last-touched ledger entries (reaper liveness stamps, ADR-0020; mirrors store_calls — a gap is a stamp leak)",
);

pub const STORE_BODIES: Family = Family::gauge(
    "b2bua_store_bodies",
    Labels::None,
    "inner CallStore body entries (pri:+bak: across partitions)",
);

pub const STORE_IDX_ENTRIES: Family = Family::gauge(
    "b2bua_store_idx_entries",
    Labels::None,
    "inner CallStore idx:* routing entries; outgrowing store_bodies = stranded-index leak (put_call is insert-only)",
);

pub const STORE_TOMBSTONES: Family = Family::gauge(
    "b2bua_store_tombstones",
    Labels::None,
    "resurrection-guard tombstones; outgrowing 300s×delete_rate = prune gap",
);

pub const REPL_META: Family = Family::gauge(
    "b2bua_repl_meta_total",
    Labels::None,
    "replica metadata entries held (all partitions)",
);

pub const REPL_META_BACKUP: Family = Family::gauge(
    "b2bua_repl_meta_backup",
    Labels::None,
    "replica metadata entries in BACKUP partitions (resident backup bodies this node holds for peers; ADR-0014)",
);

pub const REPL_CHANGELOG_ENTRIES: Family = Family::gauge(
    "b2bua_repl_changelog_entries",
    Labels::None,
    "outbound changelog entries across all peer logs (replication buffer depth)",
);

pub const REPL_CHANGELOG_PEERS: Family = Family::gauge(
    "b2bua_repl_changelog_peers",
    Labels::None,
    "peer logs currently held in the changelog",
);

pub const WITHDRAWN_RUNNING: Family = Family::gauge(
    "b2bua_withdrawn_running",
    Labels::None,
    "1 while this worker has observed its own endpoint withdrawn from routing and still runs (ADR-0031 D6)",
);

pub const CORDONED: Family = Family::gauge(
    "b2bua_cordoned",
    Labels::None,
    "1 while an operator cordon is set on this worker (POST /admin/cordon, lifted by DELETE): it refuses new calls with its own 503 and serves every dialog; a worker that drains keeps the cordon it was given (ADR-0050 §9)",
);

/// What an operator's release-all did to a call, indexed like
/// [`crate::metrics::OPERATOR_RELEASE_KINDS`].
pub const RELEASE_KIND: Dim = Dim::new("kind", &crate::metrics::OPERATOR_RELEASE_KINDS);

pub const OPERATOR_RELEASED: Family = Family::counter(
    "b2bua_operator_released_total",
    Labels::Product(&[RELEASE_KIND]),
    "calls an operator's release-all reached (POST /admin/release-all, ADR-0050 §10), counted as requested: released = answered, ended with no BYE; aborted = in setup, 503 to the caller and CANCEL to a ringing callee",
);

/// One of the worker's two loops, as [`crate::loops::LOOP_NAMES`] names them.
pub const LOOP: Dim = Dim::new("loop", &crate::loops::LOOP_NAMES);

pub const LOOP_STALLED: Family = Family::gauge(
    "b2bua_loop_stalled",
    Labels::Product(&[LOOP]),
    "1 while the loop (txn_owner: the transaction layer's recv/timer/sweep loop; router: the event router) has been on one piece of work past the stall threshold (B2BUA_LOOP_STALL_MS, above the call-control timeout); an idle loop is never stalled. Readiness is 503 and /healthz 503 while any reads 1",
);

pub const REPL_PEERS_PULLED_NOT_READY: Family = Family::gauge(
    "b2bua_repl_peers_pulled_not_ready",
    Labels::None,
    "replication peers pulled while their endpoint is not ready (present in membership only; ADR-0031 D1)",
);

pub const REPL_BOOTSTRAP_LAST_APPLIED: Family = Family::gauge(
    "b2bua_repl_bootstrap_last_applied",
    Labels::None,
    "bodies the most recent bootstrap pass imported (re-stalling at the same value across passes ⇒ the stream is truncating, not the materialisation)",
);

pub const REPL_RECLAIM_SCANNED: Family = Family::gauge(
    "b2bua_repl_reclaim_scanned",
    Labels::None,
    "bodies the most recent bulk reclaim pass found in pri:{self} (denominator: everything bootstrap import made reclaimable; ≪ peer repl_meta_backup ⇒ a bootstrap-import/forward-replication gap)",
);

pub const REPL_RECLAIM_MATERIALIZED: Family = Family::gauge(
    "b2bua_repl_reclaim_materialized",
    Labels::None,
    "bodies the most recent bulk reclaim pass freshly re-served into the live map (cumulative total is repl_reclaimed_total; ≪ scanned cumulatively ⇒ a materialise gap)",
);

pub const CENSUS_CDR_EVENTS: Family = Family::gauge(
    "b2bua_census_cdr_events",
    Labels::None,
    "sum of cdr_events Vec len across live calls (drained only at terminal; climbing ratio vs store_calls = per-call CDR leak)",
);

pub const CENSUS_PENDING_REQUESTS: Family = Family::gauge(
    "b2bua_census_pending_requests",
    Labels::None,
    "sum of inbound_pending_requests across all dialogs of live calls, retired early dialogs' included (removed only on a correlated final response; a climbing ratio = uncorrelated/lost-response leak)",
);

pub const CENSUS_PENDING_REQUESTS_MAX: Family = Family::gauge(
    "b2bua_census_pending_requests_max",
    Labels::None,
    "max inbound_pending_requests on any single live call (the worst held dialog)",
);

pub const CENSUS_DIALOGS: Family = Family::gauge(
    "b2bua_census_dialogs",
    Labels::None,
    "sum of dialogs Vec len across all legs of live calls (forking early-dialogs should collapse to 1 after confirm; a climb = un-pruned fork)",
);

pub const CENSUS_ROUTE_SET: Family = Family::gauge(
    "b2bua_census_route_set",
    Labels::None,
    "sum of dialog route_set entries across live calls",
);

pub const CENSUS_TIMERS: Family = Family::gauge(
    "b2bua_census_timers",
    Labels::None,
    "sum of serializable timer-intent Vec len across live calls (deduped by id; should be flat per call)",
);

pub const CENSUS_TAG_MAP: Family = Family::gauge(
    "b2bua_census_tag_map",
    Labels::None,
    "sum of callee dialogs fronted by caller-facing dialogs across live calls",
);

pub const CENSUS_B_LEGS: Family =
    Family::gauge("b2bua_census_b_legs", Labels::None, "sum of b_legs Vec len across live calls");

pub const SM_CURSORS: Family = Family::gauge(
    "b2bua_sm_cursors",
    Labels::Product(&[Dim::new("machine", &[]), Dim::new("state", &[])]),
    "live calls resting at each state-machine cursor (machine=global-call|transfer|announcement|…, state=label); the live distribution of every call's machine positions (ADR-0016)",
).semi_open();

pub const PEER_FAILURES: Family = Family::counter(
    "b2bua_peer_failures_total",
    Labels::Product(&[PEER, PEER_SCOPE, PEER_FAILURE_KIND]),
    "Per-peer SIP failures/timeouts by kind, split internal/external. Internal peers always keep their series; external peers past the cap land on peer=\"_overflow\" (b2bua_peer_failures_overflow_total).",
).capped(&PEER_FAILURES_OVERFLOW, &["peer"]);

pub const REQUESTS_OVERFLOW: Family = Family::counter(
    "b2bua_requests_overflow_total",
    Labels::None,
    "observations of b2bua_requests_total past its cap, each counted on its series whose method reads _overflow",
);

pub const RESPONSES_OVERFLOW: Family = Family::counter(
    "b2bua_responses_overflow_total",
    Labels::None,
    "observations of b2bua_responses_total past its cap, each counted on its series whose method reads _overflow",
);

pub const REQUESTS_OUT_OVERFLOW: Family = Family::counter(
    "b2bua_requests_out_overflow_total",
    Labels::None,
    "observations of b2bua_requests_out_total past its cap, each counted on its series whose method reads _overflow",
);

pub const UNROUTABLE_REFUSED_OVERFLOW: Family = Family::counter(
    "b2bua_unroutable_refused_overflow_total",
    Labels::None,
    "observations of b2bua_unroutable_refused_total past its cap, each counted on its series whose method reads _overflow",
);

pub const UNROUTABLE_INTERNAL_OVERFLOW: Family = Family::counter(
    "b2bua_unroutable_internal_overflow_total",
    Labels::None,
    "observations of b2bua_unroutable_internal_total past its cap, each counted on its series whose method reads _overflow",
);

pub const PEER_FAILURES_OVERFLOW: Family = Family::counter(
    "b2bua_peer_failures_overflow_total",
    Labels::None,
    "observations of b2bua_peer_failures_total past its cap, each counted on its series whose peer reads _overflow",
);
