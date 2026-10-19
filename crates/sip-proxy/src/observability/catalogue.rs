//! The proxy's catalogued metric families: its data path, its routing, its
//! receive queues, its overload shedding, its workers' health, its
//! per-peer failures, and the proxy-self gate. [`PROXY`] and [`SELF_GATE`]
//! list them in `/metrics` order.

use metric_catalogue::{assert_exposition_order, label_values, Dim, Family, Labels};
use sip_message::method::Method;
use sip_message::status::STACK_CODES;

use super::metrics::{
    CancelLookup, Direction, Face, HmacFailureReason, MessageResult, NoTargetDrop,
    RoutingDecisionKind, UntrustedClaim,
};
use super::peer_failures::{PeerFailureKind, PeerScope};
use crate::resolver::{outcome, refresh_outcome};
use crate::self_gate::ShedReason;

/// `direction:result`, every direction then every result.
const MESSAGE_LABELS: [&str; 6] = [
    "inbound:forwarded",
    "inbound:responded",
    "inbound:dropped",
    "outbound:forwarded",
    "outbound:responded",
    "outbound:dropped",
];

/// `face:direction`, every face then every direction.
const FACE_LABELS: [&str; 4] = ["int:inbound", "int:outbound", "ext:inbound", "ext:outbound"];

/// A request's method: every method modelled natively; an extension method
/// gets its own series, under the family's cap.
pub const METHOD: Dim = Dim::new("method", &Method::NATIVE_TOKENS);

#[expect(clippy::indexing_slicing, reason = "a const: an index past its table fails the build")]
const ANSWERED_METHOD_VALUES: [&str; 13] = {
    let mut out = [""; 13];
    let mut rest: &[&str] = &Method::NATIVE_TOKENS;
    let mut j = 0;
    while let [first, tail @ ..] = rest {
        if !matches!(first.as_bytes(), b"ACK") {
            out[j] = *first;
            j += 1;
        }
        rest = tail;
    }
    out
};
/// A method a response answers: every native method but ACK.
pub const ANSWERED_METHOD: Dim = Dim::new("method", &ANSWERED_METHOD_VALUES);

/// A response's status: every code this stack sends or matches on; another
/// code gets its own series (the parser bounds codes to 100..=699).
pub const CODE: Dim = Dim::new("code", &STACK_CODES);

const ROUTING_KIND_VALUES: [&str; 8] =
    label_values!(RoutingDecisionKind::ALL, RoutingDecisionKind::as_str);
/// A routing decision, indexed like [`RoutingDecisionKind::ALL`].
pub const ROUTING_KIND: Dim = Dim::new("kind", &ROUTING_KIND_VALUES);

const HMAC_REASON_VALUES: [&str; 3] =
    label_values!(HmacFailureReason::ALL, HmacFailureReason::as_str);
/// Why an HMAC verify failed, indexed like [`HmacFailureReason::ALL`].
pub const HMAC_REASON: Dim = Dim::new("reason", &HMAC_REASON_VALUES);

const UNTRUSTED_CLAIM_VALUES: [&str; 5] =
    label_values!(UntrustedClaim::ALL, UntrustedClaim::as_str);

const CANCEL_LOOKUP_VALUES: [&str; 2] = label_values!(CancelLookup::ALL, CancelLookup::as_str);

const NO_TARGET_DROP_VALUES: [&str; 3] = label_values!(NoTargetDrop::ALL, NoTargetDrop::as_str);

/// Every locally decided reject: the routing path's own, the load
/// balancer's selection failures, the self gate's.
const REJECT_REASONS: [&str; 15] = [
    "drop_unforwardable",
    "no_target_dropped",
    "non_request",
    "ack_not_answered",
    "ack_max_forwards_exhausted",
    "too_many_hops",
    "ack_proxy_require_unsupported",
    "proxy_require_unsupported",
    "malformed_request_uri",
    "stickiness_decode_rejected",
    "no_target_selected",
    "no_target_available",
    "worker_rate_capped",
    ShedReason::IntakeAge.label(),
    ShedReason::Cps.label(),
];

const SHED_REASON_VALUES: [&str; 2] = label_values!(ShedReason::ALL, ShedReason::label);

/// A worker's health state, in the order the registry counts them.
pub const HEALTH_STATES: [&str; 3] = ["alive", "unknown", "dead"];

/// An alive worker's acceptance of new calls, in the order the registry
/// counts them.
pub const ACCEPTANCE_STATES: [&str; 2] = ["accept", "refuse"];

