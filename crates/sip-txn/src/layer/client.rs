//! RFC 3261 §17.1 client (UAC) transactions: creation on `send_request`
//! (CANCEL/ACK build no txn — they reuse their INVITE's branch; a CANCEL for a
//! response-less INVITE txn is held until the first provisional or the grace
//! expiry, whichever comes first — §9.1 bounded per ADR-0028 — and once on the
//! wire retransmits on its own Timer-E ladder as a sub-state of the INVITE
//! txn, §17.1.2.2; an ACK goes raw and rides no timer, §13.2.2.4),
//! Timer A/E retransmission, Timer B/F timeout, inbound-response matching
//! (including the non-2xx auto-ACK + Timer D hold), and per-call eviction. The
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
use crate::timers::{ms, TIMER_B, TIMER_D, TIMER_F};
use sip_retransmit::{Class, Ladder, Schedule};

use super::owner::Owner;
use super::txn::{CancelWire, HeldCancel, NewTransaction, Timer, Transaction, TxnRole, TxnState};

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
        // to (RFC 3261 §9.1 / §13.2.2.4). The txns map is keyed by branch, so
        // creating a client transaction for them here would DISPLACE the live INVITE
        // client txn at that shared branch — and the txn could never complete anyway
        // (CANCEL responses are passed through, an ACK elicits none). Send them raw
        // without creating a map entry. This closes the branch-collision foot-gun at
        // its source, so the branch-only key never has to disambiguate by method.
        if msg.method() == Method::Cancel || msg.method() == Method::Ack {
            // RFC 3261 §9.1: a CANCEL for an INVITE client txn that has received
            // NO response is held on the txn and flushed on the first provisional
            // (`handle_inbound_response`). A CANCEL sent before any response is
            // unmatchable at a UAS that has not built the server txn yet — it
            // 481s while Timer A keeps re-ringing the callee for a call that no
            // longer exists. The hold is BOUNDED (ADR-0028): a grace timer sends
            // the CANCEL regardless when the branch stays response-less, so a
            // callee that never answers anything still hears the cancellation —
            // the wait is a courtesy, never a veto. The held CANCEL is cleared
            // unsent only when the txn takes a final first (§9.2 — the UAS
            // already answered; cancellation is moot).
            // A CANCEL whose txn already took its final (Completed, Timer-D hold)
            // is suppressed outright: §9.1/§9.2 — a CANCEL has no effect on a
            // request the UAS already answered, and sending it puts a pre-1xx
            // CANCEL on the wire when the final raced the caller's decision.
            // A CANCEL matching NO txn is still sent raw: an absent txn is not
            // proof the INVITE ended — a takeover-restored call CANCELs a b-leg
            // whose INVITE client txn lived on the failed peer, and dropping it
            // would orphan a protected ringing callee (ADR-0014).
            #[derive(PartialEq)]
            enum CancelGate {
                Hold,
                Suppress,
                Send,
            }
            let gate = if msg.method() == Method::Cancel {
                match msg.top_via().branch().and_then(|b| self.txns.get(b)) {
                    Some(t) if t.role == TxnRole::Client && t.kind == TxnKind::Invite => {
                        match t.state {
                            // A pre-1xx copy already on the wire (grace expired)
                            // makes a NEWER CANCEL a plain re-send, not a hold.
                            TxnState::Trying
                                if t.held_cancel
                                    .as_ref()
                                    .map_or(true, |h| h.wire == CancelWire::Held) =>
                            {
                                CancelGate::Hold
                            }
                            TxnState::Trying => CancelGate::Send,
                            TxnState::Completed => CancelGate::Suppress,
                            _ => CancelGate::Send,
                        }
                    }
                    _ => CancelGate::Send,
                }
            } else {
                CancelGate::Send
            };
            match gate {
                CancelGate::Hold => {
                    let branch_key = msg.top_via().branch().unwrap_or_default().to_string();
                    let grace_ms = self.cancel_hold_grace_ms;
                    if let Some(txn) = self.txns.get_mut(branch_key.as_str()) {
                        // A newer CANCEL supersedes a still-held one — count the
                        // displaced (never-sent) datagram so held counters
                        // reconcile (held == flushed + flushed_pre1xx + dropped).
                        if txn
                            .held_cancel
                            .replace(HeldCancel::new(buf, dest, CancelWire::Held))
                            .is_some()
                        {
                            self.metrics
                                .held_cancels_dropped
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        // Bound the hold (ADR-0028 bounded policy): if the
                        // first provisional never comes, the grace expiry
                        // sends the CANCEL anyway. Strict §9.1 policy
                        // (`cancel_hold_grace_ms: None`) arms nothing — the
                        // hold lasts until a provisional or txn death.
                        if let Some(grace) = grace_ms {
                            if txn.cancel_grace_key.is_none() {
                                txn.cancel_grace_key = Some(
                                    self.timers
                                        .insert(Timer::CancelGrace(branch_key.clone()), ms(grace)),
                                );
                            }
                        }
                    }
                    self.metrics.cancels_held.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                CancelGate::Suppress => {
                    self.metrics
                        .cancels_suppressed_on_final
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                CancelGate::Send => {
                    // A CANCEL reaching the wire on a live client INVITE txn is
                    // parked on it (superseding: the newest CANCEL is what the
                    // ladder — and the one owed pre-1xx re-send — replays) and
                    // gets the Timer-E ladder (§17.1.2.2). A CANCEL matching no
                    // live txn stays a raw single send: there is nothing to hang
                    // a ladder on without re-keying the branch map. An ACK is
                    // always raw (§13.2.2.4 — no timer of its own).
                    let park = if msg.method() == Method::Cancel {
                        msg.top_via().branch().filter(|b| {
                            self.txns.get(*b).is_some_and(|t| {
                                t.role == TxnRole::Client
                                    && t.kind == TxnKind::Invite
                                    && t.state.is_active()
                            })
                        })
                    } else {
                        None
                    };
                    let arm = park.map(str::to_string);
                    if let Some(t) = park.and_then(|b| self.txns.get_mut(b)) {
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
        let (call_ref, leg_id) = extract_via_custom_params(&msg);

        self.set_txn(Transaction::new(NewTransaction {
            branch: branch.clone(),
            role: TxnRole::Client,
            kind: txn_type,
            method: msg.method().clone(),
            call_id: msg.call_id().as_str().to_string(),
            from_tag: msg.from().tag().unwrap_or_default().to_string(),
            original_request: matches!(txn_type, TxnKind::Invite).then(|| msg.clone()),
            call_ref,
            leg_id,
            state: TxnState::Trying,
            destination: Some(dest),
        }));

        self.send_buffer(endpoint, &buf, dest).await;
        let (max_ms, timeout_kind) = self.client_timeout(txn_type, &msg);
        self.start_client_retransmit(&branch, buf, dest, max_ms, timeout_kind);

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
    /// provisional response before sending"); a first send arms the Timer-E
    /// ladder. A datagram the grace expiry already sent pre-1xx goes out ONCE
    /// more here — the UAS that 481'd the pre-1xx copy has built its server
    /// transaction by now, so this is the matchable send — counted as a
    /// re-flush, not a fresh flush, and it neither resets nor restarts the
    /// ladder (an exhausted ceiling is final).
    pub(super) async fn flush_cancel_on_provisional(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        branch: &str,
    ) {
        let send = match self.txns.get_mut(branch).and_then(|t| t.held_cancel.as_mut()) {
            Some(h) if h.wire != CancelWire::Sent => {
                let fresh = h.wire == CancelWire::Held;
                h.wire = CancelWire::Sent;
                Some((h.buf.clone(), h.dest, fresh))
            }
            _ => None,
        };
        if let Some((buf, dest, fresh)) = send {
            self.send_buffer(endpoint, &buf, dest).await;
            let counter = if fresh {
                &self.metrics.held_cancels_flushed
            } else {
                &self.metrics.held_cancels_reflushed
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if fresh {
                self.arm_cancel_retransmit(branch);
            }
        }
    }

    /// Arms the Timer-E ladder for the on-wire CANCEL parked on `branch`,
    /// resetting the pacing to T1. No-op while a ladder is already running —
    /// a re-send never resets the pacing. Callers: the three first-send sites
    /// (direct pass-through, grace expiry, first-provisional flush) plus the
    /// pass-through of a superseding CANCEL (a fresh TU datagram earns a fresh
    /// ladder even after a ceiling).
    fn arm_cancel_retransmit(&mut self, branch: &str) {
        let Some(txn) = self.txns.get_mut(branch) else { return };
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
        let send = match self.txns.get_mut(branch) {
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
            if let Some(txn) = self.txns.get_mut(branch) {
                txn.cancel_retransmit_key = Some(key);
            }
        }
    }

    /// Grace expiry for a held CANCEL (ADR-0028, [`Timer::CancelGrace`]): the
    /// branch is still response-less past the courtesy window, so the CANCEL
    /// goes on the wire NOW — a callee that never answers anything must still
    /// hear the cancellation, or an abandoned setup rings to the callee's own
    /// give-up while the caller is long gone. The datagram stays armed for one
    /// re-send on the first provisional (see [`flush_cancel_on_provisional`]),
    /// and the send arms the Timer-E ladder (§17.1.2.2).
    pub(super) async fn fire_cancel_grace(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let send = match self.txns.get_mut(branch) {
            Some(t) if t.state == TxnState::Trying => match t.held_cancel.as_mut() {
                Some(h) if h.wire == CancelWire::Held => {
                    h.wire = CancelWire::SentPre1xx;
                    Some((h.buf.clone(), h.dest))
                }
                _ => None,
            },
            // Proceeding/Completed already resolved the hold; a gone txn owes
            // nothing.
            _ => None,
        };
        if let Some((buf, dest)) = send {
            self.send_buffer(endpoint, &buf, dest).await;
            self.metrics
                .held_cancels_flushed_pre1xx
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.arm_cancel_retransmit(branch);
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
        let Some(class) = self.txns.get(branch).map(|t| match t.kind {
            TxnKind::Invite => Class::InviteClient,
            TxnKind::NonInvite => Class::NonInviteClient,
        }) else {
            return;
        };
        // Retransmission stops at the give-up, and at Timer B (RFC 3261
        // §17.1.1.2) even when the give-up timer runs longer: a tightened
        // first-response bound cuts the ladder with it, so no rung lands past
        // the give-up; a raised initial-INVITE bound extends the wait for a
        // response, never the retransmit storm.
        let schedule = Schedule::rfc(class).with_give_up(ms(max_ms.min(TIMER_B)));
        let Some((ladder, first)) = Ladder::armed(schedule) else { return };
        let r_key = self.timers.insert(Timer::ClientRetransmit(branch.to_string()), first);
        let t_key = self.timers.insert(Timer::ClientTimeout(branch.to_string()), ms(max_ms));
        if let Some(txn) = self.txns.get_mut(branch) {
            txn.retransmit_key = Some(r_key);
            txn.timeout_key = Some(t_key);
            txn.retransmit_buf = Some(buf);
            txn.ladder = Some(ladder);
            txn.timeout_kind = timeout_kind;
            txn.destination = Some(dest);
        }
    }

    pub(super) async fn fire_retransmit(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let (buf, dest, proceeding, paced_by) = match self.txns.get(branch) {
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
        let rearm = match self.txns.get_mut(branch).and_then(|t| t.ladder.as_mut()) {
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
            if let Some(txn) = self.txns.get_mut(branch) {
                txn.retransmit_key = Some(key);
            }
        }
    }

    pub(super) async fn fire_timeout(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let (call_ref, leg_id, method, destination, timeout_kind) = match self.txns.get(branch) {
            Some(t) if t.state.is_active() => {
                // Every transaction names its method (RFC 3261 §17.1: the TU
                // is told which request drew no answer), the non-INVITE ones
                // included — a BYE's Timer F is not an OPTIONS's.
                let method = Some(t.method.to_string());
                // The kind was stored explicitly at arming (`client_timeout` →
                // `start_client_retransmit`): `Transaction` for the long
                // out-of-dialog INVITE bound, `Response` for Timer B/F.
                (t.call_ref.clone(), t.leg_id.clone(), method, t.destination, t.timeout_kind)
            }
            _ => return,
        };
        // A never-sent held CANCEL dying with the timed-out txn goes on the
        // wire first under the bounded policy (ADR-0028 — same duty as the
        // evict flush): a grace window that a tight custom config lets Timer B
        // / the transaction bound outrun must not swallow the CANCEL.
        let flush = match self.txns.get_mut(branch).and_then(|t| t.held_cancel.as_mut()) {
            Some(h) if h.wire == CancelWire::Held && self.cancel_hold_grace_ms.is_some() => {
                h.wire = CancelWire::SentPre1xx;
                Some((h.buf.clone(), h.dest))
            }
            _ => None,
        };
        if let Some((buf, dest)) = flush {
            self.send_buffer(endpoint, &buf, dest).await;
            self.metrics
                .held_cancels_flushed_pre1xx
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.delete_txn(branch);
        // Critical: the txn is gone and Timer B/F cancelled, so nothing re-fires —
        // a dropped Timeout would strand the leg until the 1 h GlobalDuration.
        self.emit_critical(TransactionEvent::Timeout {
            branch: branch.to_string(),
            call_ref,
            leg_id,
            method,
            destination,
            kind: timeout_kind,
        });
    }

    /// Hold the Completed client INVITE txn at `branch` for Timer D (RFC 3261
    /// §17.1.1.2): its cleanup deletes it once retransmitted finals can no
    /// longer arrive.
    fn hold_for_timer_d(&mut self, branch: &str) {
        let key = self.timers.insert(Timer::Cleanup(branch.to_string()), ms(TIMER_D));
        if let Some(txn) = self.txns.get_mut(branch) {
            txn.cleanup_key = Some(key);
        }
    }

    pub(super) async fn do_cancel_txns_for_call(
        &mut self,
        endpoint: &dyn UdpEndpoint,
        call_ref: &str,
    ) {
        // O(k-branches) over just this call's live branches (the lockstep index),
        // not an O(total_txns) scan of the whole map.
        //
        // CLIENT transactions only. The eviction's job is to stop Timer B/F firing
        // against a vanished call — both are client timers. A SERVER txn must be
        // left to its own Timer H/J: cancelling a Completed BYE/INVITE server txn
        // here would drop its retransmit-absorption window (RFC 3261 §17.2.1/§17.2.2),
        // so a retransmitted request after teardown builds a fresh txn and 481s
        // upstream instead of replaying the cached final.
        //
        // The SAME retransmit-absorption argument exempts a client INVITE txn in
        // **Completed** (a non-2xx final, ACKed, holding Timer D — §17.1.1.2): its
        // Timer B/F are already cancelled, and deleting it would drop the re-ACK
        // window, so a rejected leg abandoned by a reroute (or a call torn down
        // before the reject's hop-ACK recovered) strands the UAS retransmitting
        // its final to Timer H, never re-ACKed. Such a txn is DETACHED instead:
        // its call attribution is dropped (so `has_txns_for` / `ActiveTxnCount` /
        // the ADR-0014 CallQuiesced timing are exactly as if it were cancelled)
        // while the txn itself lives on to re-ACK + absorb until its own Timer D
        // cleanup deletes it.
        //
        // The SAME detach — for the SAME "finish your in-flight protocol
        // obligation off the vanished call's books" reason — extends to an ACTIVE
        // (Trying/Proceeding, still awaiting its final) non-INVITE CLIENT txn
        // (NOTIFY / BYE / INFO / MESSAGE …). Deleting it here cancels its Timer E
        // retransmit + Timer F before the first 500 ms retransmit can fire, so a
        // datagram lost right as the call is torn down is NEVER re-sent — in a
        // REFER transfer a dropped progress NOTIFY, whose BYE lands within a few
        // ms, leaves a permanent hole in that leg's in-dialog CSeq stream. Detach
        // instead: Timer E keeps re-sending (500 ms → ×2 capped at T2) until the
        // final arrives (the inbound-final path `delete_txn`s it — call_ref no
        // longer needed) or Timer F (64·T1 = 32 s) self-reaps it (`fire_timeout` →
        // `delete_txn`). Bounded — a permanently-lost final still reaps at Timer F,
        // never an infinite retransmit or a leak, and the detach drops the call
        // attribution so it is not counted as a live txn for the gone call.
        //
        // An ACTIVE client INVITE is deliberately still DELETED (its Timer B is the
        // call's own failure-detection deadline — teardown means give up now).
        let branches: Vec<String> = match self.txn_index.get(call_ref) {
            Some(set) => set.iter().cloned().collect(),
            None => return,
        };
        for branch in branches {
            let (is_client, is_completed, is_non_invite) =
                self.txns.get(branch.as_str()).map_or((false, false, false), |t| {
                    (
                        t.role == TxnRole::Client,
                        t.state == TxnState::Completed,
                        t.kind == TxnKind::NonInvite,
                    )
                });
            if !is_client {
                continue;
            }
            if is_completed || is_non_invite {
                let cr = self.txns.get_mut(branch.as_str()).and_then(|t| t.call_ref.take());
                self.untrack_call_ref(&cr, &branch);
            } else {
                // An active client INVITE dying with a never-sent held CANCEL:
                // under the bounded policy, put it on the wire first (ADR-0028
                // — every emitted CANCEL reaches the callee; eviction must not
                // swallow one still inside its grace window). Marked sent so
                // `delete_txn` does not double-count it as dropped. Strict
                // §9.1 policy keeps the old drop.
                let flush =
                    match self.txns.get_mut(branch.as_str()).and_then(|t| t.held_cancel.as_mut()) {
                        Some(h)
                            if h.wire == CancelWire::Held
                                && self.cancel_hold_grace_ms.is_some() =>
                        {
                            h.wire = CancelWire::SentPre1xx;
                            Some((h.buf.clone(), h.dest))
                        }
                        _ => None,
                    };
                if let Some((buf, dest)) = flush {
                    self.send_buffer(endpoint, &buf, dest).await;
                    self.metrics
                        .held_cancels_flushed_pre1xx
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                if self.delete_txn(&branch) {
                    self.metrics
                        .txn_cancelled_on_call_evict
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
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
                // that already took its final (Completed holds for Timer D).
                let (key, grace_key, flush) = match self.txns.get_mut(branch) {
                    Some(txn)
                        if txn.role == TxnRole::Client && txn.state != TxnState::Completed =>
                    {
                        txn.state = TxnState::Proceeding;
                        (
                            (txn.kind == TxnKind::Invite)
                                .then(|| txn.retransmit_key.take())
                                .flatten(),
                            txn.cancel_grace_key.take(),
                            true,
                        )
                    }
                    _ => (None, None, false),
                };
                self.cancel_timer(key);
                self.cancel_timer(grace_key);
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
            // is owed, not even the pre-1xx re-send). A never-sent held CANCEL
            // cannot have drawn a response, so only on-wire state clears here.
            if resp.cseq().method() == Method::Cancel {
                let key = match self.txns.get_mut(branch) {
                    Some(t)
                        if t.role == TxnRole::Client
                            && t.kind == TxnKind::Invite
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

            // Snapshot what we need before mutating.
            let client_match = self
                .txns
                .get(branch)
                .filter(|t| t.role == TxnRole::Client)
                .map(|t| (t.kind, t.state, t.original_request.clone(), t.destination));
            matched_client_txn = client_match.is_some();

            if let Some((kind, state, original_request, destination)) = client_match {
                if resp.status() < 200 {
                    // Provisional 1xx>100 — Proceeding. Ignore once Completed (a
                    // late provisional must not downgrade a txn that already took its
                    // final). INVITE stops retransmitting (§17.1.1.2); non-INVITE
                    // continues at T2 (§17.1.2.2), so only cancel retransmit for INVITE.
                    if state != TxnState::Completed {
                        let (key, grace_key) = match self.txns.get_mut(branch) {
                            Some(txn) => {
                                txn.state = TxnState::Proceeding;
                                (
                                    (kind == TxnKind::Invite)
                                        .then(|| txn.retransmit_key.take())
                                        .flatten(),
                                    txn.cancel_grace_key.take(),
                                )
                            }
                            None => (None, None),
                        };
                        self.cancel_timer(key);
                        self.cancel_timer(grace_key);
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
                    // FIRST non-2xx final: hold the txn in Completed for Timer D so
                    // retransmitted finals are re-ACKed + absorbed, not re-surfaced.
                    // The auto-ACK silenced the UAS's retransmission *trigger*, so
                    // without Timer D a lost ACK would have the UAS resend the final
                    // unanswered until its own Timer H, each resend re-emitting
                    // upstream as a duplicate.
                    let (r, t, g, cr) = match self.txns.get_mut(branch) {
                        Some(txn) => {
                            txn.state = TxnState::Completed;
                            // A final ends the txn — a still-held CANCEL is moot
                            // (the UAS already rejected) and an on-wire one stops
                            // retransmitting. Cleared; counted as dropped only if
                            // it never made the wire (a grace-sent copy is
                            // already accounted pre-1xx).
                            if txn.held_cancel.take().is_some_and(|h| h.wire == CancelWire::Held) {
                                self.metrics
                                    .held_cancels_dropped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            (
                                txn.retransmit_key.take(),
                                txn.timeout_key.take(),
                                txn.cancel_grace_key.take(),
                                txn.cancel_retransmit_key.take(),
                            )
                        }
                        None => (None, None, None, None),
                    };
                    self.cancel_timer(r);
                    self.cancel_timer(t);
                    self.cancel_timer(g);
                    self.cancel_timer(cr);
                    self.hold_for_timer_d(branch);
                    // Critical: we auto-ACKed (silenced the UAS's resend), so this is
                    // the app's only delivery of the final.
                    self.emit_critical(TransactionEvent::Message {
                        message: Box::new(SipMessage::Response(resp)),
                        src,
                        matched_client_txn: true,
                    });
                    return true;
                } else {
                    // 2xx INVITE (the TU ACKs end-to-end) or any non-INVITE final:
                    // terminate the client txn immediately.
                    self.delete_txn(branch);
                    self.emit_critical(TransactionEvent::Message {
                        message: Box::new(SipMessage::Response(resp)),
                        src,
                        matched_client_txn: true,
                    });
                    return true;
                }
            }
            // (a response landing on a server txn is anomalous — pass through)
        }

        // Provisionals and unmatched responses are protocol-redelivered → lossy.
        // A re-offered miss stays with the consumer that holds it.
        if matched_client_txn || !reoffer {
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
    /// gives up at 64·T1 — or, for an initial INVITE, at the owner's tighter
    /// `invite_first_response_timeout_ms` (default Timer B; ADR-0032 X1). The
    /// window `invite_initial_timeout_ms` owns opens at an INVITE's first
    /// provisional, where [`Owner::rearm_invite_bound`] arms it — initial and
    /// in-dialog alike, since a peer that answered 1xx has left Calling; a
    /// non-INVITE never leaves Timer F.
    fn client_timeout(&self, kind: TxnKind, req: &SipRequest) -> (u64, TimeoutKind) {
        match kind {
            // `min`: whichever bound is configured tighter is the deadline
            // and owns the give-up.
            TxnKind::Invite if req.to().tag().is_none() => (
                self.invite_first_response_timeout_ms.min(self.invite_initial_timeout_ms),
                TimeoutKind::Response,
            ),
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
    /// provisional arrives. No-op for a non-INVITE and a txn already re-armed —
    /// a second provisional never extends the bound.
    fn rearm_invite_bound(&mut self, branch: &str) {
        let bound = self.invite_initial_timeout_ms;
        let remaining = match self.txns.get(branch) {
            Some(t)
                if t.role == TxnRole::Client
                    && t.kind == TxnKind::Invite
                    && t.timeout_kind == TimeoutKind::Response =>
            {
                bound.saturating_sub(t.created_at.elapsed().as_millis() as u64).max(1)
            }
            _ => return,
        };
        let old = self.txns.get_mut(branch).and_then(|t| t.timeout_key.take());
        self.cancel_timer(old);
        let key = self.timers.insert(Timer::ClientTimeout(branch.to_string()), ms(remaining));
        if let Some(txn) = self.txns.get_mut(branch) {
            txn.timeout_key = Some(key);
            txn.timeout_kind = TimeoutKind::Transaction;
        }
    }
}

/// The top Via's `cr` (callRef) / `lg` (legId) custom params, URL-decoded. The
/// B2BUA's `build_call_via` URL-encodes both (callRefs contain `|`/`@`) and a
/// param value stays as written on the wire, so decoding here is what makes
/// `cancel_txns_for_call` match the natural callRef the caller passes (see the
/// cr/lg round-trip regression test).
pub(super) fn extract_via_custom_params(req: &SipRequest) -> (Option<String>, Option<String>) {
    let top_via = req.top_via();
    let read = |name: &str| top_via.param(name).and_then(ParamValue::as_str).map(decode_param);
    (read("cr"), read("lg"))
}
