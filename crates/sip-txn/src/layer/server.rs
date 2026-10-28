//! RFC 3261 §17.2 server (UAS) transactions: inbound request admission
//! (auto-100, cached-response replay for duplicates, an out-of-dialog OPTIONS
//! passed up with no transaction under `stateless_options`), ACK absorption and the
//! Timer I Confirmed hold, the RFC 6026 Accepted hold after a 2xx, TU
//! response sending (Timer H/J/L arming), and Timer G non-2xx final
//! retransmission. A CANCEL of a server INVITE is `layer::server_cancel`'s;
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
use super::key::ServerTxnId;
use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{NewTransaction, Timer, TxnId, TxnRef, TxnState};

impl Owner {
    /// RFC 3261 §17.2.1 Timer G: retransmit the cached non-2xx final of an INVITE
    /// server txn still in `Completed` (not yet ACKed / Timer-H'd), then re-arm at
    /// MIN(2×interval, T2). The auto-100 we already sent silenced the UAC's INVITE
    /// retransmit, so the passive "replay the cached final on a request retransmit"
    /// path never fires — without this a single dropped reject wedges the caller
    /// for the full 32 s (Timer H). The ACK (`hold_for_timer_i`) cancels
    /// `retransmit_key` and Timer H deletes the transaction, so this stops
    /// exactly when RFC requires.
    /// Non-INVITE (Timer J) and 2xx (TU-owned §13.3.1.4 retransmit) are excluded at
    /// the arming site in `do_send_response`.
    pub(super) async fn fire_server_retransmit(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        id: ServerTxnId<'_>,
    ) {
        let (buf, dest, status) = match self.server(id) {
            Some(t) if t.kind == TxnKind::Invite && t.state == TxnState::Completed => {
                match (&t.last_response, t.destination, t.last_response_status) {
                    (Some(buf), Some(dest), Some(status)) => (buf.clone(), dest, status),
                    _ => return, // no cached final / destination — nothing to resend
                }
            }
            _ => return, // ACKed (Confirmed), Timer-H'd, or no longer a completed INVITE server txn
        };

        self.send_buffer(endpoint, &buf, dest).await;
        self.metrics.retransmits.record_final(status);

        let rearm = match self.server_mut(id).and_then(|t| t.ladder.as_mut()) {
            Some(ladder) => ladder.advance(),
            None => return,
        };
        if let Some(next_interval) = rearm {
            let key = self.arm(Timer::ServerRetransmit(id.to_key()), next_interval);
            if let Some(t) = self.server_mut(id) {
                t.retransmit_key = Some(key);
            }
        }
    }