/// A peer: no value declared; each one observed gets its own series.
pub const PEER: Dim = Dim::new("peer", &[]);

const PEER_SCOPE_VALUES: [&str; 2] = label_values!(PeerScope::ALL, PeerScope::label);
/// A peer's scope, indexed like [`PeerScope::ALL`].
pub const PEER_SCOPE: Dim = Dim::new("scope", &PEER_SCOPE_VALUES);

const PEER_FAILURE_KIND_VALUES: [&str; PeerFailureKind::ALL.len()] =
    label_values!(PeerFailureKind::ALL, PeerFailureKind::label);
/// A per-peer failure's kind, indexed like [`PeerFailureKind::ALL`].
pub const PEER_FAILURE_KIND: Dim = Dim::new("kind", &PEER_FAILURE_KIND_VALUES);

assert_exposition_order!(Direction: Inbound, Outbound);
assert_exposition_order!(MessageResult: Forwarded, Responded, Dropped);
assert_exposition_order!(Face: Internal, External);
assert_exposition_order!(
    UntrustedClaim: OutboundMarker,
    WorkerVia,
    WorkerSource,
    ForeignRoute,
    ResponseNotToWorker,
);
assert_exposition_order!(
    RoutingDecisionKind: SelectNew,
    DecodeForward,
    DecodeForwardBackup,
    LooseRoute,
    WorkerOutbound,
    Cancel,
    AckHop,
    Reject,
);
assert_exposition_order!(HmacFailureReason: Missing, Decode, Mismatch);
assert_exposition_order!(CancelLookup: Hit, Miss);
assert_exposition_order!(NoTargetDrop: InDialog, Cancel, Initial);
assert_exposition_order!(ShedReason: IntakeAge, Cps);
assert_exposition_order!(PeerScope: Internal, External);
assert_exposition_order!(
    PeerFailureKind: ResponseTimeout,
    TransactionTimeout,
    SendFailure,
    MessageTooLong,
);

pub const MESSAGES: Family = Family::counter(
    "sip_messages_total",
    Labels::Product(&[Dim::new("label", &MESSAGE_LABELS)]),
    "SIP messages by direction+result.",
);

pub const FACE_MESSAGES: Family = Family::counter(
    "sip_proxy_face_messages_total",
    Labels::Product(&[Dim::new("label", &FACE_LABELS)]),
    "Datagrams by proxy face + direction (dual-face mode; single-face records int only).",
);

pub const EXTERNAL_UNTRUSTED: Family = Family::counter(
    "sip_proxy_external_untrusted_total",
    Labels::Product(&[Dim::new("kind", &UNTRUSTED_CLAIM_VALUES)]),
    "Messages on the external face claiming the internal plane, by claim: an outbound marker, a worker Via or a worker source ignored, foreign Route entries removed (the request is routed as an inbound one), a response whose next Via is no worker dropped.",
);

pub const REQUESTS: Family = Family::counter(
    "sip_proxy_requests_total",
    Labels::Product(&[METHOD]),
    "Inbound SIP requests by method.",
)
.capped(&REQUESTS_OVERFLOW, &["method"]);

pub const RESPONSES: Family = Family::counter(
    "sip_proxy_responses_total",
    Labels::Product(&[ANSWERED_METHOD, CODE]),
    "Inbound SIP responses by CSeq method + status code.",
)
.capped(&RESPONSES_OVERFLOW, &["method"]);

pub const CALLS: Family = Family::counter(
    "sip_proxy_calls_total",
    Labels::None,
    "New calls: initial dialog-creating INVITEs (no To-tag).",
);

pub const ROUTING_DECISION: Family = Family::counter(
    "sip_routing_decision_total",
    Labels::Product(&[ROUTING_KIND]),
    "Routing decisions by kind.",
);

pub const REJECTS: Family = Family::counter(
    "sip_proxy_rejects_total",
    Labels::Product(&[Dim::new("reason", &REJECT_REASONS)]),
    "Locally decided rejects by reason (the per-cause split of sip_routing_decision_total kind=reject).",
);

pub const HMAC_FAILURES: Family = Family::counter(
    "sip_proxy_hmac_failures_total",
    Labels::Product(&[HMAC_REASON]),
    "HMAC verify failures by reason.",
);

