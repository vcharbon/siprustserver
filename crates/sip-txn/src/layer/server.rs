//! RFC 3261 §17.2 server (UAS) transactions: inbound request admission
//! (auto-100, cached-response replay for duplicates), ACK absorption and the
//! Timer I Confirmed hold, CANCEL→200+487, TU response sending (Timer H/J
//! arming), and Timer G non-2xx final retransmission. The client (UAC) side
//! does NOT live here — see `layer::client`; a server INVITE rebuilt from a
//! record is `layer::seed`'s.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::ParamValue;
use sip_message::param_codec::decode_param;
use sip_message::{Method, SipMessage, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::event::{TransactionEvent, TxnKind};
use crate::timers::{ms, TIMER_H, TIMER_I, TIMER_J};
use sip_retransmit::{Class, Ladder, Schedule};

use super::owner::Owner;
use super::txn::{NewTransaction, Timer, Transaction, TxnRole, TxnState};

impl Owner {
    /// RFC 3261 §17.2.1 Timer G: retransmit the cached non-2xx final of an INVITE
    /// server txn still in `Completed` (not yet ACKed / Timer-H'd), then re-arm at
    /// MIN(2×interval, T2). The auto-100 we already sent silenced the UAC's INVITE
    /// retransmit, so the passive "replay the cached final on a request retransmit"
    /// path never fires — without this a single dropped reject wedges the caller
    /// for the full 32 s (Timer H). The ACK (`hold_for_timer_i`) or Timer H
    /// cancels `retransmit_key`, so this stops exactly when RFC requires.
    /// Non-INVITE (Timer J) and 2xx (TU-owned §13.3.1.4 retransmit) are excluded at
    /// the arming site in `do_send_response`.
    pub(super) async fn fire_server_retransmit(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
    ) {
        let (buf, dest, status) = match self.txns.get(branch) {
            Some(t)
                if t.role == TxnRole::Server
                    && t.kind == TxnKind::Invite
                    && t.state == TxnState::Completed =>
            {
                match (&t.last_response, t.destination, t.last_response_status) {
                    (Some(buf), Some(dest), Some(status)) => (buf.clone(), dest, status),
                    _ => return, // no cached final / destination — nothing to resend
                }
            }
            _ => return, // ACKed (Confirmed), Timer-H'd, or no longer a completed INVITE server txn
        };

        self.send_buffer(endpoint, &buf, dest).await;
        self.metrics.retransmits.record_final(status);

        let rearm = match self.txns.get_mut(branch).and_then(|t| t.ladder.as_mut()) {
            Some(ladder) => ladder.advance(),
            None => return,
        };
        if let Some(next_interval) = rearm {
            let key =
                self.timers.insert(Timer::ServerRetransmit(branch.to_string()), next_interval);
            if let Some(t) = self.txns.get_mut(branch) {
                t.retransmit_key = Some(key);
            }
        }
    }

    /// Send a TU response through its server transaction and return the
    /// datagram that left: the response's own image (ADR-0025, ADR-0029 X3),
    /// re-rendered only where `bind_to_tag` held it to the bound To-tag.
    pub(super) async fn do_send_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipResponse,
        dest: SocketAddr,
    ) -> Bytes {
        let status = msg.status();
        let cseq_method = msg.cseq().method().clone();
        let branch = msg.top_via().branch().map(str::to_string);
        let branch = branch.as_deref();

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
            if let Some(txn) = branch.and_then(|b| self.txns.get(b)) {
                if txn.role == TxnRole::Server
                    && matches!(txn.state, TxnState::Completed | TxnState::Confirmed)
                {
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

        if let Some(branch) = branch {
            self.record_uas_tag(branch, &msg);
            if let Some(txn) = self.txns.get_mut(branch) {
                if txn.role == TxnRole::Server {
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
                        self.arm_final_hold(branch, kind, status, dest);
                    }
                }
            } else if status >= 300 && cseq_method == Method::Invite {
                // A non-2xx INVITE final on a branch no transaction holds is
                // DROPPED: the transaction that admitted the INVITE — or its
                // seed (ADR-0014) — has either sent its one final and left, or
                // never existed here, and only it can run the Timer G ladder
                // the final owes. Counted, so a takeover that materialised
                // without a seed shows. Every other status on an unseen branch
                // leaves raw.
                self.metrics
                    .server_final_unseen_branch
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return buf;
            }
        }

        self.send_buffer(endpoint, &buf, dest).await;
        buf
    }

    /// Park the Completed INVITE server txn at `branch` in Confirmed for
    /// Timer I (RFC 3261 §17.2.1): the ACK ended Timer G and Timer H, and the
    /// branch keeps absorbing ACK retransmissions and refusing a second final
    /// until the T4 cleanup deletes it. Its protocol obligation to the call is
    /// met, so it leaves the call's books now (ADR-0014).
    fn hold_for_timer_i(&mut self, branch: &str) {
        let Some(txn) = self.txns.get_mut(branch) else { return };
        let (timer_g, timer_h) = (txn.retransmit_key.take(), txn.cleanup_key.take());
        txn.state = TxnState::Confirmed;
        txn.ladder = None;
        self.cancel_timer(timer_g);
        self.cancel_timer(timer_h);
        let key = self.timers.insert(Timer::Cleanup(branch.to_string()), ms(TIMER_I));
        if let Some(txn) = self.txns.get_mut(branch) {
            txn.cleanup_key = Some(key);
        }
        self.detach_from_call(branch);
    }

    /// Arm a server txn's post-final timers (RFC 3261 §17.2): the Timer H/J
    /// cleanup, and for an INVITE answered non-2xx the Timer G ladder that
    /// re-sends the final to `dest` (T1, then ×2 capped at T2) until the ACK or
    /// Timer H. The auto-100 already silenced the UAC's INVITE retransmit, so
    /// the passive replay-on-request-retransmit path never fires and a single
    /// dropped reject would otherwise wedge the caller for the full 32 s. 2xx is
    /// exempt (the TU owns §13.3.1.4 2xx retransmission); non-INVITE (Timer J)
    /// only absorbs, never retransmits.
    fn arm_final_hold(&mut self, branch: &str, kind: TxnKind, status: u16, dest: SocketAddr) {
        let delay = match kind {
            TxnKind::Invite => TIMER_H,
            TxnKind::NonInvite => TIMER_J,
        };
        let arm_timer_g = matches!(kind, TxnKind::Invite) && status >= 300;
        // Disjoint-field borrow: txns and timers are separate Owner fields.
        let key = self.timers.insert(Timer::Cleanup(branch.to_string()), ms(delay));
        let armed = arm_timer_g
            .then(|| Ladder::armed(Schedule::rfc(Class::InviteServerFinal)))
            .flatten()
            .map(|(ladder, first)| {
                (ladder, self.timers.insert(Timer::ServerRetransmit(branch.to_string()), first))
            });
        if let Some(txn) = self.txns.get_mut(branch) {
            txn.cleanup_key = Some(key);
            if let Some((ladder, g_key)) = armed {
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
        let top_via = req.top_via();
        let branch = top_via.branch().unwrap_or_default();

        if branch.is_empty() {
            // No branch — pass through (pre-RFC 3261 UA).
            self.emit(TransactionEvent::Message {
                message: Box::new(SipMessage::Request(req)),
                src,
                matched_client_txn: false,
            });
            return;
        }

        // ── ACK ──────────────────────────────────────────────────────────────
        if req.method() == Method::Ack {
            if let Some(existing) = self.txns.get(branch) {
                if existing.role == TxnRole::Server && existing.kind == TxnKind::Invite {
                    match (existing.state, existing.last_response_status) {
                        // A retransmitted ACK in Confirmed — absorbed (§17.2.1).
                        (TxnState::Confirmed, _) => return,
                        // ACK for non-2xx (3xx-6xx) — absorb, hold for Timer I.
                        (TxnState::Completed, Some(s)) if s >= 300 => {
                            self.hold_for_timer_i(branch);
                            return;
                        }
                        // ACK for 2xx — pass through to app, terminate.
                        (TxnState::Completed, Some(s)) if (200..300).contains(&s) => {
                            self.delete_txn(branch);
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
            // ACK with no matching server txn. A stateless-503 ACK carries no
            // To-tag and must be absorbed (not propagated); a legitimate 2xx
            // ACK always has a To-tag and passes through.
            if req.to().tag().is_none() {
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

        // Tier-3 overload admission (a stateless 503 ahead of txn creation)
        // deliberately sits ABOVE this layer — see docs/adr/0007 "Deferred".
        // This layer admits unconditionally.

        // ── New server transaction ─────────────────────────────────────────────
        let kind =
            if req.method() == Method::Invite { TxnKind::Invite } else { TxnKind::NonInvite };
        let is_invite = matches!(kind, TxnKind::Invite);

        // Attribute the server txn to its call so the B2BUA's acting-backup
        // self-release (ADR-0014) can count "transactions still serving this
        // call". An in-dialog request the proxy routes to the B2BUA carries the
        // `callRef` in its Request-URI (the dialog remote target = the B2BUA
        // Contact, which stamps it) — the same key the router resolves on. An
        // out-of-dialog request (initial INVITE / OPTIONS keepalive) has no
        // `callRef` param yet → `None`.
        let call_ref = extract_ruri_call_ref(&req);

        self.set_txn(Transaction::new(NewTransaction {
            branch: branch.to_string(),
            role: TxnRole::Server,
            kind,
            method: req.method().clone(),
            call_id: req.call_id().as_str().to_string(),
            from_tag: req.from().tag().unwrap_or_default().to_string(),
            // INVITE server txns keep the request for the CANCEL→487 path; a
            // non-INVITE server txn never reads it, so skip that clone.
            original_request: is_invite.then(|| req.clone()),
            call_ref,
            leg_id: None,
            state: TxnState::Trying,
            destination: None,
        }));

        // For INVITE, immediately send 100 Trying and move to proceeding.
        if is_invite {
            // The recipe froze the 100 into its own image, which IS its wire
            // form — send and cache that instead of rendering it twice.
            let trying_buf =
                generate_response(&req, 100, "Trying", &GenerateResponseOpts::default())
                    .image()
                    .clone();
            self.send_buffer(endpoint, &trying_buf, src).await;
            if let Some(txn) = self.txns.get_mut(branch) {
                txn.state = TxnState::Proceeding;
                // Cache the 100 as the latest provisional so a retransmitted INVITE
                // replays it (RFC 3261 §17.2.1) instead of being absorbed silently —
                // the auto-100 already silenced the UAC's own retransmission timer.
                txn.last_response = Some(trying_buf);
                txn.last_response_status = Some(100);
            }
        }

        // Critical for INVITE: the 100 we just sent stops the UAC retransmitting,
        // so this Message is the app's ONLY notice of the call — a drop would leave
        // a timer-less server txn squatting until the sweep while the caller hears
        // 100-then-silence. Non-INVITE requests stay lossy (the UAC resends them).
        let event = TransactionEvent::Message {
            message: Box::new(SipMessage::Request(req)),
            src,
            matched_client_txn: false,
        };
        if is_invite {
            self.emit_critical(event);
        } else {
            self.emit(event);
        }
    }

    /// The retransmission path (RFC 3261 §17.2.1): a request whose branch
    /// already holds a server transaction draws that transaction's cached
    /// response — a repeat no timer paced, counted as `trigger` under the
    /// request's method and the response's status. A `Proceeding` INVITE
    /// server transaction holding no response yet (a seed rebuilt from a
    /// record, ADR-0014) owes the retransmission its most recent provisional:
    /// a 100 Trying is composed from `req`, sent and cached, as the admit path
    /// does for the first copy. A non-INVITE transaction still awaiting its
    /// first response sends nothing. `true` when a transaction absorbed the
    /// request, `false` when the branch is unseen or empty.
    pub(super) async fn replay_cached(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        req: &SipRequest,
        src: SocketAddr,
    ) -> bool {
        let branch = req.top_via().branch().unwrap_or_default();
        if branch.is_empty() {
            return false;
        }
        let Some(existing) = self.txns.get(branch) else { return false };
        let replay = existing
            .last_response
            .clone()
            .zip(existing.last_response_status)
            .map(|(cached, status)| (cached, status, existing.method.clone()));
        let owes_trying = existing.role == TxnRole::Server
            && existing.kind == TxnKind::Invite
            && existing.state == TxnState::Proceeding;
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
                if let Some(txn) = self.txns.get_mut(branch) {
                    txn.last_response = Some(trying_buf);
                    txn.last_response_status = Some(100);
                }
            }
            None => {}
        }
        true
    }

    /// The To-tag the server INVITE txn on `branch` has bound
    /// (`Transaction::bound_to_tag`), pinned now where none was yet: a request
    /// that named the dialog is answered under its own To-tag (RFC 3261
    /// §8.2.6.2), any other under a fresh one.
    fn uas_to_tag_of(&mut self, branch: &str) -> Option<String> {
        let known = self.txns.get(branch).and_then(|t| t.bound_to_tag().map(str::to_string));
        if known.is_some() {
            return known;
        }
        let requested = self
            .txns
            .get(branch)
            .and_then(|t| t.original_request.as_ref())
            .and_then(|r| r.to().tag().map(str::to_string));
        let pinned = requested.unwrap_or_else(|| self.id_gen.new_tag());
        if let Some(txn) = self.txns.get_mut(branch) {
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
        // INVITE's top-Via branch (RFC 3261 §9.1), so a compliant peer keys it
        // directly — an O(1) `get` instead of an O(total_txns) scan; we still
        // confirm callId+fromTag (and fall back to the scan for a peer that didn't
        // preserve the branch).
        let cancel_via = req.top_via();
        let cancel_branch = cancel_via.branch().unwrap_or_default();
        // A CANCEL target holds the INVITE it answers 487 for: a seed rebuilt
        // without its request absorbs the INVITE's retransmissions only.
        let is_cancel_target = |t: &Transaction| {
            t.role == TxnRole::Server
                && t.kind == TxnKind::Invite
                && t.call_id == call_id.as_str()
                && t.from_tag == from_tag
                && t.state.is_active()
                && t.original_request.is_some()
        };
        let matched_branch = self
            .txns
            .get(cancel_branch)
            .filter(|t| is_cancel_target(t))
            .map(|_| cancel_branch.to_string())
            .or_else(|| self.txns.iter().find_map(|(b, t)| is_cancel_target(t).then(|| b.clone())));

        // A server INVITE txn that has sent its final is still held — Accepted
        // after a 2xx for Timer L (RFC 6026 §7.1), Completed after a non-2xx for
        // Timer H and Confirmed after its ACK for Timer I (RFC 3261 §17.2.1) —
        // so a CANCEL matching it has no effect and is answered 200 under the
        // final's To-tag (RFC 3261 §9.2): no 487, no `Cancelled`, and the
        // established call upstream is untouched.
        if matched_branch.is_none() {
            let is_answered_target = |t: &Transaction| {
                t.role == TxnRole::Server
                    && t.kind == TxnKind::Invite
                    && t.call_id == call_id.as_str()
                    && t.from_tag == from_tag
                    && matches!(t.state, TxnState::Completed | TxnState::Confirmed)
            };
            let answered = self
                .txns
                .get(cancel_branch)
                .filter(|t| is_answered_target(t))
                .map(|_| cancel_branch.to_string())
                .or_else(|| {
                    self.txns.iter().find_map(|(b, t)| is_answered_target(t).then(|| b.clone()))
                });
            if let Some(branch) = answered {
                let to_tag = self.uas_to_tag_of(&branch);
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
        let branch = match matched_branch {
            Some(b) => b,
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
        let (invite_cseq, in_dialog) = self
            .txns
            .get(branch.as_str())
            .and_then(|t| t.original_request.as_ref())
            .map(|r| (Some(r.cseq().seq()), r.to().tag().is_some()))
            .unwrap_or((None, false));

        // Resolve (and lazily pin) the UAS To-tag on the matched INVITE.
        let uas_to_tag = self.uas_to_tag_of(&branch);

        // 200 OK to the CANCEL itself.
        let cancel_ok = generate_response(
            &req,
            200,
            "OK",
            &GenerateResponseOpts { to_tag: uas_to_tag.clone(), ..Default::default() },
        );
        self.send_buffer(endpoint, cancel_ok.image(), src).await;

        // 487 Request Terminated on the matched INVITE.
        let original = self.txns.get(branch.as_str()).and_then(|t| t.original_request.clone());
        if let Some(original) = original {
            let terminated = generate_response(
                &original,
                487,
                "Request Terminated",
                &GenerateResponseOpts { to_tag: uas_to_tag, ..Default::default() },
            );
            let terminated_buf = terminated.image().clone();
            self.send_buffer(endpoint, &terminated_buf, src).await;
            self.record_uas_tag(&branch, &terminated);
            if let Some(txn) = self.txns.get_mut(branch.as_str()) {
                txn.state = TxnState::Completed;
                txn.last_response = Some(terminated_buf);
                txn.last_response_status = Some(487);
                txn.original_request = None;
            }
            // The layer's own final holds the transaction like a TU's: Timer G
            // repeats the 487 until the ACK, Timer H bounds it (§17.2.1).
            self.arm_final_hold(&branch, TxnKind::Invite, 487, src);
        }

        // Critical: we already answered 200 + 487 on the wire; a dropped Cancelled
        // would leave the b-leg ringing a cancelled call (no other signal upstream).
        self.emit_critical(TransactionEvent::Cancelled {
            call_id: call_id.as_str().to_string(),
            from_tag: from_tag.to_string(),
            invite_cseq,
            in_dialog,
            headers: req.headers().to_vec(),
        });
        true
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
