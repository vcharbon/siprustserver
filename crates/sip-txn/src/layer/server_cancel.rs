//! A CANCEL received for an INVITE server transaction (RFC 3261 §9.2): the
//! INVITE it names answered 487 and the CANCEL 200 while that INVITE is
//! active, the 487 of a re-INVITE left to the consumer
//! (`consumer_ends_cancelled_reinvite`, `layer::held_cancel`); the CANCEL
//! alone answered 200 once the INVITE took its final or the CANCEL was taken
//! already, and handed up through a non-INVITE transaction of its own when no
//! INVITE here matches it. The rest of the server transaction lives in
//! `layer::server`.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::Via;
use sip_message::received::stamped_destination;
use sip_message::{Method, SipMessage, SipRequest};
use sip_net::UdpEndpoint;

use crate::event::TransactionEvent;
use call::TxnKind;

use super::call_id_budget::Retired;
use super::copy_gate::opening_copies;
use super::key::{ServerTxnId, ServerTxnKey};
use super::owner::Owner;
use super::txn::{NewTransaction, Transaction, TxnId, TxnRef, TxnState, UnsentCancel};

impl Owner {
    /// RFC 3261 §9.2 at this layer: a CANCEL matching an active INVITE server
    /// transaction is answered 200 and its INVITE 487, the 487 of a re-INVITE
    /// left to the consumer instead (`consumer_ends_cancelled_reinvite`), and
    /// `Cancelled` is emitted; `true`. Its copies, once the INVITE took its
    /// final or while a re-INVITE left open awaits it, draw the 200 again on
    /// the RFC schedule ([`cancel_copy_owed`](Self::cancel_copy_owed)), never
    /// handed up. One matching nothing
    /// here is the TU's to answer — a call it holds for a peer may hold the
    /// INVITE the CANCEL names, and only the TU can rebuild that transaction
    /// and re-offer the CANCEL — so it is handed up unanswered as a `Message`
    /// carrying `datagram`, through a transaction of its own
    /// ([`take_stray_cancel`](Self::take_stray_cancel), `false`); a re-offer
    /// (`datagram` `None`) still unmatched is handed nowhere. The 200 goes to
    /// the CANCEL's response target, the 487 to the INVITE transaction's
    /// destination (§18.2.2). A CANCEL with a transaction of its own is
    /// answered through it: the 200 settles it, and once it holds a final
    /// (that 200 or the TU's 481) that final answers every copy on one count
    /// (§17.2.2), before any INVITE is looked up, so a copy never cancels an
    /// INVITE rebuilt on its branch after the CANCEL was answered.
    pub(super) async fn handle_cancel(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: SipRequest,
        src: SocketAddr,
        datagram: Option<Bytes>,
    ) -> bool {
        let cancel_dest = stamped_destination(req.top_via(), src);
        let call_id = req.call_id();
        let from = req.from();
        let from_tag = from.tag().unwrap_or_default();

        // The CANCEL shares its INVITE's top-Via branch and sent-by (RFC 3261
        // §9.1), the INVITE transaction's key (§9.2, §17.2.3): one lookup, and
        // a miss is no match. Call-ID and From-tag are confirmed on the hit.
        let cancel_via = req.top_via();
        if self.cancel_answers_held(cancel_via) {
            return true;
        }
        if self.cancel_answered_in_own_txn(&req) {
            self.replay_cached(endpoint, &req, src).await;
            return true;
        }
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
                if !self.cancel_copy_owed(key.id()) {
                    return true;
                }
                let to_tag = self.uas_to_tag_of(key.id());
                let opts = GenerateResponseOpts { to_tag, ..Default::default() };
                if let Ok(cancel_ok) = generate_response(&req, 200, "OK", &opts) {
                    self.settle_cancel_txn(&req, cancel_ok.image());
                    self.send_buffer(endpoint, call_id.as_str(), cancel_ok.image(), cancel_dest)
                        .await;
                }
                return true;
            }
        }

        // No INVITE server txn this CANCEL names: no 200, no 487, no
        // Cancelled. The TU decides between the §9.2 481 and a re-offer
        // against a rebuilt INVITE.
        let key = match matched {
            Some(key) => key,
            None => {
                if let Some(datagram) = datagram {
                    self.take_stray_cancel(endpoint, req, src, datagram).await;
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
        if self.server(id).is_some_and(|t| t.cancel_copies.is_some()) {
            return self.answer_cancel_copy(endpoint, &req, id, cancel_dest).await;
        }

        // Resolve (and lazily pin) the UAS To-tag on the matched INVITE: the
        // tag both answers carry, reported upstream with the event.
        let uas_to_tag = self.uas_to_tag_of(id);
        self.note_first_cancel(id);
        let invite_open = in_dialog && self.consumer_ends_cancelled_reinvite;

        // 200 OK to the CANCEL itself. This 200 and the 487 below are the
        // layer's own answers: they carry no header the TU states on its
        // messages.
        // A request whose lines no response can echo draws none.
        let opts = GenerateResponseOpts { to_tag: uas_to_tag.clone(), ..Default::default() };
        let hold = self.holds_cancel_answers() || invite_open;
        let cancel_ok = generate_response(&req, 200, "OK", &opts).ok();
        let cancel_ok = cancel_ok.map(|ok| (ok.image().clone(), cancel_dest));
        if let Some((buf, _)) = cancel_ok.as_ref() {
            self.settle_cancel_txn(&req, buf);
        }
        if let Some((buf, dest)) = cancel_ok.as_ref().filter(|_| !hold) {
            self.send_buffer(endpoint, call_id.as_str(), buf, *dest).await;
        }

        // 487 Request Terminated on the matched INVITE.
        let invite_dest = self.server(id).and_then(|t| t.destination).unwrap_or(cancel_dest);
        let original = self.server(id).and_then(|t| t.original_request.clone());
        let terminated =
            original.and_then(|o| generate_response(&o, 487, "Request Terminated", &opts).ok());
        let mut held_487 = None;
        if let Some(terminated) = terminated {
            let terminated_buf = terminated.image().clone();
            if hold {
                held_487 = Some((terminated_buf.clone(), invite_dest));
            } else {
                self.send_buffer(endpoint, call_id.as_str(), &terminated_buf, invite_dest).await;
            }
            self.record_uas_tag(id, &terminated);
            if !invite_open {
                self.complete_with_487(id, terminated_buf);
            }
            // The layer's own final holds the transaction like a TU's: Timer G
            // repeats the 487 until the ACK, Timer H bounds it (§17.2.1);
            // from its release when it is held.
            if !hold {
                self.arm_final_hold(id, TxnKind::Invite, 487);
            }
        }
        if hold {
            let held_as = (
                call_id.as_str().to_string(),
                from_tag.to_string(),
                invite_cseq.unwrap_or_default(),
            );
            let answers = UnsentCancel { cancel_ok, terminated: held_487, held_as, invite_open };
            self.hold_cancel_answers(&key, answers);
        }

        // Critical: the 200 and the 487 are sent or held for this event's turn; a
        // dropped Cancelled would leave the b-leg ringing a cancelled call.
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

    /// A copy of the CANCEL `req` the still active INVITE server transaction
    /// `id` already took, its INVITE left open for the consumer's final
    /// ([`TransactionConfig::consumer_ends_cancelled_reinvite`](crate::TransactionConfig)):
    /// answered 200 again on the RFC schedule (§17.2.2), handed up never;
    /// `true`.
    async fn answer_cancel_copy(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: &SipRequest,
        id: ServerTxnId<'_>,
        dest: SocketAddr,
    ) -> bool {
        if !self.cancel_copy_owed(id) {
            return true;
        }
        let to_tag = self.uas_to_tag_of(id);
        let opts = GenerateResponseOpts { to_tag, ..Default::default() };
        if let Ok(ok) = generate_response(req, 200, "OK", &opts) {
            self.send_buffer(endpoint, req.call_id().as_str(), ok.image(), dest).await;
        }
        true
    }

    /// The INVITE server transaction `id` takes `terminated`, the layer's 487,
    /// as its final: Completed, its request gone (§17.2.1).
    pub(super) fn complete_with_487(&mut self, id: ServerTxnId<'_>, terminated: Bytes) {
        if let Some(txn) = self.server_mut(id) {
            txn.state = TxnState::Completed;
            txn.last_response = Some(terminated);
            txn.last_response_status = Some(487);
            txn.original_request = None;
        }
    }

    /// A CANCEL received from `src` as `datagram` that names no INVITE here
    /// is answered through a non-INVITE server transaction of its own (RFC
    /// 3261 §9.2, ADR-0063 §1): handed up once, for the TU's 481 or its
    /// re-offer, its copies answered from that transaction on the RFC
    /// schedule. A copy whose transaction left meets the record it left: past
    /// the schedule it is absorbed. A CANCEL the event queue drops, or one the
    /// TU answers statelessly (`layer::stateless_answer`), leaves no
    /// transaction behind.
    async fn take_stray_cancel(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: SipRequest,
        src: SocketAddr,
        datagram: Bytes,
    ) {
        if self.replay_cached(endpoint, &req, src).await {
            return;
        }
        let retired = self.retired_copy(&req);
        if matches!(retired, Retired::Absorbed) {
            return;
        }
        let Some(key) = ServerTxnKey::of(&req) else { return };
        let head = NewTransaction {
            id: TxnId::Server(key.clone()),
            kind: TxnKind::NonInvite,
            method: Method::Cancel,
            cseq: req.cseq().seq(),
            call_id: req.call_id().as_str().to_string(),
            from_tag: req.from().tag().unwrap_or_default().to_string(),
            original_request: None,
            call_ref: None,
            leg_id: None,
            incarnation_mark: None,
            state: TxnState::Trying,
            destination: Some(stamped_destination(req.top_via(), src)),
        };
        let copies = opening_copies(&Method::Cancel, retired);
        self.open_txn_with(head, |txn| txn.copies = Some(copies));
        let event = TransactionEvent::Message {
            message: Box::new(SipMessage::Request(req)),
            src,
            matched_client_txn: false,
            datagram,
            opened_at: None,
        };
        if !self.emit(event) {
            self.delete_txn(TxnRef::Server(key.id()));
        }
    }

    /// Whether the 200 to a CANCEL with top Via `via` is still held: it
    /// answers the CANCEL's copies when released. Once it left with the 487 of
    /// an INVITE left open still held, a copy is answered again
    /// ([`Self::answer_cancel_copy`]).
    fn cancel_answers_held(&self, via: &Via) -> bool {
        let held =
            |t: &Transaction| t.unsent_cancel.as_ref().is_some_and(|u| u.cancel_ok.is_some());
        self.cancelled_invite(via, held).is_some()
    }

    /// Whether the CANCEL `req`'s own server transaction (a stray CANCEL's,
    /// [`take_stray_cancel`](Self::take_stray_cancel)) holds its final: that
    /// transaction answers every copy of it.
    fn cancel_answered_in_own_txn(&self, req: &SipRequest) -> bool {
        ServerTxnId::of_request(req)
            .and_then(|id| self.server(id))
            .is_some_and(|t| !t.state.is_active())
    }

    /// The CANCEL `req`'s own server transaction, still unanswered, takes
    /// `ok`, the 200 this layer answers the CANCEL with, as its final:
    /// Completed under Timer J (RFC 3261 §17.2.2), so a later copy draws `ok`
    /// from it and the TU's own answer to the CANCEL is dropped.
    fn settle_cancel_txn(&mut self, req: &SipRequest, ok: &Bytes) {
        let Some(id) = ServerTxnId::of_request(req) else { return };
        let Some(txn) = self.server_mut(id).filter(|t| t.state.is_active()) else { return };
        txn.state = TxnState::Completed;
        txn.last_response = Some(ok.clone());
        txn.last_response_status = Some(200);
        self.arm_final_hold(id, TxnKind::NonInvite, 200);
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