pub const CANCEL_LOOKUPS: Family = Family::counter(
    "sip_proxy_cancel_lookups_total",
    Labels::Product(&[Dim::new("outcome", &CANCEL_LOOKUP_VALUES)]),
    "CANCEL messages, retransmissions included, by whether their INVITE's hop was remembered (hit: the CANCEL follows it; miss: routed on its own).",
);

pub const NO_TARGET_DROPPED: Family = Family::counter(
    "sip_proxy_no_target_dropped_total",
    Labels::Product(&[Dim::new("kind", &NO_TARGET_DROP_VALUES)]),
    "Requests dropped unanswered because neither the primary nor the backup of the stickiness cookie placing them is alive: an in-dialog request (in_dialog), a CANCEL following its INVITE's cookie (cancel), a retransmitted out-of-dialog request following the cookie its first copy minted (initial); retransmissions included. The sender's retransmission meets the next worker view.",
);

pub const ROUTING_DURATION_SECONDS: Family =
    Family::histogram("sip_routing_duration_seconds", Labels::None, "Routing decision duration.");

pub const RECORD_ROUTE_INSERTED: Family = Family::counter(
    "sip_proxy_record_route_inserted_total",
    Labels::None,
    "Record-Route headers inserted.",
);

pub const PENDING_INVITE_LRU_SIZE: Family =
    Family::gauge("sip_proxy_pending_invite_lru_size", Labels::None, "Pending-INVITE LRU size.");

pub const NAMED_SENDS: Family = Family::counter(
    "sip_proxy_named_sends_total",
    Labels::Product(&[Dim::new("outcome", &outcome::ALL)]),
    "Named-target (DNS) sends by outcome.",
);

pub const RESOLVER_REFRESH: Family = Family::counter(
    "sip_proxy_resolver_refresh_total",
    Labels::Product(&[Dim::new("outcome", &refresh_outcome::ALL)]),
    "Proactive resolver refresh + startup prewarm events by outcome.",
);

pub const RESOLVER_CACHE_SIZE: Family =
    Family::gauge("sip_proxy_resolver_cache_size", Labels::None, "Resolver name-cache size.");

pub const SEND_FAILURES: Family = Family::counter(
    "sip_proxy_send_failures_total",
    Labels::None,
    "Outbound datagrams the endpoint failed to send.",
);

pub const PARSE_PANICS: Family = Family::counter(
    "sip_proxy_parse_panics_total",
    Labels::None,
    "Inbound datagrams the SIP parser panicked on and the recv shard dropped; each one is a parser defect, expected 0.",
);

pub const POISONED_LOCK_RECOVERIES: Family = Family::counter(
    "sip_proxy_poisoned_lock_recoveries_total",
    Labels::None,
    "Proxy locks a panic poisoned, taken as they stood and cleared (process-wide); each one is a defect, expected 0.",
);

pub const UDP_QUEUE_DEPTH: Family = Family::gauge(
    "sip_proxy_udp_queue_depth",
    Labels::None,
    "Inbound UDP queue depth (sampled, summed over recv shards and both faces).",
);

pub const UDP_QUEUE_MAX: Family = Family::gauge(
    "sip_proxy_udp_queue_max",
    Labels::None,
    "Inbound UDP queue capacity (summed over recv shards and both faces).",
);

pub const UDP_ENQUEUED: Family = Family::counter(
    "sip_proxy_udp_enqueued_total",
    Labels::None,
    "Datagrams accepted into the inbound queue(s), summed over recv shards and both faces.",
);

pub const UDP_TAIL_DROPPED: Family = Family::counter(
    "sip_proxy_udp_tail_dropped_total",
    Labels::None,
    "Datagrams tail-dropped by the full inbound queue(s), summed over recv shards and both faces.",
);

pub const INTAKE_SHED: Family = Family::counter(
    "sip_proxy_intake_shed_total",
    Labels::None,
    "New non-emergency INVITEs dropped by the depth-watermark pre-ingress shed (the selective last-line guard below the admission layer).",
);

pub const UDP_SEND_WOULD_BLOCK: Family = Family::counter(
    "sip_proxy_udp_send_would_block_total",
    Labels::None,
    "Outbound datagrams dropped because the socket's send buffer was full (a blocking send would have parked the recv shard; ADR-0033). Summed over recv shards and both faces.",
);

pub const UDP_KERNEL_RX_DROPPED: Family = Family::counter(
    "sip_proxy_udp_kernel_rx_dropped_total",
    Labels::None,
    "Inbound datagrams the kernel dropped on the signalling sockets before the proxy read them, mostly on a full receive buffer (SO_RCVBUF, PROXY_UDP_RCVBUF). Summed over recv shards and both faces.",
);

