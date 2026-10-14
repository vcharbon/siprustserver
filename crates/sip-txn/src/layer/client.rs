//! RFC 3261 §17.1 client (UAC) transactions: creation on `send_request`
//! (CANCEL/ACK build no txn — they reuse their INVITE's branch; a CANCEL for a
//! response-less INVITE txn is held until the first provisional, §9.1, and
//! once on the wire retransmits on its own Timer-E ladder as a sub-state of the INVITE
//! txn, §17.1.2.2; an ACK goes raw and rides no timer, §13.2.2.4),
//! Timer A/E retransmission, Timer B/F timeout, inbound-response matching
//! (including the non-2xx auto-ACK + Timer D hold); the orphaning of a
//! released call's transactions is `layer::orphan`'s (ADR-0034). The
//! server (UAS) side does NOT live here — see `layer::server`; a client INVITE
//! rebuilt from a record is `layer::seed`'s.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::generators::generate_ack_for_non_2xx;
use sip_message::header::ParamValue;
use sip_message::param_codec::decode_param;
use sip_message::{serialize, Method, SipMessage, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::event::{ClientTransactionHandle, TimeoutKind, TransactionEvent, TxnKind};
use crate::metrics::method_slot;
use crate::timers::{ms, TIMER_B, TIMER_F};
use sip_retransmit::{Class, Ladder, Schedule};

use super::lifetime::Hold;
use super::owner::Owner;
use super::txn::{CancelWire, HeldCancel, NewTransaction, Timer, TxnId, TxnRef, TxnState};

impl Owner {
    pub(super) async fn do_send_request(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        msg: SipRequest,
        dest: SocketAddr,
        txn_type: TxnKind,
    ) -> ClientTransactionHandle {
        // Wrap by value to serialize (avoids a full request clone just to make a
        // `&SipMessage`), then destructure `msg` back out for the rest.
        let wrapped = SipMessage::Request(msg);
        let buf = Bytes::from(serialize(&wrapped));
        let SipMessage::Request(msg) = wrapped else { unreachable!("just wrapped a request") };

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
            // Accepted) or is gone (a 2xx deletes it) is suppressed outright: it
            // has no effect on an answered request (§9.1/§9.2).
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
            self.timers.insert(
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
        txn.cancel_retransmit_key =
            Some(self.timers.insert(Timer::CancelRetransmit(branch.to_string()), first));
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
            let key = self.timers.insert(Timer::CancelRetransmit(branch.to_string()), interval);
            if let Some(txn) = self.clients.get_mut(branch) {
                txn.cancel_retransmit_key = Some(key);
            }
        }
    }

    fn start_client_retransmit(
        &mut self,
        branch: &str,
        buf: Bytes,
        dest: SocketAddr,
        max_ms: u64,
        timeout_kind: TimeoutKind,
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
        let r_key = self.timers.insert(Timer::ClientRetransmit(branch.to_string()), first);
        let t_key = self.timers.insert(Timer::ClientTimeout(branch.to_string()), ms(max_ms));
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
            let key =
                self.timers.insert(Timer::ClientRetransmit(branch.to_string()), next_interval);
            if let Some(txn) = self.clients.get_mut(branch) {
                txn.retransmit_key = Some(key);
            }
        }
    }

    pub(super) fn fire_timeout(&mut self, branch: &str) {
        let Some(event) = self.timeout_event(branch) else { return };
        let reported = self.clients.get(branch).is_some_and(|t| t.dead_hop_reported);
        self.give_up(branch);
        if !reported {
            // Critical: the give-up fires once, so nothing re-fires — a dropped
            // Timeout would strand the leg until the 1 h GlobalDuration.
            self.emit_critical(event);
        }
    }

    /// The TU's first-response deadline on an initial INVITE (ADR-0044): a
    /// branch still unanswered is reported dead to the TU, once, and the
    /// transaction keeps Timer A to Timer B, where it gives up unreported.
    pub(super) fn fire_first_response(&mut self, branch: &str) {
        let unanswered = self.clients.get(branch).is_some_and(|t| {
            t.kind == TxnKind::Invite && t.state == TxnState::Trying && !t.dead_hop_reported
        });
        if !unanswered {
            return;
        }
        let Some(event) = self.timeout_event(branch) else { return };
        if let Some(t) = self.clients.get_mut(branch) {
            t.dead_hop_reported = true;
        }
        // Critical: the deadline fires once, like the give-up it reports.
        self.emit_critical(event);
    }

    /// The `Timeout` the active client txn at `branch` reports, `None` once it
    /// took a final or gave up.
    fn timeout_event(&self, branch: &str) -> Option<TransactionEvent> {
        let t =
            self.clients.get(branch).filter(|t| t.state.is_active() && !t.lifetime.gave_up())?;
        Some(TransactionEvent::Timeout {
            branch: branch.to_string(),
            call_ref: t.call_ref.clone(),
            leg_id: t.leg_id.clone(),
            incarnation_mark: t.incarnation_mark.clone(),
            // Every transaction names its method (RFC 3261 §17.1: the TU is
            // told which request drew no answer) — a BYE's Timer F is not an
            // OPTIONS's.
            method: Some(t.method.to_string()),
            destination: t.destination,
            kind: t.timeout_kind,
        })
    }

    /// End the client txn at `branch` on its give-up. A non-INVITE is deleted.
    /// An INVITE stops retransmitting and is held [`Hold::GaveUp`], 64·T1. In
    /// Proceeding that is RFC 3261 §9.1's wait: the TU abandons it with a
    /// CANCEL, which still matches it (and restarts the hold), and the final
    /// that CANCEL provokes is ACKed (§17.1.1.3) and handed up, Timer D
    /// replacing the hold. In Calling it is a robustness hold:
    /// its CANCEL stays held unless a late provisional arrives, and a late
    /// final is ACKed.
    fn give_up(&mut self, branch: &str) {
        let retransmit = match self.clients.get_mut(branch) {
            Some(t) if t.kind == TxnKind::Invite => {
                t.ladder = None;
                t.retransmit_key.take()
            }
            _ => {
                self.delete_txn(TxnRef::Client(branch));
                return;
            }
        };
        self.cancel_timer(retransmit);
        self.hold(TxnRef::Client(branch), Hold::GaveUp);
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
                    self.emit(TransactionEvent::Message {
                        message: Box::new(SipMessage::Response(resp)),
                        src,
                        matched_client_txn: false,
                    });
                }
                return false;
            }

            // Snapshot what we need before mutating. The branch and the CSeq
            // method match (RFC 3261 §17.1.3): the branch is unguessable, and
            // the source goes unchecked (ADR-0007).
            let client_match =
                self.clients.get(branch).filter(|t| t.method == *resp.cseq().method()).map(|t| {
                    (t.kind, t.state, t.original_request.clone(), t.destination, t.orphaned)
                });
            matched_client_txn = client_match.is_some();

            if let Some((kind, state, original_request, destination, orphaned)) = client_match {
                if state == TxnState::Accepted {
                    // RFC 6026 §7.2 + RFC 3261 §13.2.2.4: a repeat of the 2xx
                    // re-draws the ACK already sent; anything else on the
                    // branch is absorbed until Timer M.
                    if (200..300).contains(&resp.status()) {
                        self.on_accepted_2xx(endpoint, branch, &resp).await;
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
                        self.rearm_invite_bound(branch);
                        self.flush_cancel_on_provisional(endpoint, branch).await;
                    }
                } else if kind == TxnKind::Invite && resp.status() >= 300 {
                    // Non-2xx INVITE final: (re-)ACK hop-by-hop (RFC 3261 §17.1.1.2).
                    if let (Some(orig), Some(dest)) = (original_request, destination) {
                        let ack = generate_ack_for_non_2xx(&orig, &resp, &[]);
                        // The recipe froze the ACK into its own image, which IS
                        // its wire form — send that instead of rendering it twice.
                        self.send_buffer(endpoint, ack.image(), dest).await;
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
                    self.emit_critical(TransactionEvent::Message {
                        message: Box::new(SipMessage::Response(resp)),
                        src,
                        matched_client_txn: true,
                    });
                    return true;
                } else if kind == TxnKind::Invite && orphaned {
                    // A 2xx to an orphaned INVITE: the UAC core that owed its ACK
                    // (§13.2.2.4) is gone, so the layer sends the bare ACK on a
                    // fresh branch itself and holds the transaction in Accepted
                    // for Timer M, then BYEs the dialog a 2xx to an initial
                    // INVITE created.
                    self.accept_orphaned_2xx(endpoint, branch, &resp).await;
                    return true;
                } else {
                    // 2xx INVITE (the TU ACKs end-to-end) or any non-INVITE final:
                    // terminate the client txn; an orphan's final has no consumer.
                    self.delete_txn(TxnRef::Client(branch));
                    if orphaned {
                        return true;
                    }
                    self.emit_critical(TransactionEvent::Message {
                        message: Box::new(SipMessage::Response(resp)),
                        src,
                        matched_client_txn: true,
                    });
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
            self.emit(TransactionEvent::Message {
                message: Box::new(SipMessage::Response(resp)),
                src,
                matched_client_txn,
            });
        }
        matched_client_txn
    }
}

