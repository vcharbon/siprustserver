//! Orphaned client transactions (ADR-0034): a released call's transactions
//! detached from it, and the obligations they then close on their own — the
//! ACK of a 2xx, the BYE of the dialog it created, the re-ACK of its repeats.

use sip_message::generators::{generate_ack_for_2xx_from_invite, generate_bye_for_2xx_from_invite};
use sip_message::SipResponse;
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
    /// any more, so the layer ends it with a BYE (§15.1.1) on an orphaned
    /// transaction of its own. Without the original request (a seeded txn)
    /// there is nothing to build from: the txn is deleted instead.
    pub(super) async fn accept_orphaned_2xx(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
        resp: &SipResponse,
    ) {
        let built = self.clients.get(branch).and_then(|t| {
            let orig = t.original_request.as_ref()?;
            let dest = t.destination?;
            let ack = generate_ack_for_2xx_from_invite(orig, resp, &self.id_gen.new_branch());
            let bye =
                orig.to().tag().is_none().then(|| {
                    generate_bye_for_2xx_from_invite(orig, resp, &self.id_gen.new_branch())
                });
            Some((ack.image().clone(), bye, dest))
        });
        let Some((ack, bye, dest)) = built else {
            self.delete_txn(TxnRef::Client(branch));
            return;
        };
        self.send_buffer(endpoint, &ack, dest).await;
        let keys = match self.clients.get_mut(branch) {
            Some(txn) => {
                txn.state = TxnState::Accepted;
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
        if let Some(bye) = bye {
            let bye_branch = bye.top_via().branch().unwrap_or_default().to_string();
            Box::pin(self.do_send_request(endpoint, bye, dest, TxnKind::NonInvite)).await;
            if let Some(t) = self.clients.get_mut(bye_branch.as_str()) {
                t.orphaned = true;
                self.detach_from_call(TxnRef::Client(&bye_branch));
                self.metrics
                    .orphaned_transactions
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Re-pass the retained ACK of an Accepted txn to a repeat of its 2xx
    /// (RFC 3261 §13.2.2.4: the same ACK, every time).
    pub(super) async fn re_ack_accepted(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let Some((ack, dest)) = self
            .clients
            .get(branch)
            .and_then(|t| Some((t.retransmit_buf.clone()?, t.destination?)))
        else {
            return;
        };
        self.send_buffer(endpoint, &ack, dest).await;
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
