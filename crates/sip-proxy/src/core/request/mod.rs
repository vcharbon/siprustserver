//! Request path, RFC 3261 §16, single-endpoint: preflight (Max-Forwards 483,
//! Proxy-Require 420), the non-2xx ACK hop decision (relay on the INVITE's
//! hop, or absorb the ACK to a self-generated final), top-Route strip +
//! worker-outbound classification, self-gate (ELU/CPS) admission, target
//! selection (CANCEL LRU → loose-route next hop → worker-outbound R-URI →
//! cookie decode → select), received/rport stamping, Record-Route insertion,
//! Via push, retransmission memos, serialize + forward.
//!
//! The routing ladder lives in [`route`], the double-Record-Route insertion in
//! [`record_route`], self-generated UAS finals in [`reply`]. The response
//! relay does NOT live here — see `core/response`.

mod record_route;
mod reply;
mod route;

#[cfg(test)]
mod ack_hop_tests;
#[cfg(test)]
mod cookie_identity_tests;
#[cfg(test)]
mod reject_metric_tests;
#[cfg(test)]
mod retransmission_tests;
#[cfg(test)]
mod rfc_small_fix_tests;
#[cfg(test)]
mod worker_outbound_tests;

use std::net::SocketAddr;

use sip_message::{SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::observability::metrics::{Direction, MessageResult, RoutingDecisionKind};

use super::ProxyCore;

/// Outcome of routing a request — what to meter after the work is done.
pub(super) struct RouteOutcome {
    pub(super) decision: RoutingDecisionKind,
    /// The forwarded-to target. The lib meters only the decision; the routing
    /// regression tests assert on this field.
    #[allow(dead_code)]
    pub(super) target: Option<ProxyAddr>,
}

/// The `branch=` token of the TOP (first) `Via` header of an inbound request —
/// the immediate upstream's client-transaction id (RFC 3261 §8.1.1.7: globally
/// unique per transaction thanks to the `z9hG4bK` magic cookie). A
/// retransmission reuses this exact token, so it is the correlator the proxy
/// keys retransmission branch-reuse on.
fn top_via_branch(req: &SipRequest) -> Option<String> {
    let top = req.top_via();
    top.branch().filter(|b| !b.is_empty()).map(str::to_owned)
}

impl ProxyCore {
    pub(super) async fn handle_request(&self, msg: SipMessage, src: SocketAddr) {
        let start_ms = self.now_ms();
        let SipMessage::Request(req) = &msg else { return };
        self.metrics.record_message(Direction::Inbound, MessageResult::Forwarded);
        // Method::as_str() is already canonical-uppercase for known methods
        // (Method::from_wire normalized at parse time); unknown tokens match
        // no routing branch and land in the bounded `other` metric slot.
        self.metrics.record_request(req.method().as_str());
        // (`sip_proxy_calls_total` is counted inside `route_request`, where the
        // retransmission memo can exclude re-sent copies of the same INVITE.)

        let outcome = self.route_request(&msg, src).await;

        let duration = (self.now_ms().saturating_sub(start_ms)) as f64 / 1000.0;
        self.metrics.observe_routing_duration(duration);
        self.metrics.record_routing_decision(outcome.decision);
    }
}
