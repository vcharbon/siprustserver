//! RFC 6026 §7.2 Accepted for an INVITE client transaction: entered on its
//! first 2xx and held to Timer M. One owner per ACK (RFC 3261 §13.2.2.4):
//! while the call holds the transaction the TU sends every ACK, and the layer
//! records the one it sends for each dialog; once the call releases the
//! transaction the layer sends them (`layer::orphan`). Also kept here: the
//! reliable provisionals that carried the offer of an offerless INVITE.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::header::RSeq;
use sip_message::{SipMessage, SipRequest, SipResponse};
use sip_net::parse_datagram;

use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{CancelWire, TxnRef, TxnState, MAX_DIALOGS};
use super::via_marks::{extract_via_custom_params, ViaMarks};

impl Owner {
    /// Move the client INVITE txn at `branch` to Accepted on its first 2xx:
    /// every timer it ran stops, a CANCEL still parked on it is moot (§9.2;
    /// counted dropped only if it never left), the 2xx's dialog is recorded,
    /// Timer M holds it from this 2xx, and it leaves its call's books
    /// ([`Owner::settle_accepted`]). The parsed INVITE, several times the size
    /// of its image, gives way to that image, all an Accepted txn reads of it
    /// beside its CSeq number; the orphan path parses the image again
    /// ([`Owner::accepted_invite`]).
    pub(super) fn accept_2xx(&mut self, branch: &str, resp: &SipResponse) {
        let keys = match self.clients.get_mut(branch) {
            Some(txn) => {
                txn.state = TxnState::Accepted;
                if txn.held_cancel.take().is_some_and(|h| h.wire == CancelWire::Held) {
                    self.metrics
                        .held_cancels_dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                if let Some(invite) = txn.original_request.take() {
                    txn.retransmit_buf = Some(invite.image().clone());
                }
                txn.seed_resend = None;
                txn.ladder = None;
                let _ = txn.dialog(resp.to().tag());
                (
                    txn.retransmit_key.take(),
                    txn.timeout_key.take(),
                    txn.cancel_retransmit_key.take(),
                )
            }
            None => return,
        };
        self.cancel_timer(keys.0);
        self.cancel_timer(keys.1);
        self.cancel_timer(keys.2);
        self.hold(TxnRef::Client(branch), Hold::TimerM);
        self.settle_accepted(branch);
    }

    /// Record `ack`, which the TU just sent to `dest` as `image`, on the dialog
    /// of the Accepted INVITE client txn it acknowledges: one of its call's
    /// (Via `cr`), on the same Call-ID, From-tag and CSeq number (§13.2.2.4),
    /// under the ACK's To-tag. An ACK matching none is the TU's alone.
    pub(super) fn note_tu_ack(&mut self, ack: &SipRequest, image: &Bytes, dest: SocketAddr) {
        let ViaMarks { call_ref, .. } = extract_via_custom_params(ack);
        let Some(txns) = call_ref.as_deref().and_then(|cr| self.txn_index.get(cr)) else {
            return;
        };
        let (call_id, from_tag) = (ack.call_id().as_str(), ack.from().tag().unwrap_or_default());
        let seq = ack.cseq().seq();
        let Some(branch) = txns
            .accepted
            .iter()
            .find(|b| {
                self.clients.get(b.as_str()).is_some_and(|t| {
                    t.state == TxnState::Accepted
                        && t.call_id == call_id
                        && t.from_tag == from_tag
                        && t.cseq == seq
                })
            })
            .cloned()
        else {
            return;
        };
        if let Some(dialog) = self.clients.get_mut(&branch).and_then(|t| t.dialog(ack.to().tag())) {
            dialog.ack = Some((image.clone(), dest));
        }
    }

    /// The INVITE of the Accepted client txn at `branch`, parsed again from
    /// the image it keeps, a parser panic contained as at ingress; `None`,
    /// logged, where that image reads as none.
    pub(super) fn accepted_invite(&self, branch: &str) -> Option<SipRequest> {
        let image = self.clients.get(branch)?.retransmit_buf.clone()?;
        match parse_datagram(image, |raw| self.parser.parse_shared(raw)) {
            Ok(Ok(SipMessage::Request(invite))) => Some(invite),
            _ => {
                tracing::warn!(%branch, "the INVITE an Accepted transaction keeps reads as none");
                None
            }
        }
    }

    /// Note the To-tag of `resp`, a provisional to the INVITE client txn at
    /// `branch`, when it is reliable (RFC 3262 §7.1: it bears an RSeq) and
    /// carries the offer the INVITE did not (§5): that dialog's offer is
    /// answered in its PRACK, never in its 2xx's ACK.
    pub(super) fn note_reliable_offer(&mut self, branch: &str, resp: &SipResponse) {
        let Some(tag) = resp.to().tag() else { return };
        if resp.sdp().is_none() || !resp.header::<RSeq>().is_some_and(|r| r.is_ok()) {
            return;
        }
        let Some(txn) = self.clients.get_mut(branch) else { return };
        let offerless = txn.original_request.as_ref().is_some_and(|o| o.sdp().is_none());
        if offerless
            && txn.reliable_offers.len() < MAX_DIALOGS
            && !txn.reliable_offers.iter().any(|t| t == tag)
        {
            txn.reliable_offers.push(tag.to_string());
        }
    }
}