impl Owner {
    /// RFC 3261 §17.1 client-transaction timeout plus the [`TimeoutKind`] its
    /// expiry emits. EVERY client txn is armed with the §17.1 failure-detection
    /// timer — §17.1.1.2 scopes Timer B to Calling, so an INVITE drawing no
    /// response of any kind is an unanswered hop, not a responding peer, and
    /// gives up at 64·T1; the TU's first-response deadline rides beside it
    /// ([`Owner::fire_first_response`], ADR-0044). The window `invite_initial_timeout_ms` owns opens at an INVITE's first
    /// provisional, where [`Owner::rearm_invite_bound`] arms it — initial and
    /// in-dialog alike, since a peer that answered 1xx has left Calling; a
    /// non-INVITE never leaves Timer F.
    fn client_timeout(&self, kind: TxnKind, req: &SipRequest) -> (u64, TimeoutKind) {
        match kind {
            // `min`: whichever bound is configured tighter is the deadline
            // and owns the give-up.
            TxnKind::Invite if req.to().tag().is_none() => {
                (TIMER_B.min(self.invite_initial_timeout_ms), TimeoutKind::Response)
            }
            TxnKind::Invite => (TIMER_B, TimeoutKind::Response),
            TxnKind::NonInvite => (TIMER_F, TimeoutKind::Response),
        }
    }

