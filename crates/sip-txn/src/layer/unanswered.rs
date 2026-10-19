//! Server transactions the consumer will not answer: a non-INVITE request it
//! discarded or whose call it released, forgotten so the UAC's retransmission
//! is admitted afresh, and the in-dialog INVITEs of a released call, answered
//! through their transactions.

use std::net::SocketAddr;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::SipRequest;
use sip_net::UdpEndpoint;

use call::TxnKind;

use super::key::ServerTxnKey;
use super::owner::Owner;
use super::txn::{TxnRef, TxnState};

impl Owner {
    /// Forget the non-INVITE server transaction `key` names while it is still
    /// `Trying` and holds the discarded request's `call_id` and `from_tag`: the
    /// consumer discarded that request unanswered, and only an unseen
    /// transaction lets the UAC's retransmission in again. Counted.
    pub(super) fn forget_unanswered(&mut self, key: &ServerTxnKey, call_id: &str, from_tag: &str) {
        let unanswered = self.server(key.id()).is_some_and(|t| {
            t.kind == TxnKind::NonInvite
                && t.state == TxnState::Trying
                && t.call_id == call_id
                && t.from_tag == from_tag
        });
        if unanswered && self.delete_txn(TxnRef::Server(key.id())) {
            self.metrics.unanswered_forgotten.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Forget every non-INVITE server transaction of `call_ref` that has sent
    /// no final (`Trying`, or `Proceeding` on a provisional): the consumer
    /// released the call, so no answer will come. Counted; returns how many.
    pub(super) fn forget_unanswered_of_call(&mut self, call_ref: &str) -> usize {
        let unanswered: Vec<ServerTxnKey> = self
            .txn_index
            .get(call_ref)
            .into_iter()
            .flat_map(|txns| txns.server.iter())
            .filter(|k| {
                self.server(k.id()).is_some_and(|t| {
                    t.kind == TxnKind::NonInvite
                        && matches!(t.state, TxnState::Trying | TxnState::Proceeding)
                })
            })
            .cloned()
            .collect();
        let forgotten =
            unanswered.iter().filter(|k| self.delete_txn(TxnRef::Server(k.id()))).count();
        self.metrics
            .released_unanswered_forgotten
            .fetch_add(forgotten as u64, std::sync::atomic::Ordering::Relaxed);
        forgotten
    }

    /// Answer every in-dialog INVITE server transaction of `call_ref` that has
    /// sent no final with `status` `reason`, through the transaction: under
    /// the To-tag it names, to the address the request came from (or, for a
    /// re-INVITE seeded at a takeover, which recorded none, the top Via's
    /// response target). Counted; returns how many.
    pub(super) async fn answer_unanswered_invites_of_call(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        call_ref: &str,
        status: u16,
        reason: &str,
    ) -> usize {
        let unanswered: Vec<(ServerTxnKey, Box<SipRequest>, Option<SocketAddr>)> = self
            .txn_index
            .get(call_ref)
            .into_iter()
            .flat_map(|txns| txns.server.iter())
            .filter_map(|k| {
                let t = self.server(k.id())?;
                let open = t.kind == TxnKind::Invite
                    && matches!(t.state, TxnState::Trying | TxnState::Proceeding);
                // An out-of-dialog INVITE naming the call in its Request-URI
                // (a Replaces INVITE, RFC 3891) starts another call.
                let req = t.original_request.clone().filter(|r| open && r.to().tag().is_some())?;
                Some((k.clone(), req, t.destination))
            })
            .collect();
        let mut answered = 0;
        for (key, req, source) in unanswered {
            let Some(dest) = source.or_else(|| via_response_target(&req)) else { continue };
            let to_tag = self.uas_to_tag_of(key.id());
            let opts = GenerateResponseOpts { to_tag, ..Default::default() };
            let Ok(resp) = generate_response(&req, status, reason, &opts) else { continue };
            self.do_send_response(endpoint, resp, dest).await;
            answered += 1;
        }
        self.metrics
            .released_unanswered_invites_answered
            .fetch_add(answered as u64, std::sync::atomic::Ordering::Relaxed);
        answered
    }
}

/// Where a response to `req` goes when its source is unknown (a re-INVITE
/// seeded at a takeover): the top Via's response target (RFC 3261 §18.2.2),
/// when it names an IP address.
fn via_response_target(req: &SipRequest) -> Option<SocketAddr> {
    let (host, port) = req.top_via().response_target();
    let ip: std::net::IpAddr = host.trim_start_matches('[').trim_end_matches(']').parse().ok()?;
    Some(SocketAddr::new(ip, port))
}
