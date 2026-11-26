//! The response path. A response is relayed when its top Via names either face
//! of this proxy (the egress face's advertise stamps each outbound Via) and it
//! carries at least two Vias (§16.7.3). It goes by the owner rule (ADR-0055 §8):
//! to the owner holding its transaction, along the next Via (`received`/`rport`
//! over sent-by, §18.2.2) or at the address that owner holds now, else to the
//! call's other owner, else nowhere. The proxy pops its own Via entry
//! (comma-aware, §7.3.1) and forwards. On an INVITE final it shortens the
//! INVITE's memo entry to Timer H and, for a non-2xx one, remembers the ACK's
//! hop: the node the final arrived from (`final_memo`; the ACK itself is routed
//! by `core/request`).
//!
//! A 100 is relayed like any other response: §16.7's absorption of a 100 is a
//! stateful proxy's, and this proxy is stateless (§16.11), so it neither
//! absorbs the downstream 100 nor generates one of its own. A response is
//! dropped only when its top Via is not this proxy, when it carries fewer than
//! two Vias, when it arrives on the external face and its next Via names no
//! registry worker and its top Via no cookie (ADR-0048), when no owner of its
//! call can take it, or when its top Via cannot be popped and the rest
//! re-rendered.
//!
//! An address that left the worker set counts as `Dead` for Timer H past the
//! last response for it (`registry::tombstone`, re-armed by every response whose
//! next Via sent-by names it), so a response for it goes to the call's other
//! owner.

use sip_message::header::Via;
use sip_message::types::SipResponse;
use sip_message::{Method, SipMessage};

use crate::addr::ProxyAddr;
use crate::headers::via_marks_same_face;
use crate::observability::metrics::{Direction, Face, MessageResult, UntrustedClaim};
use crate::trace::emit;

use super::ProxyCore;
use owner::OwnerRoute;

/// Whether a response this hop relayed rejects the call's SETUP for good — the
/// class after which the ACK is the only fact left. The two auth challenges are
/// not in it: they are answered by retrying the same call with credentials, so
/// the call outlives the transaction they end.
fn rejects_the_setup(status: u16, method: &Method) -> bool {
    *method == Method::Invite && (300..700).contains(&status) && !matches!(status, 401 | 407)
}

impl ProxyCore {
    /// `src` is the datagram's source: the node that sent this final, which
    /// is the hop a §17.1.1.3 ACK for it must reach. `face` is the face it
    /// arrived on.
    pub(super) async fn handle_response(
        &self,
        resp: SipResponse,
        src: std::net::SocketAddr,
        face: Face,
    ) {
        let cseq = resp.cseq().clone();
        self.metrics.record_message(Direction::Inbound, MessageResult::Forwarded);
        self.metrics.record_response(cseq.method().as_str(), resp.status());
        let at_ms = self.trace_ms();
        let is_traced = emit::response_in(
            &self.traces,
            resp.call_id().as_str(),
            at_ms,
            resp.status(),
            cseq.method().as_str(),
            resp.image(),
        );
        let traced: Option<String> = is_traced.then(|| resp.call_id().as_str().to_string());
        let ends_the_call = cseq.method() == Method::Bye && resp.status() >= 200;

        let hops: Vec<Via> = resp.via().iter().cloned().collect();
        let [ours, next, ..] = hops.as_slice() else {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        };
        let (top_host, top_port) = ours.host_port();
        if !self.is_self_addr(top_host, top_port) {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        }

        let (sent_by_host, sent_by_port) = next.sent_by().pair();
        let sent_by = ProxyAddr::new(sent_by_host, sent_by_port);
        let marked = via_marks_same_face(ours);
        let for_a_worker = self.egress.response_may_be_a_workers(face, marked);
        if for_a_worker {
            self.registry.rearm_departed(&sent_by);
        }

        // ADR-0055 §8
        let msg = SipMessage::Response(resp);
        let route = self.owner_route(&msg, ours, &sent_by, for_a_worker);
        let SipMessage::Response(resp) = msg else { return };
        let (host, port, owner_named) = match (route, face) {
            (OwnerRoute::To(target), _) => (target.host, target.port, true),
            (OwnerRoute::Nowhere, _) => {
                self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
                return;
            }
            (OwnerRoute::Via { owner_named }, Face::Internal) => {
                let (host, port) = next.response_target();
                (host.to_string(), port, owner_named)
            }
            (OwnerRoute::Via { .. }, Face::External) => match self.external_response_target(next) {
                Some(target) => (target.host, target.port, true),
                None => {
                    self.metrics.record_external_untrusted(UntrustedClaim::ResponseNotToWorker);
                    self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
                    return;
                }
            },
        };

        let Ok(popped) = resp.thaw().pop_top::<Via>() else {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        };
        let Ok(bytes) = popped.freeze_bytes() else {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        };
        let next_hop = ProxyAddr::new(host, port);
        let egress = self.egress.response(face, owner_named, marked);
        let _ = self.send_to(&bytes, &next_hop, egress, None).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Forwarded);
        if let Some(call_id) = &traced {
            emit::relayed(&self.traces, call_id, at_ms, &next_hop, &bytes);
            if ends_the_call {
                self.traces.close(call_id);
            }
            if rejects_the_setup(resp.status(), cseq.method()) {
                self.traces.arm_close_on_ack(call_id, resp.from().tag(), cseq.seq());
            }
        }

        self.remember_final(&resp, next, ours, src);
    }

    /// A test's `handle_response` on a single-face proxy, where every response
    /// arrives on the internal face. A dual-face test names the face itself.
    #[cfg(test)]
    pub(super) async fn handle_response_from(&self, resp: SipResponse, src: std::net::SocketAddr) {
        assert!(self.external.is_none(), "a dual-face test names the response's face");
        self.handle_response(resp, src, Face::Internal).await
    }

    /// Where a response arriving on the external face goes (ADR-0048): its next
    /// Via's response address when that is a registry worker, a departed one
    /// included; else that Via's sent-by when the sent-by is one, a worker
    /// whose source was translated on its way to the proxy. `None`: the Via
    /// names no worker, so the response is not relayed. The proxy reflects
    /// nothing to a host a sender names.
    fn external_response_target(&self, next: &Via) -> Option<ProxyAddr> {
        let (host, port) = next.response_target();
        let target = ProxyAddr::new(host, port);
        if self.registry.lookup_by_address(&target).is_some() {
            return Some(target);
        }
        let (host, port) = next.sent_by().pair();
        let sent_by = ProxyAddr::new(host, port);
        self.registry.lookup_by_address(&sent_by).is_some().then_some(sent_by)
    }
}

mod final_memo;
mod owner;

#[cfg(test)]
mod reverse_failover_tests;

#[cfg(test)]
mod relay_tests;

#[cfg(test)]
mod via_cookie_tests;