    /// Re-arm an INVITE client txn's give-up from Timer B to the configured
    /// INVITE bound at its FIRST provisional: the txn has left Calling, the
    /// only state Timer B governs (RFC 3261 §17.1.1.2), and a peer that has
    /// answered is not a dead hop — whether the INVITE opens a dialog or
    /// renegotiates one. The bound is the pre-final backstop of every INVITE
    /// in Proceeding, so a renegotiation whose peer rings or goes silent after
    /// its provisional ends on it, not on Timer B.
    ///
    /// The bound is measured from the ORIGINAL SEND, so its configured ordering
    /// against the app's setup/no-answer deadline holds however late the
    /// provisional arrives. No-op for a non-INVITE, a txn already re-armed —
    /// a second provisional never extends the bound — a txn that gave up, and
    /// one whose hop was already reported dead: it keeps Timer B.
    fn rearm_invite_bound(&mut self, branch: &str) {
        let bound = self.invite_initial_timeout_ms;
        let remaining = match self.clients.get(branch) {
            Some(t)
                if t.kind == TxnKind::Invite
                    && t.timeout_kind == TimeoutKind::Response
                    && !t.lifetime.gave_up()
                    && !t.dead_hop_reported =>
            {
                bound.saturating_sub(t.created_at.elapsed().as_millis() as u64).max(1)
            }
            _ => return,
        };
        let old = self.clients.get_mut(branch).and_then(|t| t.timeout_key.take());
        self.cancel_timer(old);
        let key = self.timers.insert(Timer::ClientTimeout(branch.to_string()), ms(remaining));
        if let Some(txn) = self.clients.get_mut(branch) {
            txn.timeout_key = Some(key);
            txn.timeout_kind = TimeoutKind::Transaction;
        }
    }
}

/// The call attribution a request's top Via carries.
pub(super) struct ViaMarks {
    /// `cr`: the callRef.
    pub(super) call_ref: Option<String>,
    /// `lg`: the leg id.
    pub(super) leg_id: Option<String>,
    /// `ci`: the mark of the call incarnation that sent it.
    pub(super) incarnation_mark: Option<String>,
}

/// The top Via's `cr` (callRef) / `lg` (legId) / `ci` (incarnation mark)
/// custom params, URL-decoded. The B2BUA's `build_call_via` URL-encodes them
/// (callRefs contain `|`/`@`) and a param value stays as written on the wire,
/// so decoding here is what makes `cancel_txns_for_call` match the natural
/// callRef the caller passes (see the cr/lg round-trip test).
pub(super) fn extract_via_custom_params(req: &SipRequest) -> ViaMarks {
    let top_via = req.top_via();
    let read = |name: &str| top_via.param(name).and_then(ParamValue::as_str).map(decode_param);
    ViaMarks { call_ref: read("cr"), leg_id: read("lg"), incarnation_mark: read("ci") }
}
