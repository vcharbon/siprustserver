//! A CANCEL received for an INVITE server transaction (RFC 3261 §9.2): the
//! INVITE it names answered 487 and the CANCEL 200 while that INVITE is
//! active, the CANCEL alone answered 200 once the INVITE took its final, and
//! handed up when no INVITE here matches it. The rest of the server
//! transaction lives in `layer::server`.

use std::net::SocketAddr;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::Via;
use sip_message::{SipMessage, SipRequest};
use sip_net::UdpEndpoint;

use crate::event::{TransactionEvent, TxnKind};

use super::key::{ServerTxnId, ServerTxnKey};
use super::owner::Owner;
use super::txn::{Transaction, TxnState};

impl Owner {
    /// RFC 3261 §9.2 at this layer: a CANCEL matching an active INVITE server
    /// transaction is answered 200 and its INVITE 487, and `Cancelled` is
    /// emitted; `true`. One matching nothing here is the TU's to answer — a
    /// call it holds for a peer may hold the INVITE the CANCEL names, and only
    /// the TU can rebuild that transaction and re-offer the CANCEL — so it is
    /// handed up unanswered as a `Message` (`false`); re-offered and still
    /// unmatched, it is handed nowhere.
    pub(super) async fn handle_cancel(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: SipRequest,
        src: SocketAddr,
        reoffer: bool,
    ) -> bool {
        let call_id = req.call_id();
        let from = req.from();
        let from_tag = from.tag().unwrap_or_default();

        // The CANCEL shares its INVITE's top-Via branch and sent-by (RFC 3261
        // §9.1), the INVITE transaction's key (§9.2, §17.2.3): one lookup, and
        // a miss is no match. Call-ID and From-tag are confirmed on the hit.
        let cancel_via = req.top_via();
        let is_cancel_target = |t: &Transaction| {
            t.kind == TxnKind::Invite
                && t.call_id == call_id.as_str()
                && t.from_tag == from_tag
                && t.state.is_active()
        };
        let matched = self.cancelled_invite(cancel_via, is_cancel_target);

        // A server INVITE txn that has sent its final is still held — Accepted
        // after a 2xx for Timer L (RFC 6026 §7.1), Completed after a non-2xx for
        // Timer H and Confirmed after its ACK for Timer I (RFC 3261 §17.2.1) —
        // so a CANCEL matching it has no effect and is answered 200 under the
        // final's To-tag (RFC 3261 §9.2): no 487, no `Cancelled`, and the
        // established call upstream is untouched.
        if matched.is_none() {
            let is_answered_target = |t: &Transaction| {
                t.kind == TxnKind::Invite
                    && t.call_id == call_id.as_str()
                    && t.from_tag == from_tag
                    && matches!(
                        t.state,
                        TxnState::Completed | TxnState::Confirmed | TxnState::Accepted
                    )
            };
            if let Some(key) = self.cancelled_invite(cancel_via, is_answered_target) {
                let to_tag = self.uas_to_tag_of(key.id());
                let cancel_ok = generate_response(
                    &req,
                    200,
                    "OK",
                    &GenerateResponseOpts { to_tag, ..Default::default() },
                );
                self.send_buffer(endpoint, cancel_ok.image(), src).await;
                return true;
            }
        }

        // No INVITE server txn this CANCEL names: no 200, no 487, no
        // Cancelled. The TU decides between the §9.2 481 and a re-offer
        // against a rebuilt INVITE.
        let key = match matched {
            Some(key) => key,
            None => {
                if !reoffer {
                    self.emit(TransactionEvent::Message {
                        message: Box::new(SipMessage::Request(req)),
                        src,
                        matched_client_txn: false,
                    });
                }
                return false;
            }
        };

        // Snapshot the matched INVITE's CANCEL-scoping identity BEFORE the 487
        // path clears `original_request` (RFC 3261 §9): its CSeq number, and
        // whether it was an in-dialog re-INVITE (`To` already tagged). The
        // upstream consumer uses these to scope the cancellation to the one
        // transaction it targets instead of tearing the whole call down.
        let id = key.id();
        let (invite_cseq, in_dialog) = self
            .server(id)
            .and_then(|t| t.original_request.as_ref())
            .map(|r| (Some(r.cseq().seq()), r.to().tag().is_some()))
            .unwrap_or((None, false));

        // Resolve (and lazily pin) the UAS To-tag on the matched INVITE: the
        // tag both answers carry, reported upstream with the event.
        let uas_to_tag = self.uas_to_tag_of(id);

        // 200 OK to the CANCEL itself. This 200 and the 487 below are the
        // layer's own answers: they carry no header the TU states on its
        // messages.
        let cancel_ok = generate_response(
            &req,
            200,
            "OK",
            &GenerateResponseOpts { to_tag: uas_to_tag.clone(), ..Default::default() },
        );
        self.send_buffer(endpoint, cancel_ok.image(), src).await;

        // 487 Request Terminated on the matched INVITE.
        let original = self.server(id).and_then(|t| t.original_request.clone());
        if let Some(original) = original {
            let terminated = generate_response(
                &original,
                487,
                "Request Terminated",
                &GenerateResponseOpts { to_tag: uas_to_tag.clone(), ..Default::default() },
            );
            let terminated_buf = terminated.image().clone();
            self.send_buffer(endpoint, &terminated_buf, src).await;
            self.record_uas_tag(id, &terminated);
            if let Some(txn) = self.server_mut(id) {
                txn.state = TxnState::Completed;
                txn.last_response = Some(terminated_buf);
                txn.last_response_status = Some(487);
                txn.original_request = None;
            }
            // The layer's own final holds the transaction like a TU's: Timer G
            // repeats the 487 until the ACK, Timer H bounds it (§17.2.1).
            self.arm_final_hold(id, TxnKind::Invite, 487, src);
        }

        // Critical: we already answered 200 + 487 on the wire; a dropped Cancelled
        // would leave the b-leg ringing a cancelled call (no other signal upstream).
        self.emit_critical(TransactionEvent::Cancelled {
            call_id: call_id.as_str().to_string(),
            from_tag: from_tag.to_string(),
            invite_cseq,
            in_dialog,
            headers: req.headers().to_vec(),
            to_tag: uas_to_tag,
        });
        true
    }

    /// The INVITE server transaction a CANCEL with top Via `via` names —
    /// the one on its branch and sent-by (§9.2, §17.2.3) — when `pick`
    /// accepts it.
    fn cancelled_invite(
        &self,
        via: &Via,
        pick: impl Fn(&Transaction) -> bool,
    ) -> Option<ServerTxnKey> {
        ServerTxnId::cancelled_invite(via)
            .and_then(|id| self.server(id))
            .filter(|t| pick(t))
            .and_then(Transaction::server_key)
    }
}
