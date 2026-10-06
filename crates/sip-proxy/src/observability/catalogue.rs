//! The proxy's catalogued metric families: its data path, its routing, its
//! receive queues, its overload shedding, its workers' health, its
//! per-peer failures, and the proxy-self gate. [`PROXY`] and [`SELF_GATE`]
//! list them in `/metrics` order.

use metric_catalogue::{assert_exposition_order, label_values, Dim, Family, Labels};
use sip_message::method::Method;
use sip_message::status::STACK_CODES;

use super::metrics::{
    CancelLookup, Direction, Face, HmacFailureReason, MessageResult, RoutingDecisionKind,
};
use super::peer_failures::{PeerFailureKind, PeerScope};
use crate::resolver::{outcome, refresh_outcome};
use crate::self_gate::ShedReason;
use crate::strategy::Promotion;

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

const ANSWERED_METHOD_VALUES: [&str; 13] = {
    let mut out = [""; 13];
    let mut i = 0;
    let mut j = 0;
    while i < Method::NATIVE_TOKENS.len() {
        if !matches!(Method::NATIVE_TOKENS[i].as_bytes(), b"ACK") {
            out[j] = Method::NATIVE_TOKENS[i];
            j += 1;
        }
        i += 1;
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

const CANCEL_LOOKUP_VALUES: [&str; 2] = label_values!(CancelLookup::ALL, CancelLookup::as_str);

const PROMOTION_VALUES: [&str; 2] = label_values!(Promotion::ALL, Promotion::as_str);

/// Every locally decided reject: the routing path's own, the load
/// balancer's selection failures, the self gate's.
const REJECT_REASONS: [&str; 13] = [
    "drop_unforwardable",
    "non_request",
    "ack_max_forwards_exhausted",
    "too_many_hops",
    "ack_proxy_require_unsupported",
    "proxy_require_unsupported",
    "malformed_request_uri",
    "stickiness_decode_rejected",
    "no_target_selected",
    "no_target_available",
    "worker_rate_capped",
    ShedReason::Elu.label(),
    ShedReason::Cps.label(),
];

const SHED_REASON_VALUES: [&str; 2] = label_values!(ShedReason::ALL, ShedReason::label);

/// A worker's health state, in the order the registry counts them.
pub const HEALTH_STATES: [&str; 5] = ["alive", "draining", "not-ready", "unknown", "dead"];

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
assert_exposition_order!(Promotion: FreshPod, NotReady);
assert_exposition_order!(ShedReason: Elu, Cps);
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

pub const DECODE_FORWARD_PROMOTIONS: Family = Family::counter(
    "sip_proxy_decode_forward_promotions_total",
    Labels::Product(&[Dim::new("reason", &PROMOTION_VALUES)]),
    "Requests routed by the stickiness cookie (in-dialog requests and a CANCEL following its INVITE's cookie, retransmissions included) sent to the cookie's backup although the primary is up: alive inside its fresh-pod guard window (fresh_pod) or not ready (not_ready). A dead, unknown, departed or draining-past-grace primary is not counted here (sip_routing_decision_total kind=decode_forward_backup counts every backup forward).",
);

pub const FRESH_POD_FORWARDS: Family = Family::counter(
    "sip_proxy_fresh_pod_forwards_total",
    Labels::None,
    "Requests routed by the stickiness cookie (in-dialog requests and a CANCEL following its INVITE's cookie, retransmissions included) forwarded to a primary inside its fresh-pod guard window because the cookie names no usable backup.",
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
    REQUESTS,
    REQUESTS_OVERFLOW,
    RESPONSES,
    RESPONSES_OVERFLOW,
    CALLS,
    ROUTING_DECISION,
    REJECTS,
    HMAC_FAILURES,
    CANCEL_LOOKUPS,
    DECODE_FORWARD_PROMOTIONS,
    FRESH_POD_FORWARDS,
    ROUTING_DURATION_SECONDS,
    RECORD_ROUTE_INSERTED,
    PENDING_INVITE_LRU_SIZE,
    NAMED_SENDS,
    RESOLVER_REFRESH,
    RESOLVER_CACHE_SIZE,
    SEND_FAILURES,
    UDP_QUEUE_DEPTH,
    UDP_QUEUE_MAX,
    UDP_ENQUEUED,
    UDP_TAIL_DROPPED,
    INTAKE_SHED,
    UDP_SEND_WOULD_BLOCK,
    UDP_KERNEL_RX_DROPPED,
    RECV_SHARDS_STALLED,
    WORKER_POOL_EMPTY,
    OVERLOAD_REJECTIONS,
    WORKER_STALE_DECREASE,
    LB_EMERGENCY_BYPASSED,
    WORKER_HEALTH,
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
