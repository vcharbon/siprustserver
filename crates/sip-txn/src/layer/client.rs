//! RFC 3261 §17.1 client (UAC) transactions: creation on `send_request`
//! (CANCEL/ACK build no txn — they reuse their INVITE's branch; a CANCEL for a
//! response-less INVITE txn is held until the first provisional, §9.1, and
//! once on the wire retransmits on its own Timer-E ladder as a sub-state of the INVITE
//! txn, §17.1.2.2; an ACK goes raw and rides no timer, §13.2.2.4),
//! Timer A/E retransmission, inbound-response matching (including the
//! non-2xx auto-ACK + Timer D hold); the give-up on Timer B/F or the INVITE
//! bound is `layer::client_give_up`'s, the RFC 6026 Accepted
//! hold after a 2xx is `layer::accepted`'s, the orphaning of a released
//! call's transactions `layer::orphan`'s (ADR-0034). The
//! server (UAS) side does NOT live here — see `layer::server`; a client INVITE
//! rebuilt from a record is `layer::seed`'s.

use std::net::SocketAddr;

use bytes::Bytes;
use call::{TxnKind, TxnTimeoutKind};
use sip_clock::delay_queue;
use sip_message::generators::generate_ack_for_non_2xx;
use sip_message::serializer::{render, write_request_line};
use sip_message::{Method, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::event::{ClientTransactionHandle, TransactionEvent};
use crate::metrics::method_slot;
use crate::timers::{ms, TIMER_B};
use sip_retransmit::{Class, Ladder, Schedule};

use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{CancelWire, HeldCancel, NewTransaction, Timer, TxnId, TxnRef, TxnState};
use super::via_marks::{extract_via_custom_params, ViaMarks};

impl Owner {
    pub(super) async fn do_send_request(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipRequest,
        dest: SocketAddr,
        txn_type: TxnKind,
    ) -> ClientTransactionHandle {
        let buf =
            Bytes::from(render(msg.headers(), msg.body(), |out| write_request_line(out, &msg)));

        // CANCEL and ACK deliberately REUSE the branch of the request they relate
        // to (RFC 3261 §9.1 / §13.2.2.4). The client map is keyed by branch, so
        // creating a client transaction for them here would DISPLACE the live INVITE
        // client txn at that shared branch — and the txn could never complete anyway
        // (CANCEL responses are passed through, an ACK elicits none). Send them raw
        // without creating a map entry. This closes the branch-collision foot-gun at
        // its source, so the branch-only key never has to disambiguate by method.
        if msg.method() == Method::Cancel || msg.method() == Method::Ack {
            // RFC 3261 §9.1 (ADR-0043 X1): a CANCEL for an INVITE client txn that
            // has received NO response is held on the txn and flushed on the
            // first provisional (`handle_inbound_response`) — before one, a UAS
            // that has not built the server txn answers it 481 while Timer A
            // keeps ringing. A branch that never draws a provisional never sees
            // it: the INVITE ends at Timer B. A final taking the txn first clears
            // it unsent (§9.2). A CANCEL whose txn took its final (Completed,
            // Accepted) or is gone is suppressed outright: it has no effect on
            // an answered request (§9.1/§9.2).
            #[derive(PartialEq)]
            enum CancelGate {
                Hold,
                Suppress,
                Send,
            }
            let gate = if msg.method() == Method::Cancel {
                match msg.top_via().branch().and_then(|b| self.clients.get(b)) {
                    Some(t) if t.kind == TxnKind::Invite && t.state.is_active() => match t.state {
                        TxnState::Trying => CancelGate::Hold,
                        _ => CancelGate::Send,
                    },
                    _ => CancelGate::Suppress,
                }
            } else {
                CancelGate::Send
            };
            match gate {
                CancelGate::Hold => {
                    let branch_key = msg.top_via().branch().unwrap_or_default();
                    if let Some(txn) = self.clients.get_mut(branch_key) {
                        // A newer CANCEL supersedes a still-held one — count the displaced
                        // (never-sent) datagram so held == flushed + dropped.
                        if txn
                            .held_cancel
                            .replace(HeldCancel::new(buf, dest, CancelWire::Held))
                            .is_some()
                        {
                            self.metrics
                                .held_cancels_dropped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    self.metrics.cancels_held.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    self.release_seed_resend(branch_key);
                }
                CancelGate::Suppress => {
                    self.metrics
                        .cancels_suppressed_on_final
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                CancelGate::Send => {
                    // A CANCEL reaching the wire on a live client INVITE txn is
                    // parked on it (superseding: the newest CANCEL is what the
                    // ladder replays) and
                    // gets the Timer-E ladder (§17.1.2.2). An ACK is always raw
                    // (§13.2.2.4 — no timer of its own).
                    let park = if msg.method() == Method::Cancel {
                        msg.top_via().branch().filter(|b| {
                            self.clients
                                .get(*b)
                                .is_some_and(|t| t.kind == TxnKind::Invite && t.state.is_active())
                        })
                    } else {
                        None
                    };
                    let arm = park.map(str::to_string);
                    let mut first_on_given_up = false;
                    if let Some(t) = park.and_then(|b| self.clients.get_mut(b)) {
                        first_on_given_up = t.lifetime.gave_up()
                            && t.held_cancel.as_ref().is_none_or(|h| h.wire == CancelWire::Held);
                        match t.held_cancel.as_mut() {
                            Some(h) => {
                                h.buf = buf.clone();
                                h.dest = dest;
                            }
                            None => {
                                t.held_cancel =
                                    Some(HeldCancel::new(buf.clone(), dest, CancelWire::Sent));
                            }
                        }
                    }
                    self.send_buffer(endpoint, &buf, dest).await;
                    if msg.method() == Method::Ack {
                        self.note_tu_ack(&msg, &buf, dest);
                    }
                    if let Some(branch) = arm {
                        self.arm_cancel_retransmit(&branch);
                        if first_on_given_up {
                            self.hold(TxnRef::Client(&branch), Hold::GaveUp);
                        }
                    }
                }
            }
            let branch = msg.top_via().branch().unwrap_or_default().to_string();
            return match txn_type {
                TxnKind::Invite => ClientTransactionHandle::Invite {
                    branch,
                    original_invite: msg,
                    destination: dest,
                },
                TxnKind::NonInvite => ClientTransactionHandle::NonInvite {
                    branch,
                    original_request: msg,
                    destination: dest,
                },
            };
        }

        let branch = msg
            .top_via()
            .branch()
            .filter(|b| !b.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| self.id_gen.new_branch());
        let ViaMarks { call_ref, leg_id, incarnation_mark } = extract_via_custom_params(&msg);
        self.note_dialog_cseq(call_ref.as_deref(), &msg);

        // Opened after the send, so `created_at` (the bound's origin) follows it.
        self.send_buffer(endpoint, &buf, dest).await;
        self.open_txn(NewTransaction {
            id: TxnId::Client(branch.clone()),
            kind: txn_type,
            method: msg.method().clone(),
            cseq: msg.cseq().seq(),
            call_id: msg.call_id().as_str().to_string(),
            from_tag: msg.from().tag().unwrap_or_default().to_string(),
            original_request: matches!(txn_type, TxnKind::Invite).then(|| msg.clone()),
            call_ref,
            leg_id,
            incarnation_mark,
            state: TxnState::Trying,
            destination: Some(dest),
        });
        let (max_ms, timeout_kind) = self.client_timeout(txn_type, &msg);
        self.start_client_retransmit(&branch, buf, dest, max_ms, timeout_kind);
        if txn_type == TxnKind::Invite
            && msg.to().tag().is_none()
            && self.invite_first_response_timeout_ms < max_ms
        {
            self.arm(
                Timer::FirstResponse(branch.clone()),
                ms(self.invite_first_response_timeout_ms),
            );
        }

        match txn_type {
            TxnKind::Invite => {
                ClientTransactionHandle::Invite { branch, original_invite: msg, destination: dest }
            }
            TxnKind::NonInvite => ClientTransactionHandle::NonInvite {
                branch,
                original_request: msg,
                destination: dest,
            },
        }
    }

    /// Put a held CANCEL on the wire — called when its INVITE client txn takes
    /// its first provisional (RFC 3261 §9.1 "wait for the arrival of a
    /// provisional response before sending") — and arm its Timer-E ladder. A
    /// send on an INVITE that gave up restarts its hold: §9.1's 64·T1 runs
    /// from the CANCEL.
    pub(super) async fn flush_cancel_on_provisional(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
    ) {
        let send = match self.clients.get_mut(branch) {
            Some(t) => match t.held_cancel.as_mut() {
                Some(h) if h.wire == CancelWire::Held => {
                    h.wire = CancelWire::Sent;
                    Some((h.buf.clone(), h.dest, t.lifetime.gave_up()))
                }
                _ => None,
            },
            None => None,
        };
        if let Some((buf, dest, on_given_up)) = send {
            if on_given_up {
                self.hold(TxnRef::Client(branch), Hold::GaveUp);
            }
            self.send_buffer(endpoint, &buf, dest).await;
            self.metrics.held_cancels_flushed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.arm_cancel_retransmit(branch);
        }
    }

    /// Arms the Timer-E ladder for the on-wire CANCEL parked on `branch`,
    /// resetting the pacing to T1. No-op while a ladder is already running —
    /// a re-send never resets the pacing. Callers: the first-send sites
    /// (direct pass-through, first-provisional flush) plus the pass-through of
    /// a superseding CANCEL (a fresh TU datagram earns a fresh ladder even
    /// after a ceiling).
    fn arm_cancel_retransmit(&mut self, branch: &str) {
        let Some(txn) = self.clients.get_mut(branch) else { return };
        if txn.cancel_retransmit_key.is_some() {
            return;
        }
        let Some(h) = txn.held_cancel.as_mut() else { return };
        let Some((ladder, first)) = Ladder::armed(Schedule::rfc(Class::CancelClient)) else {
            return;
        };
        h.ladder = Some(ladder);
        let timer = Timer::CancelRetransmit(branch.to_string());
        txn.cancel_retransmit_key = Some(delay_queue::insert(&mut self.timers, timer, first));
    }

    /// Timer-E tick for the on-wire CANCEL (RFC 3261 §17.1.2.2: T1, doubling,
    /// capped at T2): re-sends the parked datagram until a CANCEL response
    /// arrives, the INVITE txn resolves, or the 64·T1 ceiling. The ceiling
    /// gives up on the CANCEL ONLY — the INVITE client txn continues under its
    /// own bound and still owes a final. The pacing is the Trying ladder for
    /// the ladder's whole life: the host txn's `Proceeding` belongs to the
    /// INVITE, and the CANCEL sub-state never has a Proceeding of its own — any
    /// response with a CANCEL CSeq (1xx included) ends the ladder outright.
    pub(super) async fn fire_cancel_retransmit(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
    ) {
        let send = match self.clients.get_mut(branch) {
            Some(t) if t.state.is_active() => match t.held_cancel.as_mut() {
                Some(h) if h.wire != CancelWire::Held => {
                    let rearm = h.ladder.as_mut().and_then(Ladder::advance);
                    Some((h.buf.clone(), h.dest, rearm))
                }
                _ => None,
            },
            _ => None,
        };
        let Some((buf, dest, rearm)) = send else { return };
        self.send_buffer(endpoint, &buf, dest).await;
        self.metrics.retransmits.record_request(Class::CancelClient, method_slot(&Method::Cancel));
        if let Some(interval) = rearm {
            let key = self.arm(Timer::CancelRetransmit(branch.to_string()), interval);
            if let Some(txn) = self.clients.get_mut(branch) {
                txn.cancel_retransmit_key = Some(key);
            }
        }
    }

    pub(super) fn start_client_retransmit(
        &mut self,
        branch: &str,
        buf: Bytes,
        dest: SocketAddr,
        max_ms: u64,
        timeout_kind: TxnTimeoutKind,
    ) {
        let Some(class) = self.clients.get(branch).map(|t| match t.kind {
            TxnKind::Invite => Class::InviteClient,
            TxnKind::NonInvite => Class::NonInviteClient,
        }) else {
            return;
        };
        // Retransmission stops at the give-up, and at Timer B (RFC 3261
        // §17.1.1.2) even when the give-up timer runs longer: a raised
        // initial-INVITE bound extends the wait for a response, never the
        // retransmit storm.
        let schedule = Schedule::rfc(class).with_give_up(ms(max_ms.min(TIMER_B)));
        let Some((ladder, first)) = Ladder::armed(schedule) else { return };
        let r_key = self.arm(Timer::ClientRetransmit(branch.to_string()), first);
        let t_key = self.arm(Timer::ClientTimeout(branch.to_string()), ms(max_ms));
        if let Some(txn) = self.clients.get_mut(branch) {
            txn.retransmit_key = Some(r_key);
            txn.timeout_key = Some(t_key);
            txn.retransmit_buf = Some(buf);
            txn.ladder = Some(ladder);
            txn.timeout_kind = timeout_kind;
            txn.destination = Some(dest);
        }
    }

    pub(super) async fn fire_retransmit(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let (buf, dest, proceeding, paced_by) = match self.clients.get(branch) {
            Some(t) if t.state.is_active() => match (&t.retransmit_buf, t.destination) {
                (Some(buf), Some(dest)) => (
                    buf.clone(),
                    dest,
                    t.kind == TxnKind::NonInvite && t.state == TxnState::Proceeding,
                    t.ladder
                        .as_ref()
                        .and_then(Ladder::class)
                        .map(|class| (class, method_slot(&t.method))),
                ),
                _ => return,
            },
            _ => return, // completed/terminated/gone — stop retransmitting
        };

        self.send_buffer(endpoint, &buf, dest).await;
        // Counted under the class that paced THIS rung: the first fire to land
        // in Proceeding was armed by the Trying ladder, and only the next is
        // the flat-T2 class's.
        if let Some((class, slot)) = paced_by {
            self.metrics.retransmits.record_request(class, slot);
        }

        // A non-INVITE that has reached Proceeding re-arms at exactly T2 from
        // here on (§17.1.2.2 — the provisional itself never touches the pending
        // timer, so the new pace takes effect on the first fire after it).
        let rearm = match self.clients.get_mut(branch).and_then(|t| t.ladder.as_mut()) {
            Some(ladder) => {
                if proceeding {
                    ladder.retarget(Class::NonInviteProceeding);
                }
                ladder.advance()
            }
            None => return,
        };
        if let Some(next_interval) = rearm {
            let key = self.arm(Timer::ClientRetransmit(branch.to_string()), next_interval);
            if let Some(txn) = self.clients.get_mut(branch) {
                txn.retransmit_key = Some(key);
            }
        }
    }

    /// Move the client INVITE txn at `branch` to Completed on its first
    /// non-2xx final: every timer it was running stops, a CANCEL still parked
    /// on it is moot (the UAS answered — §9.2; counted dropped only if it
    /// never reached the wire), and Timer D holds it to re-ACK repeats (RFC
    /// 3261 §17.1.1.2), from this final.
    fn complete_non_2xx(&mut self, branch: &str) {
        let (r, t, cr) = match self.clients.get_mut(branch) {
            Some(txn) => {
                txn.state = TxnState::Completed;
                if txn.held_cancel.take().is_some_and(|h| h.wire == CancelWire::Held) {
                    self.metrics
                        .held_cancels_dropped
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                (
                    txn.retransmit_key.take(),
                    txn.timeout_key.take(),
                    txn.cancel_retransmit_key.take(),
                )
            }
            None => (None, None, None),
        };
        self.cancel_timer(r);
        self.cancel_timer(t);
        self.cancel_timer(cr);
        self.hold(TxnRef::Client(branch), Hold::TimerD);
    }

    pub(super) async fn handle_inbound_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        resp: SipResponse,
        src: SocketAddr,
    ) {
        self.inbound_response(endpoint, resp, src, false).await;
    }

    /// The inbound-response FSM step. Returns whether a client transaction
    /// took the response. `reoffer` is the consumer handing back a datagram it
    /// already holds (`TransactionLayer::reoffer`): the matched paths run and
    /// emit exactly as on first arrival, while a miss emits nothing — the
    /// consumer keeps the copy it has.
    pub(super) async fn inbound_response(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        resp: SipResponse,
        src: SocketAddr,
        reoffer: bool,
    ) -> bool {
        let top_via = resp.top_via();
        let branch = top_via.branch().unwrap_or_default();

        // 100 Trying: absorb after nudging the matching client txn's state. An
        // INVITE client stops retransmitting on a provisional (§17.1.1.2); a
        // non-INVITE client KEEPS retransmitting at T2 in Proceeding (§17.1.2.2),
        // so leave its timer running.
        if resp.status() == 100 {
            let mut matched = false;
            if !branch.is_empty() {
                // Like the 1xx>100 path: a late 100 must not downgrade a txn
                // that already took its final (Completed holds for Timer D,
                // Accepted for Timer M).
                let (key, flush) = match self.clients.get_mut(branch) {
                    Some(txn) if txn.method == *resp.cseq().method() && txn.state.is_active() => {
                        txn.state = TxnState::Proceeding;
                        (
                            (txn.kind == TxnKind::Invite)
                                .then(|| txn.retransmit_key.take())
                                .flatten(),
                            true,
                        )
                    }
                    _ => (None, false),
                };
                self.cancel_timer(key);
                if flush {
                    matched = true;
                    self.rearm_invite_bound(branch);
                    self.flush_cancel_on_provisional(endpoint, branch).await;
                }
            }
            return matched;
        }

        let mut matched_client_txn = false;
        if !branch.is_empty() {
            // CANCEL responses reuse the INVITE branch — never match them to the
            // INVITE client txn (would tear it down on the 200 and miss the 487).
            // They DO end the CANCEL sub-state: the ladder stops and the parked
            // datagram is dropped (the UAS matched the CANCEL — nothing further
            // is owed). A never-sent held CANCEL
            // cannot have drawn a response, so only on-wire state clears here.
            if resp.cseq().method() == Method::Cancel {
                let key = match self.clients.get_mut(branch) {
                    Some(t)
                        if t.kind == TxnKind::Invite
                            && t.held_cancel
                                .as_ref()
                                .is_some_and(|h| h.wire != CancelWire::Held) =>
                    {
                        t.held_cancel = None;
                        t.cancel_retransmit_key.take()
                    }
                    _ => None,
                };
                self.cancel_timer(key);
                if !reoffer {
                    self.emit(TransactionEvent::response(resp, src, false));
                }
                return false;
            }

            // Snapshot what we need before mutating. The branch and the CSeq
            // method match (RFC 3261 §17.1.3): the branch is unguessable, and
            // the source goes unchecked (ADR-0007). A 2xx on an Accepted txn its
            // call holds goes to the TU, which owns each ACK, matched to none.
            let is_2xx = (200..300).contains(&resp.status());
            let client_match = self
                .clients
                .get(branch)
                .filter(|t| t.method == *resp.cseq().method())
                .filter(|t| t.state != TxnState::Accepted || t.orphaned || !is_2xx)
                .map(|t| (t.kind, t.state, t.original_request.clone(), t.destination, t.orphaned));
            matched_client_txn = client_match.is_some();

            if let Some((kind, state, original_request, destination, orphaned)) = client_match {
                if state == TxnState::Accepted {
                    // RFC 6026 §7.2 + RFC 3261 §13.2.2.4: the orphan's 2xx
                    // draws its dialog's ACK from the layer; a 1xx or a non-2xx
                    // is absorbed until Timer M (§8.4: only 2xx go up).
                    if is_2xx {
                        self.release_2xx(endpoint, branch, &resp).await;
                    }
                    return true;
                }
                if resp.status() < 200 {
                    // Provisional 1xx>100 — Proceeding. Ignore once Completed or
                    // Accepted (a late provisional must not downgrade a txn that
                    // already took its final). INVITE stops retransmitting
                    // (§17.1.1.2); non-INVITE continues at T2 (§17.1.2.2), so only
                    // cancel retransmit for INVITE.
                    if state.is_active() {
                        let key = match self.clients.get_mut(branch) {
                            Some(txn) => {
                                txn.state = TxnState::Proceeding;
                                (kind == TxnKind::Invite)
                                    .then(|| txn.retransmit_key.take())
                                    .flatten()
                            }
                            None => None,
                        };
                        self.cancel_timer(key);
                        if kind == TxnKind::Invite {
                            self.note_reliable_offer(branch, &resp);
                        }
                        self.rearm_invite_bound(branch);
                        self.flush_cancel_on_provisional(endpoint, branch).await;
                    }
                } else if kind == TxnKind::Invite && resp.status() >= 300 {
                    // Non-2xx INVITE final: (re-)ACK hop-by-hop (RFC 3261 §17.1.1.2).
                    if let (Some(orig), Some(dest)) = (original_request, destination) {
                        match generate_ack_for_non_2xx(&orig, &resp, &[]) {
                            Ok(ack) => self.send_buffer(endpoint, ack.image(), dest).await,
                            Err(e) => tracing::warn!(%branch, %e, "ACK not sent: no ACK echoes it"),
                        }
                    }
                    if state == TxnState::Completed {
                        // A RETRANSMITTED non-2xx final (our first ACK was lost): we
                        // just re-ACKed it above; absorb without re-notifying.
                        return true;
                    }
                    if orphaned {
                        // No consumer holds this transaction's call: the hop
                        // ACK above and the Timer D hold below are the whole
                        // of what the final draws.
                        self.complete_non_2xx(branch);
                        return true;
                    }
                    // FIRST non-2xx final: hold the txn in Completed for Timer D so
                    // retransmitted finals are re-ACKed + absorbed, not re-surfaced.
                    // The auto-ACK silenced the UAS's retransmission *trigger*, so
                    // without Timer D a lost ACK would have the UAS resend the final
                    // unanswered until its own Timer H, each resend re-emitting
                    // upstream as a duplicate.
                    self.complete_non_2xx(branch);
                    // Critical: we auto-ACKed (silenced the UAS's resend), so this is
                    // the app's only delivery of the final.
                    self.emit_critical(TransactionEvent::response(resp, src, true));
                    return true;
                } else if kind == TxnKind::Invite {
                    // A 2xx INVITE final: Accepted until Timer M (RFC 6026
                    // §7.2). The TU ACKs it end to end; to an orphaned INVITE,
                    // whose UAC core is gone (§13.2.2.4), the layer does.
                    if orphaned {
                        self.accept_orphaned_2xx(endpoint, branch, &resp).await;
                        return true;
                    }
                    self.accept_2xx(branch, &resp);
                    // Critical: the transaction's only delivery of its final.
                    self.emit_critical(TransactionEvent::response(resp, src, true));
                    return true;
                } else {
                    // Any non-INVITE final: terminate the client txn; an
                    // orphan's final has no consumer.
                    self.delete_txn(TxnRef::Client(branch));
                    if orphaned {
                        return true;
                    }
                    self.emit_critical(TransactionEvent::response(resp, src, true));
                    return true;
                }
            }
            // (a response matching no client txn — pass through)
        }

        // Provisionals and unmatched responses are protocol-redelivered → lossy.
        // A re-offered miss stays with the consumer that holds it; an orphaned
        // transaction's provisional has no consumer at all.
        let orphaned = self.clients.get(branch).is_some_and(|t| t.orphaned);
        if (matched_client_txn && !orphaned) || !reoffer {
            self.emit(TransactionEvent::response(resp, src, matched_client_txn));
        }
        matched_client_txn
    }
}
