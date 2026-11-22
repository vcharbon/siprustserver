//! Request path, RFC 3261 §16: preflight (Max-Forwards 483, a request from this
//! proxy's own address 482, Proxy-Require 420), the non-2xx ACK hop decision
//! (relay on the INVITE's hop, or absorb the ACK to a self-generated final),
//! top-Route strip and direction under the ingress face's trust (ADR-0048),
//! self-gate (ELU/CPS) admission, target selection (CANCEL memo, loose-route
//! next hop, worker-outbound R-URI, cookie decode, select), received/rport
//! stamping, Record-Route insertion, Via push, retransmission memos, serialize
//! and forward, and a 513 for a forward the socket refused as too long.
//!
//! The routing ladder lives in [`route`], Route preprocessing in [`classify`],
//! the gate in [`admission`], the target's steps in [`target`], the
//! double-Record-Route insertion in [`record_route`], the cookie on a worker's
//! Via in [`via_cookie`], self-generated UAS finals in [`reply`], the answer to a
//! supervision OPTIONS in [`supervision`]. The response relay lives in
//! `core/response`.

mod admission;
mod classify;
mod first_ingress;
mod parse_refusal;
mod record_route;
mod reply;
mod route;
mod supervision;
mod target;
mod trace_seam;
mod via_cookie;

#[cfg(test)]
mod ack_hop_tests;
#[cfg(test)]
mod cancel_route_tests;
#[cfg(test)]
mod cookie_identity_tests;
#[cfg(test)]
mod copy_schedule_tests;
#[cfg(test)]
mod ipv6_advertise_tests;
#[cfg(test)]
mod memo_bound_tests;
#[cfg(test)]
mod named_ack_tests;
#[cfg(test)]
mod own_final_ack_tests;
#[cfg(test)]
mod own_final_seam_tests;
#[cfg(test)]
mod own_source_tests;
#[cfg(test)]
mod reject_metric_tests;
#[cfg(test)]
mod retransmission_tests;
#[cfg(test)]
mod rfc_small_fix_tests;
#[cfg(test)]
mod stateless_branch_tests;
#[cfg(test)]
mod too_large_tests;
#[cfg(test)]
mod trace_request_tests;
#[cfg(test)]
mod worker_outbound_tests;

use std::net::SocketAddr;

use sip_message::header::has_magic_cookie;
use sip_message::{Method, SipMessage, SipRequest};

use crate::addr::ProxyAddr;
use crate::observability::metrics::{Direction, Face, MessageResult, RoutingDecisionKind};
use crate::trace::emit;

use super::ProxyCore;

/// How a request reached this proxy: its datagram's source and the face it
/// arrived on. An own final to it leaves on that face.
#[derive(Debug, Clone, Copy)]
pub(in crate::core) struct Arrival {
    pub(in crate::core) src: SocketAddr,
    pub(in crate::core) face: Face,
}

/// Outcome of routing a request — what to meter after the work is done.
pub(super) struct RouteOutcome {
    pub(super) decision: RoutingDecisionKind,
    /// The forwarded-to target; `None` for a request not forwarded.
    pub(super) target: Option<ProxyAddr>,
}

/// The RFC 3261 branch of an inbound request's top Via (§8.1.1.7, the
/// `z9hG4bK` magic cookie first): the immediate upstream's client-transaction
/// id, which a retransmission, its CANCEL and its non-2xx ACK repeat. The wire
/// parser refuses a top Via without one, so `None` is a request no receive
/// path parsed.
fn received_branch(req: &SipRequest) -> Option<&str> {
    req.top_via().branch().filter(|b| has_magic_cookie(b))
}

impl ProxyCore {
    /// The request is recorded on its call's trace (ADR-0026) before it is routed:
    /// a datagram of a traced call after the one that opens its span is recorded
    /// here and only here, a repeat that reaches activation again included.
    pub(super) async fn handle_request(&self, msg: SipMessage, src: SocketAddr, face: Face) {
        let start_ms = self.now_ms();
        let SipMessage::Request(req) = &msg else { return };
        if let Some(refusal) = self.first_ingress_refusal(req, src, face) {
            self.refuse_unparsed(&refusal, src, face).await;
            return;
        }
        self.metrics.record_message(Direction::Inbound, MessageResult::Forwarded);
        self.metrics.record_request(req.method().as_str());

        let at_ms = i64::try_from(start_ms).unwrap_or(i64::MAX);
        let call_id = req.call_id().as_str();
        emit::sip_in(&self.traces, call_id, at_ms, src, req.image());
        let initial_invite = matches!(req.method(), Method::Invite) && req.to().tag().is_none();

        let outcome = self.route_request(&msg, src, face).await;

        // ADR-0026 §4
        if initial_invite && outcome.decision == RoutingDecisionKind::Reject {
            self.traces.close(call_id);
        } else if req.method() == Method::Ack && outcome.target.is_some() {
            self.traces.close_on_ack(call_id);
        }

        let elapsed_ms = self.now_ms().saturating_sub(start_ms);
        self.metrics.observe_routing_duration(std::time::Duration::from_millis(elapsed_ms));
        self.metrics.record_routing_decision(outcome.decision);
    }

    /// A test's `handle_request` on a single-face proxy, where every request
    /// arrives on the internal face. A dual-face test names the face itself.
    #[cfg(test)]
    pub(in crate::core) async fn handle_request_from(&self, msg: SipMessage, src: SocketAddr) {
        assert!(self.external.is_none(), "a dual-face test names the request's face");
        self.handle_request(msg, src, Face::Internal).await
    }

    /// A test's `route_request`, as [`Self::handle_request_from`].
    #[cfg(test)]
    pub(in crate::core) async fn route_request_from(
        &self,
        msg: &SipMessage,
        src: SocketAddr,
    ) -> RouteOutcome {
        assert!(self.external.is_none(), "a dual-face test names the request's face");
        self.route_request(msg, src, Face::Internal).await
    }
}
