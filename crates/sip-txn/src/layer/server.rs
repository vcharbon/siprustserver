//! RFC 3261 §17.2 server (UAS) transactions: inbound request admission
//! (auto-100, cached-response replay for duplicates), ACK absorption,
//! CANCEL→200+487, TU response sending (Timer H/J arming), and Timer G non-2xx
//! final retransmission. The client (UAC) side does NOT live here — see
//! `layer::client`.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::ParamValue;
use sip_message::message_helpers::decode_param;
use sip_message::{serialize, Method, SipMessage, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::event::{TransactionEvent, TxnKind};
use crate::timers::{ms, T1, T2, TIMER_B, TIMER_H, TIMER_J};

use super::owner::Owner;
use super::txn::{Timer, Transaction, TxnRole, TxnState};

impl Owner {
    /// RFC 3261 §17.2.1 Timer G: retransmit the cached non-2xx final of an INVITE
    /// server txn still in `Completed` (not yet ACKed / Timer-H'd), then re-arm at
    /// MIN(2×interval, T2). The auto-100 we already sent silenced the UAC's INVITE
    /// retransmit, so the passive "replay the cached final on a request retransmit"
    /// path never fires — without this a single dropped reject wedges the caller
    /// for the full 32 s (Timer H). The ACK (or Timer H) `delete_txn`s the branch,
    /// which cancels `retransmit_key`, so this stops exactly when RFC requires.
    /// Non-INVITE (Timer J) and 2xx (TU-owned §13.3.1.4 retransmit) are excluded at
    /// the arming site in `do_send_response`.
    pub(super) async fn fire_server_retransmit(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let (buf, dest, next_interval) = match self.txns.get(branch) {
            Some(t)
                if t.role == TxnRole::Server
                    && t.kind == TxnKind::Invite
                    && t.state == TxnState::Completed =>
            {
                match (&t.last_response, t.destination) {
                    (Some(buf), Some(dest)) => {
                        (buf.clone(), dest, std::cmp::min(t.retransmit_interval_ms * 2, T2))
                    }
                    _ => return, // no cached final / destination — nothing to resend
                }
            }
            _ => return, // ACKed (deleted), Timer-H'd, or no longer a completed INVITE server txn
        };

        self.send_buffer(endpoint, &buf, dest).await;
        self.metrics
            .server_final_retransmits
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let key = self
            .timers
            .insert(Timer::ServerRetransmit(branch.to_string()), ms(next_interval));
        if let Some(t) = self.txns.get_mut(branch) {
            t.retransmit_key = Some(key);
            t.retransmit_interval_ms = next_interval;
        }
    }

    pub(super) async fn do_send_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipResponse,
        dest: SocketAddr,
    ) {
        // Every field this layer needs is read before the response goes to the
        // serializer BY VALUE, so keeping a `&SipMessage` alive costs no
        // whole-message clone. The response is rendered rather than sent as its
        // own image: the TU may have edited the header list of a message it
        // parsed, so only a message this layer itself built carries its wire form.
        let status = msg.status;
        let top_via = msg.top_via();
        let branch = top_via.branch();
        let outbound_to_tag =
            if status > 100 { msg.to().tag().map(str::to_string) } else { None };
        let buf = Bytes::from(serialize(&SipMessage::Response(msg)));

        if let Some(branch) = branch {
            if let Some(txn) = self.txns.get_mut(branch) {
                if txn.role == TxnRole::Server {
                    // RFC 3261 §17.2.1: a Completed server txn has already sent its
                    // final and now only retransmits the STORED response on a
                    // request retransmit. DROP any further final from the TU here
                    // (a 200 racing handle_cancel's autonomous 487, or a duplicate
                    // relayed final) — re-sending it would put a second final with a
                    // different To-tag on the wire, flip the ACK 2xx/non-2xx
                    // classifier (last_response_status), and orphan a duplicate
                    // Timer H/J. The stored final + dup-absorption cover retransmits.
                    if txn.state == TxnState::Completed {
                        return;
                    }

                    let is_final = status >= 200;
                    // Pin the UAS To-tag on the first >100 response (§17.2.1).
                    if txn.uas_to_tag.is_none() {
                        txn.uas_to_tag = outbound_to_tag;
                    }
                    txn.last_response = Some(buf.clone());
                    txn.last_response_status = Some(status);
                    txn.state = if is_final {
                        TxnState::Completed
                    } else {
                        TxnState::Proceeding
                    };
                    // Free memory on completion — only lastResponse is needed
                    // for retransmit absorption.
                    if is_final {
                        txn.original_request = None;
                        // Schedule Timer H/J cleanup (disjoint-field borrow: txns
                        // and timers are separate Owner fields).
                        let delay = match txn.kind {
                            TxnKind::Invite => TIMER_H,
                            TxnKind::NonInvite => TIMER_J,
                        };
                        // RFC 3261 §17.2.1: an INVITE server txn that answered NON-2xx
                        // MUST actively retransmit the final (Timer G, T1 then ×2 capped
                        // at T2) until the ACK or Timer H — our auto-100 already silenced
                        // the UAC's INVITE retransmit, so the passive replay-on-request-
                        // retransmit path never fires and a single dropped reject would
                        // otherwise wedge the caller for the full 32 s. 2xx is exempt (the
                        // TU owns §13.3.1.4 2xx retransmission); non-INVITE (Timer J) only
                        // absorbs, never retransmits.
                        let arm_timer_g = matches!(txn.kind, TxnKind::Invite) && status >= 300;
                        let key = self.timers.insert(Timer::Cleanup(branch.to_string()), ms(delay));
                        let g_key = arm_timer_g.then(|| {
                            self.timers
                                .insert(Timer::ServerRetransmit(branch.to_string()), ms(T1))
                        });
                        if let Some(txn) = self.txns.get_mut(branch) {
                            txn.cleanup_key = Some(key);
                            if let Some(g_key) = g_key {
                                txn.retransmit_key = Some(g_key);
                                txn.retransmit_interval_ms = T1;
                                txn.destination = Some(dest);
                            }
                        }
                    }
                }
            }
        }

        self.send_buffer(endpoint, &buf, dest).await;
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
            });
            return;
        }

        // ── ACK ──────────────────────────────────────────────────────────────
        if req.method == Method::Ack {
            if let Some(existing) = self.txns.get(branch) {
                if existing.role == TxnRole::Server
                    && existing.kind == TxnKind::Invite
                    && existing.state == TxnState::Completed
                {
                    match existing.last_response_status {
                        // ACK for non-2xx (3xx-6xx) — absorb, terminate.
                        Some(s) if s >= 300 => {
                            self.delete_txn(branch);
                            return;
                        }
                        // ACK for 2xx — pass through to app, terminate.
                        Some(s) if (200..300).contains(&s) => {
                            self.delete_txn(branch);
                            self.emit(TransactionEvent::Message {
                                message: Box::new(SipMessage::Request(req)),
                                src,
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
            });
            return;
        }

        // ── CANCEL ─────────────────────────────────────────────────────────────
        if req.method == Method::Cancel {
            self.handle_cancel(endpoint, req, src).await;
            return;
        }

        // ── Duplicate detection for other requests ─────────────────────────────
        if let Some(existing) = self.txns.get(branch) {
            if let Some(cached) = existing.last_response.clone() {
                self.send_buffer(endpoint, &cached, src).await;
            }
            // else: duplicate with no response yet — absorb silently.
            return;
        }

        // Tier-3 overload admission (a stateless 503 ahead of txn creation)
        // deliberately sits ABOVE this layer — see docs/adr/0007 "Deferred".
        // This layer admits unconditionally.

        // ── New server transaction ─────────────────────────────────────────────
        let kind = if req.method == Method::Invite {
            TxnKind::Invite
        } else {
            TxnKind::NonInvite
        };
        let is_invite = matches!(kind, TxnKind::Invite);

        // Attribute the server txn to its call so the B2BUA's acting-backup
        // self-release (ADR-0014) can count "transactions still serving this
        // call". An in-dialog request the proxy routes to the B2BUA carries the
        // `callRef` in its Request-URI (the dialog remote target = the B2BUA
        // Contact, which stamps it) — the same key the router resolves on. An
        // out-of-dialog request (initial INVITE / OPTIONS keepalive) has no
        // `callRef` param yet → `None`.
        let call_ref = extract_ruri_call_ref(&req);

        let txn = Transaction {
            branch: branch.to_string(),
            role: TxnRole::Server,
            kind,
            call_id: req.call_id().as_str().to_string(),
            from_tag: req.from().tag().unwrap_or_default().to_string(),
            // INVITE server txns keep the request for the CANCEL→487 path; a
            // non-INVITE server txn never reads it, so skip that clone.
            original_request: is_invite.then(|| req.clone()),
            last_response: None,
            last_response_status: None,
            call_ref,
            leg_id: None,
            state: TxnState::Trying,
            destination: None,
            created_at: tokio::time::Instant::now(),
            uas_to_tag: None,
            retransmit_key: None,
            timeout_key: None,
            cleanup_key: None,
            retransmit_buf: None,
            retransmit_interval_ms: T1,
            retransmit_elapsed_ms: T1,
            retransmit_max_ms: TIMER_B,
        };
        self.set_txn(txn);

        // For INVITE, immediately send 100 Trying and move to proceeding.
        if is_invite {
            // The recipe froze the 100 into its own image, which IS its wire
            // form — send and cache that instead of rendering it twice.
            let trying_buf = generate_response(&req, 100, "Trying", &GenerateResponseOpts::default()).raw;
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
        };
        if is_invite {
            self.emit_critical(event);
        } else {
            self.emit(event);
        }
    }

    async fn handle_cancel(&mut self, endpoint: &dyn UdpEndpoint, req: SipRequest, src: SocketAddr) {
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
        let is_cancel_target = |t: &Transaction| {
            t.role == TxnRole::Server
                && t.kind == TxnKind::Invite
                && t.call_id == call_id.as_str()
                && t.from_tag == from_tag
                && t.state.is_active()
        };
        let matched_branch = self
            .txns
            .get(cancel_branch)
            .filter(|t| is_cancel_target(t))
            .map(|_| cancel_branch.to_string())
            .or_else(|| {
                self.txns
                    .iter()
                    .find_map(|(b, t)| is_cancel_target(t).then(|| b.clone()))
            });

        // RFC 3261 §9.2: a CANCEL matching no active INVITE server txn gets a 481
        // and has NO effect. An unconditional 200 + Cancelled event here would let
        // a late/retransmitted/replayed CANCEL — one arriving after the call was
        // answered (txn Completed, not active) — tear down an established call
        // upstream, and give the retransmitted CANCEL a tagless 200 plus a
        // duplicate Cancelled. Reject it cleanly instead.
        let branch = match matched_branch {
            Some(b) => b,
            None => {
                let reject = generate_response(
                    &req,
                    481,
                    "Call/Transaction Does Not Exist",
                    &GenerateResponseOpts::default(),
                );
                self.send_buffer(endpoint, &reject.raw, src).await;
                return;
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
        let mut uas_to_tag = self.txns.get(branch.as_str()).and_then(|t| t.uas_to_tag.clone());
        if uas_to_tag.is_none() {
            let pinned = self.id_gen.new_tag();
            if let Some(txn) = self.txns.get_mut(branch.as_str()) {
                txn.uas_to_tag = Some(pinned.clone());
            }
            uas_to_tag = Some(pinned);
        }

        // 200 OK to the CANCEL itself.
        let cancel_ok = generate_response(
            &req,
            200,
            "OK",
            &GenerateResponseOpts {
                to_tag: uas_to_tag.clone(),
                ..Default::default()
            },
        );
        self.send_buffer(endpoint, &cancel_ok.raw, src).await;

        // 487 Request Terminated on the matched INVITE.
        let original = self
            .txns
            .get(branch.as_str())
            .and_then(|t| t.original_request.clone());
        if let Some(original) = original {
            let terminated = generate_response(
                &original,
                487,
                "Request Terminated",
                &GenerateResponseOpts {
                    to_tag: uas_to_tag,
                    ..Default::default()
                },
            );
            let terminated_buf = terminated.raw;
            self.send_buffer(endpoint, &terminated_buf, src).await;
            if let Some(txn) = self.txns.get_mut(branch.as_str()) {
                txn.state = TxnState::Completed;
                txn.last_response = Some(terminated_buf);
                txn.last_response_status = Some(487);
                txn.original_request = None;
            }
            // Timer-H-487 cleanup if the ACK for 487 never arrives.
            let key = self.timers.insert(Timer::Cleanup(branch.to_string()), ms(TIMER_H));
            if let Some(txn) = self.txns.get_mut(branch.as_str()) {
                txn.cleanup_key = Some(key);
            }
        }

        // Critical: we already answered 200 + 487 on the wire; a dropped Cancelled
        // would leave the b-leg ringing a cancelled call (no other signal upstream).
        self.emit_critical(TransactionEvent::Cancelled {
            call_id: call_id.as_str().to_string(),
            from_tag: from_tag.to_string(),
            invite_cseq,
            in_dialog,
        });
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
