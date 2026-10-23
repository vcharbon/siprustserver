//! Response path, single-endpoint: validate ≥2 Via and that the top is us;
//! route by the owner rule (`owner`, ADR-0055 §8), every method alike: to the
//! owner holding the response's transaction, along the next Via
//! (received/rport precedence) or at the address it holds now, else to the
//! call's other owner, else nowhere; pop the top Via entry (comma-aware);
//! forward; remember the ACK-relay hop for a non-2xx INVITE final — the node
//! the final arrived from (the ACK itself travels end-to-end — see
//! `core/request`).
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
//! An address that LEFT the worker set counts as `Dead` here for Timer H past
//! the last response for it (`registry::tombstone`, re-armed by every response
//! whose next Via names it), so a response for it goes to the call's other
//! owner.

use sip_message::header::Via;
use sip_message::types::SipResponse;
use sip_message::{Method, SipMessage};

use crate::addr::ProxyAddr;
use crate::cancel_lru::{ack_hop_key, invite_txn_key};
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
        // Per-call trace tier (ADR-0026): the parsed Call-ID, one predicted
        // branch while nothing is sampled.
        let at_ms = self.trace_ms();
        let is_traced = emit::response_in(
            &self.traces,
            resp.call_id().as_str(),
            at_ms,
            resp.status(),
            cseq.method().as_str(),
            resp.image(),
        );
        // The relay below consumes the message, so a TRACED call's key is
        // carried across it. An untraced call allocates nothing — the emission
        // above already answered whether this call has a span, so the clone
        // rides that answer and not the process-wide sampled flag.
        let traced: Option<String> = is_traced.then(|| resp.call_id().as_str().to_string());
        let ends_the_call = cseq.method() == Method::Bye && resp.status() >= 200;

        // §16.7.3: need ≥2 Via (ours + the next hop's).
        let hops: Vec<Via> = resp.via().iter().cloned().collect();
        let [ours, next, ..] = hops.as_slice() else {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        };
        let (top_host, top_port) = ours.host_port();
        // Dual-face: the proxy stamps its outbound Via with the EGRESS face's
        // advertise, so a response legitimately names either face here.
        if !self.is_self_addr(top_host, top_port) {
            // Top Via is not us — not our response to relay.
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        }

        // The worker a response is for is named by its Via sent-by — its
        // advertised registry address, SNAT-immune, never the received/rport
        // send target, which behind address translation is a node IP and an
        // ephemeral port; a response for a departed address, whatever its
        // method, re-arms that address's window.
        let (sent_by_host, sent_by_port) = next.sent_by().pair();
        let sent_by = ProxyAddr::new(sent_by_host, sent_by_port);
        self.registry.rearm_departed(&sent_by);

        // ── The owner rule (ADR-0055 §8), every method alike ────────────────
        let msg = SipMessage::Response(resp);
        let route = self.owner_route(&msg, ours, &sent_by).await;
        let SipMessage::Response(resp) = msg else { return };
        let (host, port) = match (route, face) {
            (OwnerRoute::To(target), _) => (target.host, target.port),
            (OwnerRoute::Nowhere, _) => {
                self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
                return;
            }
            // received / rport take precedence over sent-by (§18.2.2 / §16.7.3).
            (OwnerRoute::Via, Face::Internal) => {
                let (host, port) = next.response_target();
                (host.to_string(), port)
            }
            (OwnerRoute::Via, Face::External) => match self.external_response_target(next) {
                Some(target) => (target.host, target.port),
                None => {
                    self.metrics.record_external_untrusted(UntrustedClaim::ResponseNotToWorker);
                    self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
                    return;
                }
            },
        };

        // Pop our own Via entry (§7.3.1 comma-aware) and forward. The thawed
        // draft reads nothing below that top line, and freezing renders the
        // relayed datagram once.
        let Ok(popped) = resp.thaw().pop_top::<Via>() else {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        };
        let Ok(bytes) = popped.freeze_bytes() else {
            self.metrics.record_message(Direction::Outbound, MessageResult::Dropped);
            return;
        };
        let next_hop = ProxyAddr::new(host, port);
        self.send_to(&bytes, &next_hop).await;
        self.metrics.record_message(Direction::Outbound, MessageResult::Forwarded);
        if let Some(call_id) = &traced {
            emit::relayed(&self.traces, call_id, at_ms, &next_hop, &bytes);
            // The BYE's final is the last fact this hop will see about the
            // call: close the span here rather than leaving it to the TTL.
            if ends_the_call {
                self.traces.close(call_id);
            }
            // A non-2xx INVITE final ends the CALL only when it rejects its
            // setup for good. An auth challenge does not — the caller retries
            // the same Call-ID with credentials — and neither does a
            // mid-dialog re-INVITE's 488/491, which the registry tells apart
            // by the transaction it is holding. One more fact follows a
            // rejection on this hop, the ACK, so the span closes there.
            if rejects_the_setup(resp.status(), cseq.method()) {
                self.traces.arm_close_on_ack(call_id, resp.from().tag(), cseq.seq());
            }
        }

        // ── Relayed non-2xx INVITE final: remember the ACK-relay hop ────────
        // This transaction-less proxy (ADR-0022 X4) synthesizes no §17.1.1.3
        // hop ACK: the UAS retransmits the final through this relay until the
        // upstream's own ACK arrives, which the request path relays on the
        // memo written here.
        //
        // The memo carries the hop to repeat: the node the final ARRIVED from
        // — after a failover that is the survivor, not the INVITE's target.
        // The branch the sender's server transaction is keyed on is §16.11's
        // function of the ACK itself (`crate::branch`). Gating the relay on
        // the memo (not the INVITE entry alone) keeps a takeover worker's 2xx
        // ACK safe when its reset `IdGen` re-mints a branch aliasing the dead
        // primary's INVITE (see `core/request`). Short TTL: the upstream ACKs
        // within its final-retransmit window (a re-sent final refreshes it).
        if (300..700).contains(&resp.status()) && cseq.method() == Method::Invite {
            // The response echoes the request's From (tag included), and the
            // Via below ours is the upstream's as the INVITE carried it, so
            // this re-builds exactly the key the INVITE was remembered under.
            let call_id = resp.call_id();
            let from = resp.from();
            let key = invite_txn_key(next, call_id.as_str(), from.tag(), cseq.seq());
            if let Some(found) = self.cancel_lru.lookup(&key) {
                self.cancel_lru.remember(
                    &ack_hop_key(next, call_id.as_str(), from.tag(), cseq.seq()),
                    crate::cancel_lru::CancelEntry {
                        target: ProxyAddr::from(src),
                        branch: found.branch.clone(),
                        stickiness: None,
                        minted: None,
                        via_cookie: None,
                    },
                    crate::cancel_lru::RTX_ENTRY_TTL_MS,
                );
            }
        }
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

mod owner;

#[cfg(test)]
mod reverse_failover_tests;

#[cfg(test)]
mod relay_tests;

#[cfg(test)]
mod via_cookie_tests;
