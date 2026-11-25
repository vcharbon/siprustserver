//! RFC 3261 §17.2 server (UAS) transactions: inbound request admission
//! (auto-100, cached-response replay for duplicates, an out-of-dialog OPTIONS
//! passed up with no transaction under `stateless_options`), ACK absorption and the
//! Timer I Confirmed hold, the RFC 6026 Accepted hold after a 2xx, TU
//! response sending (Timer H/J/L arming). Timer G non-2xx final
//! retransmission is `layer::timer_g`'s, a CANCEL of a server INVITE
//! `layer::server_cancel`'s;
//! the client (UAC) side is `layer::client`'s; a server INVITE rebuilt from
//! a record is `layer::seed`'s.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::ParamValue;
use sip_message::param_codec::decode_param;
use sip_message::received::{stamp_received, stamped_destination, Received, Stamp};
use sip_message::{Method, SipMessage, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::event::TransactionEvent;
use call::TxnKind;
use sip_retransmit::{Class, Ladder, Schedule};

use super::backlog::NewInvite;
use super::call_id_budget::Retired;
use super::copy_gate::opening_copies;
use super::key::{ServerTxnId, ServerTxnKey};
use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{NewTransaction, Timer, TxnId, TxnRef, TxnState};
use super::unattributed_lane::Lane;

impl Owner {
    /// Send a TU response through its server transaction and return the
    /// datagram that left: the response's own image (ADR-0025, ADR-0032 X3),
    /// re-rendered only where `bind_to_tag` held it to the bound To-tag. The
    /// transaction is the one its request matched (§17.2.3): the response
    /// echoes that request's top Via and CSeq method, and leaves to the
    /// destination the transaction holds (§18.2.2), whatever `dest` names.
    /// `dest` addresses only a response no transaction here holds. A CANCEL's
    /// response goes through the CANCEL's own transaction (a stray CANCEL's,
    /// `layer::server_cancel`); one no transaction holds leaves raw, never
    /// through, nor dropped by, the INVITE transaction on its branch.
    /// A transaction that has sent its final (Completed, Confirmed, Accepted)
    /// only retransmits the stored one (§17.2.1): any response the TU sends it
    /// after is dropped — a 200 racing the layer's own 487, a duplicate
    /// relayed final — so no second final leaves under another To-tag. The TU
    /// repeats its own 2xx raw (§13.3.1.4).
    pub(super) async fn do_send_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipResponse,
        dest: SocketAddr,
    ) -> Bytes {
        let status = msg.status();
        let cseq_method = msg.cseq().method().clone();

        let held = ServerTxnId::of_response(&msg).and_then(|id| self.server(id));
        if let Some(txn) = held {
            if matches!(txn.state, TxnState::Completed | TxnState::Confirmed | TxnState::Accepted) {
                return msg.image().clone();
            }
        }
        let cancel_unheld = cseq_method == Method::Cancel && held.is_none();

        let msg = self.bind_to_tag(msg);
        let outbound_to_tag = if status > 100 { msg.to().tag().map(str::to_string) } else { None };
        let buf: Bytes = msg.image().clone();

        if cancel_unheld {
            self.send_buffer(endpoint, msg.call_id().as_str(), &buf, dest).await;
            return buf;
        }

        let mut dest = dest;
        let mut in_txn = false;
        if let Some(id) = ServerTxnId::of_response(&msg) {
            if let Some(held) = self.server(id).map(|t| t.destination) {
                in_txn = true;
                dest = held.unwrap_or(dest);
                self.record_uas_tag(id, &msg);
                if let Some(txn) = self.server_mut(id) {
                    let is_final = status >= 200;
                    // Pin the UAS To-tag on the first >100 response (§17.2.1).
                    if txn.uas_to_tag.is_none() {
                        txn.uas_to_tag = outbound_to_tag;
                    }
                    txn.last_response = Some(buf.clone());
                    txn.last_response_status = Some(status);
                    txn.state = state_after(txn.kind, status);
                    // Free memory on completion — only lastResponse is needed
                    // for retransmit absorption.
                    if is_final {
                        txn.original_request = None;
                        drop_parked_487(txn);
                        let kind = txn.kind;
                        self.arm_final_hold(id, kind, status);
                    }
                }
            } else if status >= 300 && cseq_method == Method::Invite {
                // A non-2xx INVITE final no server transaction holds is
                // DROPPED: the transaction that admitted the INVITE — or its
                // seed (ADR-0014) — has either sent its one final and left, or
                // never existed here, and only it can run the Timer G ladder
                // the final owes. Counted, so a takeover that materialised
                // without a seed shows. Every other status on an unseen
                // transaction leaves raw.
                self.metrics
                    .server_final_unseen_branch
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return buf;
            }
        }

        if !in_txn && self.on_keepalive_lane(&cseq_method, true) {
            self.send_uncounted(endpoint, &buf, dest).await;
        } else {
            self.send_buffer(endpoint, msg.call_id().as_str(), &buf, dest).await;
        }
        buf
    }

    /// Park the Completed INVITE server txn `id` names in Confirmed for
    /// Timer I (RFC 3261 §17.2.1): the ACK ends Timer G, Timer I replaces
    /// Timer H, and the branch keeps absorbing ACK retransmissions and
    /// refusing a second final until Timer I deletes it. Its protocol
    /// obligation to the call is met, so it leaves the call's books now
    /// (ADR-0014).
    fn hold_for_timer_i(&mut self, id: ServerTxnId<'_>) {
        let Some(txn) = self.server_mut(id) else { return };
        let timer_g = txn.retransmit_key.take();
        txn.state = TxnState::Confirmed;
        txn.ladder = None;
        self.cancel_timer(timer_g);
        self.hold(TxnRef::Server(id), Hold::TimerI);
        self.detach_from_call(TxnRef::Server(id));
    }

    /// Arm a server txn's post-final timers (RFC 3261 §17.2): the Timer H/J
    /// hold, or Timer L for an INVITE Accepted on a 2xx (RFC 6026 §7.1), and
    /// for an INVITE answered non-2xx the Timer G ladder that re-sends the
    /// final to the transaction's destination (T1, then ×2 capped at T2)
    /// until the ACK or Timer H.
    /// The auto-100 already silenced the UAC's INVITE retransmit, so the
    /// passive replay-on-request-retransmit path never fires and a single
    /// dropped reject would otherwise wedge the caller for the full 32 s. 2xx
    /// is exempt (the TU owns §13.3.1.4 2xx retransmission); non-INVITE
    /// (Timer J) only absorbs, never retransmits. An unattributed transaction
    /// stops serving its call here (`layer::unattributed`).
    pub(super) fn arm_final_hold(&mut self, id: ServerTxnId<'_>, kind: TxnKind, status: u16) {
        self.unattributed_answered(id);
        self.hold(
            TxnRef::Server(id),
            match (kind, status) {
                (TxnKind::Invite, 200..=299) => Hold::TimerL,
                (TxnKind::Invite, _) => Hold::TimerH,
                (TxnKind::NonInvite, _) => Hold::TimerJ,
            },
        );
        let arm_timer_g = matches!(kind, TxnKind::Invite) && status >= 300;
        let armed = arm_timer_g
            .then(|| Ladder::armed(Schedule::rfc(Class::InviteServerFinal)))
            .flatten()
            .map(|(ladder, first)| (ladder, self.arm(Timer::ServerRetransmit(id.to_key()), first)));
        if let Some((ladder, g_key)) = armed {
            if let Some(txn) = self.server_mut(id) {
                txn.retransmit_key = Some(g_key);
                txn.ladder = Some(ladder);
            }
        }
    }

    /// Admit a request received from `src`. A copy a transaction absorbs is
    /// matched as it arrived (§17.2.3 reads no `received`/`rport`); a request
    /// that goes on — handed to the TU, opening a transaction, a CANCEL, a
    /// refused INVITE — is first stamped with what the transport observed
    /// (RFC 3261 §18.2.1, RFC 3581 §4), once, so the TU, the transaction and
    /// every response composed from it read the stamped hop. The event still
    /// carries the datagram as it arrived. A copy whose transaction left
    /// within 64·T1 of its last copy meets that transaction's record
    /// (ADR-0063 §1): past the schedule it is absorbed, else the transaction
    /// it re-opens carries the count on. A request no transaction absorbed
    /// that rides the unattributed lane ([`lane_of`](Self::lane_of)) and
    /// finds it full is dropped before its copy record is read, before it is
    /// stamped or judged, and before any transaction opens. An ACK a 2xx's
    /// Accepted transaction matches rides the event queue.
    pub(super) async fn handle_inbound_request(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: SipRequest,
        src: SocketAddr,
    ) {
        let datagram = req.image().clone();
        let Some(id) = ServerTxnId::of_request(&req) else {
            // No branch — pass through (pre-RFC 3261 UA).
            self.hand_up(req, src, datagram);
            return;
        };

        // ── ACK ──────────────────────────────────────────────────────────────
        if req.method() == Method::Ack {
            if let Some(existing) = self.server(id) {
                if existing.kind == TxnKind::Invite {
                    match (existing.state, existing.last_response_status) {
                        // A retransmitted ACK in Confirmed — absorbed (§17.2.1).
                        (TxnState::Confirmed, _) => return,
                        // ACK for non-2xx (3xx-6xx) — absorb, hold for Timer I.
                        (TxnState::Completed, Some(s)) if s >= 300 => {
                            self.hold_for_timer_i(id);
                            return;
                        }
                        // An ACK for the 2xx (RFC 6026 §8.7): passed up, every
                        // copy; the transaction stays Accepted for Timer L.
                        (TxnState::Accepted, _) => {
                            self.detach_from_call(TxnRef::Server(id));
                            let event = self.upward(req, src, datagram);
                            self.emit(event);
                            return;
                        }
                        _ => {}
                    }
                }
            }
            // ACK with no matching server txn. One without a To-tag answers
            // no dialog and one acknowledging this layer's own refusal or
            // parse-refusal 400 ends here; a 2xx ACK always has a To-tag and
            // passes through.
            if req.to().tag().is_none()
                || self.acknowledges_refusal(&req)
                || self.acknowledges_parse_refusal(&req)
            {
                return;
            }
            self.hand_up(req, src, datagram);
            return;
        }

        // ── CANCEL ─────────────────────────────────────────────────────────────
        if req.method() == Method::Cancel {
            let cancel = self.stamp(req, src).request;
            self.handle_cancel(endpoint, cancel, src, Some(datagram)).await;
            return;
        }

        // ── Duplicate detection for other requests ─────────────────────────────
        if self.replay_cached(endpoint, &req, src).await {
            return;
        }

        // ── Out-of-dialog OPTIONS, stateless (RFC 3261 §8.2.7) ─────────────────
        // No transaction: every copy reaches the TU, on the keepalive lane when
        // the layer runs one, and its answer leaves raw.
        if self.on_keepalive_lane(req.method(), req.to().tag().is_none()) {
            let event = self.upward(req, src, datagram);
            self.emit_keepalive(event);
            return;
        }

        let lane = self.lane_of(&req);
        if !self.lane_has_room(lane) {
            return;
        }
        let retired = self.retired_copy(&req);
        if matches!(retired, Retired::Absorbed) {
            return;
        }

        // New-call admission sits above this layer (ADR-0037). This layer
        // refuses, before any 100 Trying or transaction exists, a copy of an
        // INVITE the node already refused, an INVITE its deferred backlog has
        // no room for, and one its new-call gate refuses, on the consumer's
        // switch or the queue's age; it admits every other INVITE, and every
        // other request its event queue takes (below).
        let kind =
            if req.method() == Method::Invite { TxnKind::Invite } else { TxnKind::NonInvite };
        let is_invite = matches!(kind, TxnKind::Invite);
        // Every response of the transaction leaves to one address: its
        // request's stamped response target (§18.2.2), fixed now.
        let Received { request: req, destination, .. } = self.stamp(req, src);
        let Some(id) = ServerTxnId::of_request(&req) else { return };
        let continued = matches!(retired, Retired::Reopens(_));
        let copies = opening_copies(req.method(), retired);
        let held = if is_invite {
            match self.judge_new_invite(endpoint, &req, destination).await {
                NewInvite::Refused => return,
                NewInvite::Admitted { held } => held,
            }
        } else {
            false
        };

        // ── New server transaction ─────────────────────────────────────────────

        let call_ref = extract_ruri_call_ref(&req);
        let unattributed = call_ref.is_none() && req.to().tag().is_some();
        let bound_at_opening = is_invite && self.trying_bound_at_opening(&req);

        let head = NewTransaction {
            id: TxnId::Server(id.to_key()),
            kind,
            method: req.method().clone(),
            cseq: req.cseq().seq(),
            call_id: req.call_id().as_str().to_string(),
            from_tag: req.from().tag().unwrap_or_default().to_string(),
            // INVITE server txns keep the request for the CANCEL→487 path; a
            // non-INVITE server txn never reads it, so skip that clone.
            original_request: is_invite.then(|| req.clone()),
            call_ref,
            leg_id: None,
            incarnation_mark: None,
            state: TxnState::Trying,
            destination: Some(destination),
        };
        self.open_txn_with(head, |txn| {
            txn.held = held;
            txn.copies = Some(copies);
        });
        if unattributed {
            self.unattributed.opened(req.call_id().as_str(), id.to_key());
        }

        // An INVITE's first provisional is its consumer's to release, or this
        // layer's at the §17.2.1 bound (`layer::trying`).
        let opened_at = self.server(id).map(|t| t.created_at);
        if let (true, Some(opened)) = (bound_at_opening, opened_at) {
            self.arm_trying_bound(id.to_key(), opened);
        }

        let forgettable = (!is_invite || lane == Lane::Unattributed).then(|| id.to_key());
        let event = TransactionEvent::Message {
            message: Box::new(SipMessage::Request(req)),
            src,
            matched_client_txn: false,
            datagram,
            opened_at,
        };
        self.hand_up_opened(event, lane, forgettable, continued);
    }

    /// Hand the request a new server transaction opened on up as `event`, on
    /// `lane`. `forgettable` names a transaction whose event may be lost: one
    /// the lane drops is not admitted but forgotten, so the UAC's copy (Timer
    /// E, §17.1.2.2, or Timer A, §17.1.1.2; UDP-only, ADR-0027) is admitted
    /// afresh rather than absorbed unanswered. That is a non-INVITE's, and an
    /// INVITE's on the unattributed lane, which found room before its
    /// transaction opened. An INVITE's on the event queue (`None`) is never
    /// dropped: its transaction absorbs its copies, so it is the consumer's
    /// only notice of the call.
    fn hand_up_opened(
        &mut self,
        event: TransactionEvent,
        lane: Lane,
        forgettable: Option<ServerTxnKey>,
        continued: bool,
    ) {
        match forgettable {
            None => self.emit_critical(event),
            Some(key) => {
                if !self.emit_on(lane, event) {
                    self.forget_unqueued(key.id(), continued);
                }
            }
        }
    }

    /// `req`, received from `src`, stamped on receipt ([`stamp_received`]);
    /// a stamp the render cannot write is counted.
    fn stamp(&self, req: SipRequest, src: SocketAddr) -> Received {
        let received = stamp_received(req, src);
        if received.stamp == Stamp::Unwritten {
            self.metrics.via_stamps_unwritten.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        received
    }

    /// Hand `req`, received from `src` as `datagram`, to the TU with no
    /// transaction, stamped on receipt, on its lane
    /// ([`lane_of`](Self::lane_of)). `false` when the lane dropped it.
    fn hand_up(&mut self, req: SipRequest, src: SocketAddr, datagram: Bytes) -> bool {
        let lane = self.lane_of(&req);
        let event = self.upward(req, src, datagram);
        self.emit_on(lane, event)
    }

    /// The event passing `req`, received from `src` as `datagram`, up to the
    /// consumer, its top Via stamped (RFC 3261 §18.2.1).
    fn upward(&self, req: SipRequest, src: SocketAddr, datagram: Bytes) -> TransactionEvent {
        let req = self.stamp(req, src).request;
        TransactionEvent::Message {
            message: Box::new(SipMessage::Request(req)),
            src,
            matched_client_txn: false,
            datagram,
            opened_at: None,
        }
    }

    /// The retransmission path (RFC 3261 §17.2.1): a request a server
    /// transaction matches (§17.2.3) draws that transaction's cached
    /// response — a repeat no timer paced, counted as `trigger` under the
    /// request's method and the response's status — on the RFC schedule
    /// (ADR-0063 §1, [`request_copy_owed`](Self::request_copy_owed)): a copy
    /// past it is absorbed. An Accepted INVITE absorbs every copy (RFC 6026
    /// §7.1) but the first an Accepted seed meets before its 2xx left this
    /// node, which draws that 2xx. A seed `Proceeding` with no response yet
    /// (ADR-0014) owes a 100 Trying, composed from `req`, sent and cached. A
    /// non-INVITE transaction still awaiting its first response sends
    /// nothing: its request is the consumer's to answer.
    /// A copy the event queue drops, or the consumer discards before handling
    /// it ([`TransactionLayer::forget_unanswered`](crate::TransactionLayer::forget_unanswered)),
    /// leaves no transaction behind. What it sends goes to the destination
    /// the transaction holds, else to the copy's response target (§18.2.2).
    /// `true` when a transaction absorbed the request, `false` when none
    /// matches it or the branch is empty.
    pub(super) async fn replay_cached(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: &SipRequest,
        src: SocketAddr,
    ) -> bool {
        let Some(id) = ServerTxnId::of_request(req) else { return false };
        // A copy of an INVITE whose CANCEL's answers are still held: they
        // answer it.
        if self.holds_cancel(id) {
            return true;
        }
        let Some(existing) = self.server(id) else { return false };
        if existing.state == TxnState::Accepted && !existing.unsent_2xx {
            self.absorb_request_copy(id);
            return true;
        }
        if !self.request_copy_owed(id) {
            return true;
        }
        let Some(existing) = self.server_mut(id) else { return true };
        existing.unsent_2xx = false;
        let replay = existing
            .last_response
            .clone()
            .zip(existing.last_response_status)
            .map(|(cached, status)| (cached, status, existing.method.clone()));
        let owes_trying =
            existing.kind == TxnKind::Invite && existing.state == TxnState::Proceeding;
        let dest = existing.destination.unwrap_or_else(|| stamped_destination(req.top_via(), src));
        match replay {
            Some((cached, status, method)) => {
                self.send_buffer(endpoint, req.call_id().as_str(), &cached, dest).await;
                self.metrics.retransmits.record_trigger(&method, status);
            }
            None if owes_trying => {
                // The 100 answers the copy (its Timestamp, §8.2.6.1), stamped
                // on receipt.
                let request = self.stamp(req.clone(), src).request;
                let opts = GenerateResponseOpts::default();
                let Ok(trying) = generate_response(&request, 100, "Trying", &opts) else {
                    return true;
                };
                let trying_buf = trying.image().clone();
                self.send_buffer(endpoint, req.call_id().as_str(), &trying_buf, dest).await;
                if let Some(txn) = self.server_mut(id) {
                    txn.last_response = Some(trying_buf);
                    txn.last_response_status = Some(100);
                }
            }
            None => {}
        }
        true
    }

    /// The To-tag the server INVITE txn `id` names has bound
    /// (`Transaction::bound_to_tag`), pinned now where none was yet: a request
    /// that named the dialog is answered under its own To-tag (RFC 3261
    /// §8.2.6.2), any other under a fresh one.
    pub(super) fn uas_to_tag_of(&mut self, id: ServerTxnId<'_>) -> Option<String> {
        let known = self.server(id).and_then(|t| t.bound_to_tag().map(str::to_string));
        if known.is_some() {
            return known;
        }
        let requested = self
            .server(id)
            .and_then(|t| t.original_request.as_ref())
            .and_then(|r| r.to().tag().map(str::to_string));
        let pinned = requested.unwrap_or_else(|| self.id_gen.new_tag());
        if let Some(txn) = self.server_mut(id) {
            txn.uas_to_tag = Some(pinned.clone());
        }
        Some(pinned)
    }
}

/// The 487 `txn`, an INVITE left open by its CANCEL, keeps for its consumer's
/// release is no longer owed once the INVITE takes a final of its own: dropped,
/// so no release and no leaving sends a second final (RFC 3261 §17.2.1). The
/// CANCEL's 200 still held stays owed.
fn drop_parked_487(txn: &mut super::txn::Transaction) {
    if let Some(held) = txn.unsent_cancel.as_mut().filter(|u| u.invite_open) {
        held.terminated = None;
    }
}

/// The state a server transaction of `kind` enters on sending a response of
/// `status`: Proceeding on a provisional, Accepted on an INVITE's 2xx
/// (RFC 6026 §7.1), Completed on any other final (RFC 3261 §17.2).
fn state_after(kind: TxnKind, status: u16) -> TxnState {
    match (kind, status) {
        (_, ..=199) => TxnState::Proceeding,
        (TxnKind::Invite, 200..=299) => TxnState::Accepted,
        _ => TxnState::Completed,
    }
}

/// The call a new server transaction is attributed to as it opens: the
/// Request-URI `callRef` param, URL-decoded (the B2BUA's `build_call_contact`
/// percent-encodes it), matched case-insensitively (RFC 3261 §19.1.1); the
/// key the router resolves on when it is there. An in-dialog request whose
/// remote target is the B2BUA's Contact carries it. `None` for an
/// out-of-dialog request and for one an element rewrote the Request-URI of:
/// the router attributes the latter once it finds the call by dialog identity
/// ([`TransactionLayer::attach_server_to_call`](crate::TransactionLayer::attach_server_to_call)),
/// and until then the self-release count of a call naming its Call-ID counts
/// it (`layer::unattributed`). The attribution is what the self-release count
/// (ADR-0014) and a release's forget and answer read.
pub(super) fn extract_ruri_call_ref(req: &SipRequest) -> Option<String> {
    req.request_uri().param("callRef").and_then(ParamValue::as_str).map(decode_param)
}
