//! RFC 3261 §17.2 server (UAS) transactions: inbound request admission
//! (auto-100, cached-response replay for duplicates), ACK absorption and the
//! Timer I Confirmed hold, CANCEL→200+487, TU response sending (Timer H/J
//! arming), and Timer G non-2xx final retransmission. The client (UAC) side
//! does NOT live here — see `layer::client`; a server INVITE rebuilt from a
//! record is `layer::seed`'s.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::{ParamValue, Via};
use sip_message::param_codec::decode_param;
use sip_message::{Method, SipMessage, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::event::{TransactionEvent, TxnKind};
use sip_retransmit::{Class, Ladder, Schedule};

use super::backlog::NewInvite;
use super::key::{ServerTxnId, ServerTxnKey};
use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{NewTransaction, Timer, Transaction, TxnId, TxnRef, TxnState};

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
            let key = self.timers.insert(Timer::ServerRetransmit(id.to_key()), next_interval);
            if let Some(t) = self.server_mut(id) {
                t.retransmit_key = Some(key);
            }
        }
    }

    /// Send a TU response through its server transaction and return the
    /// datagram that left: the response's own image (ADR-0025, ADR-0032 X3),
    /// re-rendered only where `bind_to_tag` held it to the bound To-tag. The
    /// transaction is the one its request matched (§17.2.3): the response
    /// echoes that request's top Via and CSeq method.
    pub(super) async fn do_send_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipResponse,
        dest: SocketAddr,
    ) -> Bytes {
        let status = msg.status();
        let cseq_method = msg.cseq().method().clone();

        // RFC 3261 §17.2.1: a server txn that has sent its final (Completed,
        // or Confirmed once the ACK landed) only retransmits the STORED
        // response. DROP any further final from the TU here (a 200 racing
        // handle_cancel's autonomous 487, a duplicate relayed final, a
        // teardown timer answering a cancelled call) — re-sending it would put
        // a second final with a different To-tag on the wire, flip the ACK
        // 2xx/non-2xx classifier (last_response_status), and orphan a
        // duplicate Timer H/J. A CANCEL's answer shares the branch and is not
        // the INVITE's final.
        if cseq_method != Method::Cancel {
            if let Some(txn) = ServerTxnId::of_response(&msg).and_then(|id| self.server(id)) {
                if matches!(txn.state, TxnState::Completed | TxnState::Confirmed) {
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

        if let Some(id) = ServerTxnId::of_response(&msg) {
            if self.server(id).is_some() {
                self.record_uas_tag(id, &msg);
                if let Some(txn) = self.server_mut(id) {
                    let is_final = status >= 200;
                    // Pin the UAS To-tag on the first >100 response (§17.2.1).
                    if txn.uas_to_tag.is_none() {
                        txn.uas_to_tag = outbound_to_tag;
                    }
                    txn.last_response = Some(buf.clone());
                    txn.last_response_status = Some(status);
                    txn.state = if is_final { TxnState::Completed } else { TxnState::Proceeding };
                    // Free memory on completion — only lastResponse is needed
                    // for retransmit absorption.
                    if is_final {
                        txn.original_request = None;
                        let kind = txn.kind;
                        self.arm_final_hold(id, kind, status, dest);
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
    /// hold, and for an INVITE answered non-2xx the Timer G ladder that
    /// re-sends the final to `dest` (T1, then ×2 capped at T2) until the ACK or
    /// Timer H. The auto-100 already silenced the UAC's INVITE retransmit, so
    /// the passive replay-on-request-retransmit path never fires and a single
    /// dropped reject would otherwise wedge the caller for the full 32 s. 2xx is
    /// exempt (the TU owns §13.3.1.4 2xx retransmission); non-INVITE (Timer J)
    /// only absorbs, never retransmits.
    fn arm_final_hold(
        &mut self,
        id: ServerTxnId<'_>,
        kind: TxnKind,
        status: u16,
        dest: SocketAddr,
    ) {
        self.hold(
            TxnRef::Server(id),
            match kind {
                TxnKind::Invite => Hold::TimerH,
                TxnKind::NonInvite => Hold::TimerJ,
            },
        );
        let arm_timer_g = matches!(kind, TxnKind::Invite) && status >= 300;
        let armed = arm_timer_g
            .then(|| Ladder::armed(Schedule::rfc(Class::InviteServerFinal)))
            .flatten()
            .map(|(ladder, first)| {
                (ladder, self.timers.insert(Timer::ServerRetransmit(id.to_key()), first))
            });
        if let Some((ladder, g_key)) = armed {
            if let Some(txn) = self.server_mut(id) {
                txn.retransmit_key = Some(g_key);
                txn.ladder = Some(ladder);
                txn.destination = Some(dest);
            }
        }
    }

    pub(super) async fn handle_inbound_request(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: SipRequest,
        src: SocketAddr,
    ) {
        let Some(id) = ServerTxnId::of_request(&req) else {
            // No branch — pass through (pre-RFC 3261 UA).
            self.emit(TransactionEvent::Message {
                message: Box::new(SipMessage::Request(req)),
                src,
                matched_client_txn: false,
            });
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
                        // ACK for 2xx — pass through to app, terminate.
                        (TxnState::Completed, Some(s)) if (200..300).contains(&s) => {
                            self.delete_txn(TxnRef::Server(id));
                            self.emit(TransactionEvent::Message {
                                message: Box::new(SipMessage::Request(req)),
                                src,
                                matched_client_txn: false,
                            });
                            return;
                        }
                        _ => {}
                    }
                }
            }
            // ACK with no matching server txn. One without a To-tag answers
            // no dialog and one acknowledging this layer's own refusal ends
            // here; a 2xx ACK always has a To-tag and passes through.
            if req.to().tag().is_none() || self.acknowledges_refusal(&req) {
                return;
            }
            self.emit(TransactionEvent::Message {
                message: Box::new(SipMessage::Request(req)),
                src,
                matched_client_txn: false,
            });
            return;
        }

        // ── CANCEL ─────────────────────────────────────────────────────────────
        if req.method() == Method::Cancel {
            self.handle_cancel(endpoint, req, src, false).await;
            return;
        }

        // ── Duplicate detection for other requests ─────────────────────────────
        if self.replay_cached(endpoint, &req, src).await {
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
        let held = if is_invite {
            match self.judge_new_invite(endpoint, &req, src).await {
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

        self.open_txn(NewTransaction {
            id: TxnId::Server(id.to_key()),
            kind,
            method: req.method().clone(),
            call_id: req.call_id().as_str().to_string(),
            from_tag: req.from().tag().unwrap_or_default().to_string(),
            // INVITE server txns keep the request for the CANCEL→487 path; a
            // non-INVITE server txn never reads it, so skip that clone.
            original_request: is_invite.then(|| req.clone()),
            call_ref,
            leg_id: None,
            incarnation_mark: None,
            state: TxnState::Trying,
            // An INVITE's source, where a final the layer itself owes it is
            // sent (`answer_unanswered_invites_of_call`); the TU's final
            // re-points it.
            destination: is_invite.then_some(src),
        })
        .held = held;

        // For INVITE, immediately send 100 Trying and move to proceeding.
        if is_invite {
            // The recipe froze the 100 into its own image, which IS its wire
            // form — send and cache that instead of rendering it twice.
            let trying_buf =
                generate_response(&req, 100, "Trying", &GenerateResponseOpts::default())
                    .image()
                    .clone();
            self.send_buffer(endpoint, &trying_buf, src).await;
            if let Some(txn) = self.server_mut(id) {
                txn.state = TxnState::Proceeding;
                // Cache the 100 as the latest provisional so a retransmitted INVITE
                // replays it (RFC 3261 §17.2.1) instead of being absorbed silently —
                // the auto-100 already silenced the UAC's own retransmission timer.
                txn.last_response = Some(trying_buf);
                txn.last_response_status = Some(100);
            }
        }

        // Critical for INVITE: the 100 we just sent stops the UAC retransmitting,
        // so this Message is the app's ONLY notice of the call. A non-INVITE the
        // full queue drops is not admitted: its transaction is forgotten, so the
        // UAC's Timer E copy (§17.1.2.2; UDP-only, ADR-0027) is admitted afresh
        // rather than absorbed unanswered by a Trying transaction.
        let forgettable = (!is_invite).then(|| TxnRef::Server(id).to_id());
        let event = TransactionEvent::Message {
            message: Box::new(SipMessage::Request(req)),
            src,
            matched_client_txn: false,
        };
        if is_invite {
            self.emit_critical(event);
        } else if !self.emit(event) {
            if let Some(key) = forgettable {
                self.delete_txn(key.as_ref());
            }
        }
    }

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
    /// response target). A seed holding no request cannot be answered and is
    /// left to its backstop.
    /// Counted; returns how many.
    pub(super) async fn answer_unanswered_invites_of_call(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        call_ref: &str,
        status: u16,
        reason: &str,
    ) -> usize {
        let unanswered: Vec<(ServerTxnKey, SipRequest, Option<SocketAddr>)> = self
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
            let resp = generate_response(
                &req,
                status,
                reason,
                &GenerateResponseOpts { to_tag, ..Default::default() },
            );
            self.do_send_response(endpoint, resp, dest).await;
            answered += 1;
        }
        self.metrics
            .released_unanswered_invites_answered
            .fetch_add(answered as u64, std::sync::atomic::Ordering::Relaxed);
        answered
    }

    /// The retransmission path (RFC 3261 §17.2.1): a request a server
    /// transaction matches (§17.2.3) draws that transaction's cached
    /// response — a repeat no timer paced, counted as `trigger` under the
    /// request's method and the response's status. A `Proceeding` INVITE
    /// server transaction holding no response yet (a seed rebuilt from a
    /// record, ADR-0014) owes the retransmission its most recent provisional:
    /// a 100 Trying is composed from `req`, sent and cached, as the admit path
    /// does for the first copy. A non-INVITE transaction still awaiting its
    /// first response sends nothing: its request is the consumer's to answer.
    /// A copy the event queue drops, or the consumer discards before handling
    /// it ([`TransactionLayer::forget_unanswered`](crate::TransactionLayer::forget_unanswered)),
    /// leaves no transaction behind. `true` when a transaction absorbed the
    /// request, `false` when none matches it or the branch is empty.
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
        match replay {
            Some((cached, status, method)) => {
                self.send_buffer(endpoint, &cached, src).await;
                self.metrics.retransmits.record_trigger(&method, status);
            }
            None if owes_trying => {
                let trying_buf =
                    generate_response(req, 100, "Trying", &GenerateResponseOpts::default())
                        .image()
                        .clone();
                self.send_buffer(endpoint, &trying_buf, src).await;
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
    fn uas_to_tag_of(&mut self, id: ServerTxnId<'_>) -> Option<String> {
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

        // Find the matching ACTIVE INVITE server txn. The CANCEL shares the
        // INVITE's top-Via branch and sent-by (RFC 3261 §9.1, §9.2), so a
        // compliant peer keys it directly — an O(1) `get` instead of an
        // O(total_txns) scan; we still confirm callId+fromTag (and fall back to
        // the scan, still on the sent-by, for a peer that didn't preserve the
        // branch).
        let cancel_via = req.top_via();
        // A CANCEL target holds the INVITE it answers 487 for: a seed rebuilt
        // without its request absorbs the INVITE's retransmissions only.
        let is_cancel_target = |t: &Transaction| {
            t.kind == TxnKind::Invite
                && t.call_id == call_id.as_str()
                && t.from_tag == from_tag
                && t.state.is_active()
                && t.original_request.is_some()
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
                    && matches!(t.state, TxnState::Completed | TxnState::Confirmed)
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

        // No INVITE server txn at all: no 200, no 487, no Cancelled. The TU
        // decides between the §9.2 481 and a re-offer against a rebuilt INVITE.
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

    /// The INVITE server transaction a CANCEL with top Via `via` names and
    /// `pick` accepts: the one on its branch and sent-by (§9.2), else any on
    /// its sent-by, for a peer that did not keep the INVITE's branch.
    fn cancelled_invite(
        &self,
        via: &Via,
        pick: impl Fn(&Transaction) -> bool,
    ) -> Option<ServerTxnKey> {
        let same_sender = |t: &Transaction| {
            t.server_id().is_some_and(|id| id.sent_by() == via.sent_by_ref()) && pick(t)
        };
        ServerTxnId::cancelled_invite(via)
            .and_then(|id| self.server(id))
            .filter(|t| pick(t))
            .or_else(|| self.servers.values().map(|t| &**t).find(|t| same_sender(t)))
            .and_then(Transaction::server_key)
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

/// Where a response to `req` goes when its source is unknown (a re-INVITE
/// seeded at a takeover): the top Via's response target (RFC 3261 §18.2.2),
/// when it names an IP address.
fn via_response_target(req: &SipRequest) -> Option<SocketAddr> {
    let (host, port) = req.top_via().response_target();
    let ip: std::net::IpAddr = host.trim_start_matches('[').trim_end_matches(']').parse().ok()?;
    Some(SocketAddr::new(ip, port))
}