pub const UDP_PRE_INGRESS_PANICS: Family = Family::counter(
    "sip_proxy_udp_pre_ingress_panics_total",
    Labels::None,
    "Inbound datagrams whose pre-ingress hook (the intake shed) panicked on the receive pump; each was accepted and is a hook defect, expected 0. Summed over recv shards and both faces.",
);

pub const RECV_SHARDS_STALLED: Family = Family::gauge(
    "sip_proxy_recv_shards_stalled",
    Labels::None,
    "Recv shards that dequeued a packet more than PROXY_SHARD_STALL_MS ago and have not returned to waiting: parked, not idle. Non-zero flips /readyz.",
);

pub const WORKER_POOL_EMPTY: Family = Family::gauge(
    "sip_proxy_worker_pool_empty",
    Labels::None,
    "1 iff no worker is Alive (routable) — the proxy can serve no new dialog.",
);

pub const DISCOVERY_NO_CLIENT: Family = Family::gauge(
    "sip_proxy_discovery_no_client",
    Labels::None,
    "1 while worker discovery's kube client has failed to build and none is built, so nothing is discovered; 0 before the first attempt ends, once one is built, and with a fixed PROXY_WORKERS pool.",
);

pub const DISCOVERY_WATCH_BROKEN_SINCE: Family = Family::gauge(
    "sip_proxy_discovery_watch_broken_since_seconds",
    Labels::None,
    "Unix time the worker EndpointSlice watch broke, 0 while it delivers (an event, or 90 s without an error) and with a fixed PROXY_WORKERS pool. The last listed pool stays meanwhile and the OPTIONS probe keeps its health current.",
);

pub const OVERLOAD_REJECTIONS: Family = Family::counter(
    "sip_proxy_overload_rejections_total",
    Labels::Product(&[Dim::new("reason", &["no_target_critical_filtered", "bucket_empty"])]),
    "New-dialog admissions rejected by the AIMD/band overload path, by reason.",
);

pub const WORKER_STALE_DECREASE: Family = Family::counter(
    "sip_proxy_worker_stale_decrease_total",
    Labels::None,
    "WorkerLoadObserver sweep_stale floor events (AIMD cap halved — no fresh X-Overload within payload_stale_ms).",
);

pub const LB_EMERGENCY_BYPASSED: Family = Family::counter(
    "sip_proxy_lb_emergency_bypassed_total",
    Labels::None,
    "Emergency new-dialog INVITEs that bypassed the LB overload gates (above_critical critical-filter + per-worker AIMD bucket). Emergency traffic skipping the load-balancer's overload path under flood.",
);

pub const WORKER_HEALTH: Family = Family::gauge(
    "sip_worker_health",
    Labels::Product(&[Dim::new("health", &HEALTH_STATES)]),
    "Worker count by health state.",
);

pub const WORKER_ACCEPTANCE: Family = Family::gauge(
    "sip_worker_acceptance",
    Labels::Product(&[Dim::new("acceptance", &ACCEPTANCE_STATES)]),
    "Alive worker count by acceptance of new calls (the X-Readiness their OPTIONS answer states): accept, or refuse while they serve their dialogs.",
);

pub const PEER_FAILURES: Family = Family::counter(
    "sip_proxy_peer_failures_total",
    Labels::Product(&[PEER, PEER_SCOPE, PEER_FAILURE_KIND]),
    "Per-peer SIP failures/timeouts by kind, split internal/external. Internal peers always keep their series; external peers past the cap land on peer=\"_overflow\" (sip_proxy_peer_failures_overflow_total).",
).capped(&PEER_FAILURES_OVERFLOW, &["peer"]);

pub const REQUESTS_OVERFLOW: Family = Family::counter(
    "sip_proxy_requests_overflow_total",
    Labels::None,
    "observations of sip_proxy_requests_total past its cap, each counted on its series whose method reads _overflow",
);

pub const RESPONSES_OVERFLOW: Family = Family::counter(
    "sip_proxy_responses_overflow_total",
    Labels::None,
    "observations of sip_proxy_responses_total past its cap, each counted on its series whose method reads _overflow",
);

pub const PEER_FAILURES_OVERFLOW: Family = Family::counter(
    "sip_proxy_peer_failures_overflow_total",
    Labels::None,
    "observations of sip_proxy_peer_failures_total past its cap, each counted on its series whose peer reads _overflow",
);

