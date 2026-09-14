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
mod trace_seam;

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
mod stateless_branch_tests;
#[cfg(test)]
mod trace_request_tests;
#[cfg(test)]
mod worker_outbound_tests;

use std::net::SocketAddr;

use sip_message::{Method, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::observability::metrics::{Direction, MessageResult, RoutingDecisionKind};
use crate::trace::emit;

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
/// keys its retransmission TARGET memo on, and the §17.1.1.3 discriminator the
/// non-2xx ACK hop decision compares.
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

        // Per-call trace tier (ADR-0026): the Call-ID is the one the routing
        // ladder already parsed — the hot path never re-scans the datagram —
        // and the whole check is one predicted branch while nothing is sampled.
        // A brand-new call is not in the map yet, so this is a miss and the
        // activation that opens its span records the INVITE. Every LATER
        // datagram of a traced call — including one that reaches activation
        // again (a digest retry, a retransmit whose memo has been evicted) — is
        // recorded HERE and only here: that activation reports `AlreadyOpen`
        // and records nothing.
        let at_ms = start_ms as i64;
        let call_id = req.call_id().as_str();
        emit::sip_in(&self.traces, call_id, at_ms, src, req.image());
        let initial_invite = matches!(req.method(), Method::Invite) && req.to().tag().is_none();

        let outcome = self.route_request(&msg, src).await;

        // Close on the last fact this hop sees about the call, rather than
        // holding its span, its active-cap slot and the process-wide sampled
        // flag for the full idle TTL:
        //  • an initial INVITE the ladder refused — nothing will ever name that
        //    Call-ID again;
        //  • the ACK of a setup this hop saw rejected for good (486, 603, and
        //    the CANCEL flow's 487) — that call is over once its ACK is on the
        //    wire. Which ACK that is was decided on the response path, where
        //    the final's status is known: an `AckHop` is written for EVERY
        //    relayed non-2xx INVITE final, including a mid-dialog re-INVITE's
        //    488 and an auth challenge, neither of which ends the call.
        if initial_invite && outcome.decision == RoutingDecisionKind::Reject {
            self.traces.close(call_id);
        } else if outcome.decision == RoutingDecisionKind::AckHop {
            self.traces.close_on_ack(call_id);
        }

        let duration = (self.now_ms().saturating_sub(start_ms)) as f64 / 1000.0;
        self.metrics.observe_routing_duration(duration);
        self.metrics.record_routing_decision(outcome.decision);
    }
}