    /// Send a TU response through its server transaction and return the
    /// datagram that left: the response's own image (ADR-0025, ADR-0032 X3),
    /// re-rendered only where `bind_to_tag` held it to the bound To-tag. The
    /// transaction is the one its request matched (§17.2.3): the response
    /// echoes that request's top Via and CSeq method, and leaves to the
    /// destination the transaction holds (§18.2.2), whatever `dest` names.
    /// `dest` addresses only a response no transaction here holds.
    pub(super) async fn do_send_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipResponse,
        dest: SocketAddr,
    ) -> Bytes {
        let status = msg.status();
        let cseq_method = msg.cseq().method().clone();

        // RFC 3261 §17.2.1: a server txn that has sent its final (Completed,
        // Confirmed once the ACK landed, Accepted after a 2xx) only
        // retransmits the STORED response. DROP any further final from the TU
        // here (a 200 racing handle_cancel's autonomous 487, a duplicate
        // relayed final, a teardown timer answering a cancelled call) —
        // re-sending it would put a second final with a different To-tag on
        // the wire, flip the ACK 2xx/non-2xx classifier
        // (last_response_status), and orphan a duplicate hold. The TU repeats
        // its own 2xx raw (§13.3.1.4). A CANCEL's answer shares the branch and
        // is not the INVITE's final.
        if cseq_method != Method::Cancel {
            if let Some(txn) = ServerTxnId::of_response(&msg).and_then(|id| self.server(id)) {
                if matches!(
                    txn.state,
                    TxnState::Completed | TxnState::Confirmed | TxnState::Accepted
                ) {
                    return msg.image().clone();
                }
            }
        }

        let msg = self.bind_to_tag(msg);
        let outbound_to_tag = if status > 100 { msg.to().tag().map(str::to_string) } else { None };
        let buf: Bytes = msg.image().clone();

        // A CANCEL response shares its INVITE's branch and this layer holds no
        // transaction for a CANCEL: it leaves raw, never through — or dropped
        // by — the INVITE transaction on that branch.
        if cseq_method == Method::Cancel {
            self.send_buffer(endpoint, &buf, dest).await;
            return buf;
        }

        let mut dest = dest;
        if let Some(id) = ServerTxnId::of_response(&msg) {
            if let Some(held) = self.server(id).map(|t| t.destination) {
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

        self.send_buffer(endpoint, &buf, dest).await;
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
    /// (Timer J) only absorbs, never retransmits.
    pub(super) fn arm_final_hold(&mut self, id: ServerTxnId<'_>, kind: TxnKind, status: u16) {
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
    /// carries the datagram as it arrived.
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
                            self.hand_up(req, src, datagram);
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
        // No transaction: every copy reaches the TU, whose answer leaves raw.
        if self.stateless_options && req.method() == Method::Options && req.to().tag().is_none() {
            self.hand_up(req, src, datagram);
            return;
        }

        // New-call admission sits above this layer (ADR-0037). This layer
        // refuses only a copy of an INVITE the node already refused and an
        // INVITE its deferred backlog has no room for, before any 100 Trying
        // or transaction exists; it admits every other INVITE, and every other
        // request its event queue takes (below).
        let kind =
            if req.method() == Method::Invite { TxnKind::Invite } else { TxnKind::NonInvite };
        let is_invite = matches!(kind, TxnKind::Invite);
        // Every response of the transaction leaves to one address: its
        // request's stamped response target (§18.2.2), fixed now.
        let Received { request: req, destination, .. } = self.stamp(req, src);
        let Some(id) = ServerTxnId::of_request(&req) else { return };
        let held = if is_invite {
            match self.judge_new_invite(endpoint, &req, destination).await {
                NewInvite::Refused => return,
                NewInvite::Admitted { held } => held,
            }
        } else {
            false
        };

        // ── New server transaction ─────────────────────────────────────────────

        // Attribute the server txn to its call so the B2BUA's acting-backup
        // self-release (ADR-0014) can count "transactions still serving this
        // call". An in-dialog request the proxy routes to the B2BUA carries the
        // `callRef` in its Request-URI (the dialog remote target = the B2BUA
        // Contact, which stamps it) — the same key the router resolves on. An
        // out-of-dialog request (initial INVITE / OPTIONS keepalive) has no
        // `callRef` param yet → `None`.
        let call_ref = extract_ruri_call_ref(&req);

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
        self.open_txn_with(head, |txn| txn.held = held);

        // An INVITE's first provisional is its consumer's to release, or this
        // layer's at the §17.2.1 bound (`layer::trying`).
        if is_invite {
            if let Some(opened) = self.server(id).map(|t| t.created_at) {
                self.arm_trying_bound(id.to_key(), opened);
            }
        }

        // Critical for INVITE: the 100 the bound sends stops the UAC
        // retransmitting, so this Message is the app's ONLY notice of the call. A non-INVITE the
        // full queue drops is not admitted: its transaction is forgotten, so the
        // UAC's Timer E copy (§17.1.2.2; UDP-only, ADR-0027) is admitted afresh
        // rather than absorbed unanswered by a Trying transaction.
        let forgettable = (!is_invite).then(|| TxnRef::Server(id).to_id());
        let event = TransactionEvent::Message {
            message: Box::new(SipMessage::Request(req)),
            src,
            matched_client_txn: false,
            datagram,
        };
        if is_invite {
            self.emit_critical(event);
        } else if !self.emit(event) {
            if let Some(key) = forgettable {
                self.delete_txn(key.as_ref());
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
    /// transaction, stamped on receipt. `false` when the event queue dropped
    /// it.
    fn hand_up(&mut self, req: SipRequest, src: SocketAddr, datagram: Bytes) -> bool {
        let req = self.stamp(req, src).request;
        self.emit(TransactionEvent::Message {
            message: Box::new(SipMessage::Request(req)),
            src,
            matched_client_txn: false,
            datagram,
        })
    }

    /// The retransmission path (RFC 3261 §17.2.1): a request a server
    /// transaction matches (§17.2.3) draws that transaction's cached
    /// response — a repeat no timer paced, counted as `trigger` under the
    /// request's method and the response's status; an Accepted INVITE answers
    /// a copy with its 2xx, this stack's choice where RFC 6026 §8.7 sends
    /// nothing (a takeover seed never sent the 2xx the copy missed). A seed
    /// `Proceeding` with no response yet (ADR-0014) owes a 100 Trying, composed
    /// from `req`, sent and cached. A non-INVITE transaction still awaiting its
    /// first response sends nothing: its request is the consumer's to answer.
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
        let Some(existing) = self.server(id) else { return false };
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
                self.send_buffer(endpoint, &cached, dest).await;
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
                self.send_buffer(endpoint, &trying_buf, dest).await;
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

/// The Request-URI `callRef` param, URL-decoded (the B2BUA's
/// `build_call_contact` percent-encodes it). URI parameter names match
/// case-insensitively per RFC 3261 §19.1.1. `None` for an out-of-dialog request,
/// which carries no such param. Attributes a server transaction to its call
/// (ADR-0014 self-release counting).
fn extract_ruri_call_ref(req: &SipRequest) -> Option<String> {
    req.request_uri().param("callRef").and_then(ParamValue::as_str).map(decode_param)
}
