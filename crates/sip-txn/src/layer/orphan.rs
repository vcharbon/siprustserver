//! Orphaned client transactions (ADR-0034): a released call's transactions
//! detached from it, and the obligations they then close on their own — the
//! ACK of a 2xx, the BYE of the dialog it created, the re-ACK of its repeats.

use sip_message::generators::{generate_ack_for_2xx_from_invite, generate_bye_for_2xx_from_invite};
use std::net::SocketAddr;

use sip_message::{SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::event::TxnKind;

use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{CancelWire, TxnRef, TxnState};

impl Owner {
    /// ACK the 2xx an orphaned INVITE client txn drew (RFC 3261 §13.2.2.4 —
    /// the layer stands in for the UAC core the release removed) and hold the
    /// txn in Accepted for Timer M (RFC 6026 §7.2), the ACK retained for the
    /// 2xx's repeats. A 2xx to an initial INVITE created a dialog nothing holds
    /// any more, so the layer ends it ([`Owner::bye_unheld_dialog`]). Without
    /// the original request there is nothing to build from: the txn is deleted
    /// instead.
    pub(super) async fn accept_orphaned_2xx(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        resp: &SipResponse,
    ) {
        let built = self.clients.get(branch).and_then(|t| {
            let orig = t.original_request.as_ref()?;
            let ack = generate_ack_for_2xx_from_invite(orig, resp, &self.id_gen.new_branch());
            Some((ack.image().clone(), t.destination?))
        });
        let Some((ack, dest)) = built else {
            self.delete_txn(TxnRef::Client(branch));
            return;
        };
        self.send_buffer(endpoint, &ack, dest).await;
        let keys = match self.clients.get_mut(branch) {
            Some(txn) => {
                txn.state = TxnState::Accepted;
                txn.accepted_to_tag = resp.to().tag().map(str::to_string);
                if txn.held_cancel.take().is_some_and(|h| h.wire == CancelWire::Held) {
                    self.metrics
                        .held_cancels_dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                txn.retransmit_buf = Some(ack);
                txn.ladder = None;
                (
                    txn.retransmit_key.take(),
                    txn.timeout_key.take(),
                    txn.cancel_retransmit_key.take(),
                )
            }
            None => (None, None, None),
        };
        self.cancel_timer(keys.0);
        self.cancel_timer(keys.1);
        self.cancel_timer(keys.2);
        self.hold(TxnRef::Client(branch), Hold::TimerM);
        self.bye_unheld_dialog(endpoint, branch, resp).await;
    }

    /// A 2xx on an Accepted orphan: a repeat of the 2xx it ACKed re-draws the
    /// retained ACK (RFC 3261 §13.2.2.4: the same ACK, every time); a 2xx under
    /// another To-tag confirms a forked dialog of its own (RFC 6026 §7.2), which
    /// draws its own ACK and BYE.
    pub(super) async fn on_accepted_2xx(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        resp: &SipResponse,
    ) {
        let Some(t) = self.clients.get(branch) else { return };
        let Some(dest) = t.destination else { return };
        let forked = t.accepted_to_tag.as_deref() != resp.to().tag();
        let ack = match (&t.retransmit_buf, &t.original_request) {
            (Some(ack), _) if !forked => ack.clone(),
            (_, Some(orig)) if forked => {
                generate_ack_for_2xx_from_invite(orig, resp, &self.id_gen.new_branch())
                    .image()
                    .clone()
            }
            _ => return,
        };
        self.send_buffer(endpoint, &ack, dest).await;
        if forked {
            self.bye_unheld_dialog(endpoint, branch, resp).await;
        }
    }

    /// End with a BYE (§15.1.1) the dialog `resp`, a 2xx to the orphaned
    /// initial INVITE at `branch`, created: numbered above every request this
    /// layer sent on that INVITE's dialogs, sent to the route set's first hop
    /// where it names an address, else where the INVITE went (§8.1.2), on an
    /// orphaned non-INVITE transaction of its own. A 2xx to a re-INVITE belongs
    /// to a dialog its call ended.
    async fn bye_unheld_dialog(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        resp: &SipResponse,
    ) {
        let built = self.clients.get(branch).and_then(|t| {
            let orig = t.original_request.as_ref().filter(|o| o.to().tag().is_none())?;
            let cseq = orig.cseq().seq().max(t.dialog_cseq) + 1;
            let bye = generate_bye_for_2xx_from_invite(orig, resp, cseq, &self.id_gen.new_branch());
            let dest = first_route_address(&bye).or(t.destination)?;
            Some((bye, dest))
        });
        let Some((bye, dest)) = built else { return };
        let bye_branch = bye.top_via().branch().unwrap_or_default().to_string();
        Box::pin(self.do_send_request(endpoint, bye, dest, TxnKind::NonInvite)).await;
        if let Some(t) = self.clients.get_mut(bye_branch.as_str()) {
            t.orphaned = true;
            self.detach_from_call(TxnRef::Client(&bye_branch));
            self.metrics.orphaned_transactions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Raise `dialog_cseq` on every INVITE client txn of `call_ref` that `req`,
    /// when a request inside a dialog, shares its Call-ID and From-tag with: a
    /// BYE the layer later sends on that dialog numbers above it (§12.2.1.1).
    pub(super) fn note_dialog_cseq(&mut self, call_ref: Option<&str>, req: &SipRequest) {
        if req.to().tag().is_none() {
            return;
        }
        let Some(txns) = call_ref.and_then(|cr| self.txn_index.get(cr)) else { return };
        let (call_id, from_tag) = (req.call_id().as_str(), req.from().tag().unwrap_or_default());
        let seq = req.cseq().seq();
        for branch in &txns.client {
            if let Some(t) = self.clients.get_mut(branch.as_str()).filter(|t| {
                t.kind == TxnKind::Invite && t.call_id == call_id && t.from_tag == from_tag
            }) {
                t.dialog_cseq = t.dialog_cseq.max(seq);
            }
        }
    }

    /// Release every client transaction of `call_ref` from the call (ADR-0034).
    /// Each is orphaned, never deleted: it keeps its timers and closes its own
    /// obligations — the ACK a final still draws (RFC 3261 §17.1.1.3, §13.2.2.4),
    /// the Timer D / Timer M re-ACK hold, Timer E to its final or Timer F — and
    /// is purged by its own timer. Server transactions keep their §17.2 holds.
    pub(super) fn do_cancel_txns_for_call(&mut self, call_ref: &str) {
        // O(k-branches) over just this call's live branches (the lockstep index),
        // not an O(total_txns) scan of the whole map.
        let branches: Vec<String> = match self.txn_index.get(call_ref) {
            Some(txns) => txns.client.iter().cloned().collect(),
            None => return,
        };
        for branch in branches {
            // A held CANCEL stays parked on the orphan: only the branch's
            // first provisional sends it (§9.1).
            match self.clients.get_mut(branch.as_str()) {
                Some(t) => t.orphaned = true,
                None => continue,
            }
            self.detach_from_call(TxnRef::Client(&branch));
            self.metrics
                .txn_orphaned_on_call_evict
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.metrics.orphaned_transactions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The address the first `Route` of `req` names, where its host is one.
fn first_route_address(req: &SipRequest) -> Option<SocketAddr> {
    let routes = req.route_set().ok()?;
    let route = routes.first()?.uri().clone();
    let host = route.host().parse::<std::net::IpAddr>().ok()?;
    Some(SocketAddr::new(host, route.port().unwrap_or(5060)))
}