// ── The proxy-self gate ──

pub const SELF_GATE_ELU_EWMA: Family = Family::gauge(
    "sip_proxy_self_elu_ewma",
    Labels::None,
    "Proxy-self ELU EWMA (0..1). Crosses elu_critical -> 503.",
);

pub const SELF_GATE_GC_FRACTION: Family = Family::gauge(
    "sip_proxy_self_gc_fraction",
    Labels::None,
    "Proxy-self GC fraction (0..1). Informational only.",
);

pub const SELF_GATE_CPS_BUCKET_LEVEL: Family = Family::gauge(
    "sip_proxy_self_cps_bucket_level",
    Labels::None,
    "Proxy-self CPS bucket level (tokens remaining).",
);

pub const SELF_GATE_CPS_BUCKET_MAX: Family = Family::gauge(
    "sip_proxy_self_cps_bucket_max",
    Labels::None,
    "Proxy-self CPS bucket capacity (constant per config).",
);

pub const SELF_GATE_EXTERNAL_INVITES_ADMITTED: Family = Family::counter(
    "sip_proxy_self_external_invites_admitted_total",
    Labels::None,
    "External new-dialog non-emergency INVITEs admitted by the proxy-self gate.",
);

pub const SELF_GATE_EXTERNAL_INVITES_REJECTED: Family = Family::counter(
    "sip_proxy_self_external_invites_rejected_total",
    Labels::Product(&[Dim::new("reason", &SHED_REASON_VALUES)]),
    "External new-dialog non-emergency INVITEs rejected by the proxy-self gate.",
);

pub const SELF_GATE_EMERGENCY_BYPASSED: Family = Family::counter(
    "sip_proxy_self_emergency_bypassed_total",
    Labels::None,
    "Emergency INVITEs that bypassed the proxy-self gate.",
);

pub const SELF_GATE_INTERNAL_BYPASSED: Family = Family::counter(
    "sip_proxy_self_internal_bypassed_total",
    Labels::None,
    "Worker-originated INVITEs that bypassed the proxy-self gate.",
);

/// The proxy's data path, as `ProxyMetrics::prometheus_text` writes it.
pub const PROXY: &[Family] = &[
    MESSAGES,
    FACE_MESSAGES,
    EXTERNAL_UNTRUSTED,
    REQUESTS,
    REQUESTS_OVERFLOW,
    RESPONSES,
    RESPONSES_OVERFLOW,
    CALLS,
    ROUTING_DECISION,
    REJECTS,
    HMAC_FAILURES,
    CANCEL_LOOKUPS,
    NO_TARGET_DROPPED,
    ROUTING_DURATION_SECONDS,
    RECORD_ROUTE_INSERTED,
    PENDING_INVITE_LRU_SIZE,
    NAMED_SENDS,
    RESOLVER_REFRESH,
    RESOLVER_CACHE_SIZE,
    SEND_FAILURES,
    PARSE_PANICS,
    POISONED_LOCK_RECOVERIES,
    UDP_QUEUE_DEPTH,
    UDP_QUEUE_MAX,
    UDP_ENQUEUED,
    UDP_TAIL_DROPPED,
    INTAKE_SHED,
    UDP_SEND_WOULD_BLOCK,
    UDP_KERNEL_RX_DROPPED,
    UDP_PRE_INGRESS_PANICS,
    RECV_SHARDS_STALLED,
    WORKER_POOL_EMPTY,
    DISCOVERY_NO_CLIENT,
    DISCOVERY_WATCH_BROKEN_SINCE,
    OVERLOAD_REJECTIONS,
    WORKER_STALE_DECREASE,
    LB_EMERGENCY_BYPASSED,
    WORKER_HEALTH,
    WORKER_ACCEPTANCE,
    PEER_FAILURES,
    PEER_FAILURES_OVERFLOW,
];

/// The proxy-self gate, as `self_gate_text` writes it.
pub const SELF_GATE: &[Family] = &[
    SELF_GATE_ELU_EWMA,
    SELF_GATE_GC_FRACTION,
    SELF_GATE_CPS_BUCKET_LEVEL,
    SELF_GATE_CPS_BUCKET_MAX,
    SELF_GATE_EXTERNAL_INVITES_ADMITTED,
    SELF_GATE_EXTERNAL_INVITES_REJECTED,
    SELF_GATE_EMERGENCY_BYPASSED,
    SELF_GATE_INTERNAL_BYPASSED,
];
