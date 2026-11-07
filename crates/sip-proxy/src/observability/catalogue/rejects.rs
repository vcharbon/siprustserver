//! The requests the proxy refuses or drops by its own decision, by reason.

use metric_catalogue::{Dim, Family, Labels};

use crate::self_gate::ShedReason;

/// Every locally decided reject: the routing path's own, the load
/// balancer's selection failures, the self gate's.
const REJECT_REASONS: [&str; 18] = [
    "drop_unforwardable",
    "no_target_dropped",
    "non_request",
    "ack_not_answered",
    "ack_max_forwards_exhausted",
    "too_many_hops",
    "loop_detected",
    "message_too_large",
    "ack_proxy_require_unsupported",
    "proxy_require_unsupported",
    "malformed_request_uri",
    "unsupported_uri_scheme",
    "stickiness_decode_rejected",
    "no_target_selected",
    "no_target_available",
    "worker_rate_capped",
    ShedReason::IntakeAge.label(),
    ShedReason::Cps.label(),
];

pub const REJECTS: Family = Family::counter(
    "sip_proxy_rejects_total",
    Labels::Product(&[Dim::new("reason", &REJECT_REASONS)]),
    "Locally decided rejects by reason (the per-cause split of sip_routing_decision_total kind=reject).",
);
