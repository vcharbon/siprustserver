//! RFC 3261 §17.1 client (UAC) transactions: creation on `send_request`
//! (CANCEL/ACK go raw — they reuse their INVITE's branch; a CANCEL for a
//! response-less INVITE txn is held until the first provisional or the grace
//! expiry, whichever comes first — §9.1 bounded per ADR-0028),
//! Timer A/E retransmission, Timer B/F timeout, inbound-response matching
//! (including the non-2xx auto-ACK + Timer D hold), and per-call eviction. The
//! server (UAS) side does NOT live here — see `layer::server`.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::generators::generate_ack_for_non_2xx;
use sip_message::header::ParamValue;
use sip_message::param_codec::decode_param;
use sip_message::{serialize, Method, SipMessage, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::event::{ClientTransactionHandle, TimeoutKind, TransactionEvent, TxnKind};
use crate::timers::{ms, T1, T2, TIMER_B, TIMER_D, TIMER_F};

use super::owner::Owner;
use super::txn::{HeldCancel, Timer, Transaction, TxnRole, TxnState};

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
                                if !t
                                    .held_cancel
                                    .as_ref()
                                    .is_some_and(|h| h.sent_pre1xx) =>
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
                            .replace(HeldCancel { buf, dest, sent_pre1xx: false })
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
                    self.metrics
                        .cancels_held
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                CancelGate::Suppress => {
                    self.metrics
                        .cancels_suppressed_on_final
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                CancelGate::Send => {
                    // A CANCEL passing through on a Trying txn whose grace copy
                    // already went out replaces the parked datagram, so the one
                    // re-send on a late provisional carries the NEWEST CANCEL.
                    if msg.method() == Method::Cancel {
                        if let Some(held) = msg
                            .top_via()
                            .branch()
                            .and_then(|b| self.txns.get_mut(b))
                            .and_then(|t| t.held_cancel.as_mut())
                            .filter(|h| h.sent_pre1xx)
                        {
                            held.buf = buf.clone();
                            held.dest = dest;
                        }
                    }
                    self.send_buffer(endpoint, &buf, dest).await;
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

        let txn = Transaction {
            branch: branch.clone(),
            role: TxnRole::Client,
            kind: txn_type,
            call_id: msg.call_id().as_str().to_string(),
            from_tag: msg.from().tag().unwrap_or_default().to_string(),
            original_request: matches!(txn_type, TxnKind::Invite).then(|| msg.clone()),
            last_response: None,
            last_response_status: None,
            call_ref,
            leg_id,
            state: TxnState::Trying,
            destination: Some(dest),
            created_at: tokio::time::Instant::now(),
            uas_to_tag: None,
            retransmit_key: None,
            timeout_key: None,
            cleanup_key: None,
            held_cancel: None,
            cancel_grace_key: None,
            retransmit_buf: None,
            retransmit_interval_ms: T1,
            retransmit_elapsed_ms: T1,
            retransmit_max_ms: TIMER_B,
            timeout_kind: TimeoutKind::Response,
        };
        self.set_txn(txn);

        self.send_buffer(endpoint, &buf, dest).await;
        let (max_ms, timeout_kind) = self.client_timeout(txn_type, &msg);
        self.start_client_retransmit(&branch, buf, dest, max_ms, timeout_kind);

        match txn_type {
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
        }
    }

    /// Put a held CANCEL on the wire — called when its INVITE client txn takes
    /// its first provisional (RFC 3261 §9.1 "wait for the arrival of a
    /// provisional response before sending"). A datagram the grace expiry
    /// already sent pre-1xx goes out ONCE more here — the UAS that 481'd the
    /// pre-1xx copy has built its server transaction by now, so this is the
    /// matchable send — counted as a re-flush, not a fresh flush.
    pub(super) async fn flush_held_cancel(
        &self,
        endpoint: &dyn UdpEndpoint,
        held: Option<HeldCancel>,
    ) {
        if let Some(h) = held {
            self.send_buffer(endpoint, &h.buf, h.dest).await;
            let counter = if h.sent_pre1xx {
                &self.metrics.held_cancels_reflushed
            } else {
                &self.metrics.held_cancels_flushed
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Grace expiry for a held CANCEL (ADR-0028, [`Timer::CancelGrace`]): the
    /// branch is still response-less past the courtesy window, so the CANCEL
    /// goes on the wire NOW — a callee that never answers anything must still
    /// hear the cancellation, or an abandoned setup rings to the callee's own
    /// give-up while the caller is long gone. The datagram stays armed for one
    /// re-send on the first provisional (see [`flush_held_cancel`]).
    pub(super) async fn fire_cancel_grace(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let send = match self.txns.get_mut(branch) {
            Some(t) if t.state == TxnState::Trying => match t.held_cancel.as_mut() {
                Some(h) if !h.sent_pre1xx => {
                    h.sent_pre1xx = true;
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
        let r_key = self.timers.insert(Timer::ClientRetransmit(branch.to_string()), ms(T1));
        let t_key = self.timers.insert(Timer::ClientTimeout(branch.to_string()), ms(max_ms));
        if let Some(txn) = self.txns.get_mut(branch) {
            txn.retransmit_key = Some(r_key);
            txn.timeout_key = Some(t_key);
            txn.retransmit_buf = Some(buf);
            txn.retransmit_interval_ms = T1;
            txn.retransmit_elapsed_ms = T1;
            // Retransmission stops at Timer B (RFC 3261 §17.1.1.2) even when
            // the give-up timer runs longer: a raised initial-INVITE bound
            // extends the wait for a response, never the retransmit storm.
            txn.retransmit_max_ms = max_ms.min(TIMER_B);
            txn.timeout_kind = timeout_kind;
            txn.destination = Some(dest);
        }
    }

    pub(super) async fn fire_retransmit(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let (buf, dest, kind, interval, elapsed, max) = match self.txns.get(branch) {
            Some(t) if t.state.is_active() => match (&t.retransmit_buf, t.destination) {
                (Some(buf), Some(dest)) => (
                    buf.clone(),
                    dest,
                    t.kind,
                    t.retransmit_interval_ms,
                    t.retransmit_elapsed_ms,
                    t.retransmit_max_ms,
                ),
                _ => return,
            },
            _ => return, // completed/terminated/gone — stop retransmitting
        };

        self.send_buffer(endpoint, &buf, dest).await;

        // Next interval: INVITE doubles unbounded; non-INVITE caps at T2.
        let next_interval = match kind {
            TxnKind::Invite => interval * 2,
            TxnKind::NonInvite => std::cmp::min(interval * 2, T2),
        };
        let next_elapsed = elapsed + next_interval;
        if next_elapsed < max {
            let key = self
                .timers
                .insert(Timer::ClientRetransmit(branch.to_string()), ms(next_interval));
            if let Some(txn) = self.txns.get_mut(branch) {
                txn.retransmit_key = Some(key);
                txn.retransmit_interval_ms = next_interval;
                txn.retransmit_elapsed_ms = next_elapsed;
            }
        }
    }

    pub(super) async fn fire_timeout(&mut self, endpoint: &dyn UdpEndpoint, branch: &str) {
        let (call_ref, leg_id, method, destination, timeout_kind) = match self.txns.get(branch) {
            Some(t) if t.state.is_active() => {
                let method = t
                    .original_request
                    .as_ref()
                    .map(|r| r.method().to_string())
                    .or_else(|| match t.kind {
                        TxnKind::Invite => Some("INVITE".to_string()),
                        TxnKind::NonInvite => None,
                    });
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
            Some(h) if !h.sent_pre1xx && self.cancel_hold_grace_ms.is_some() => {
                h.sent_pre1xx = true;
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
            let (is_client, is_completed, is_non_invite) = self
                .txns
                .get(branch.as_str())
                .map_or((false, false, false), |t| {
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
                let flush = match self.txns.get_mut(branch.as_str()).and_then(|t| t.held_cancel.as_mut()) {
                    Some(h) if !h.sent_pre1xx && self.cancel_hold_grace_ms.is_some() => {
                        h.sent_pre1xx = true;
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
        let top_via = resp.top_via();
        let branch = top_via.branch().unwrap_or_default();

        // 100 Trying: absorb after nudging the matching client txn's state. An
        // INVITE client stops retransmitting on a provisional (§17.1.1.2); a
        // non-INVITE client KEEPS retransmitting at T2 in Proceeding (§17.1.2.2),
        // so leave its timer running.
        if resp.status() == 100 {
            if !branch.is_empty() {
                // Like the 1xx>100 path: a late 100 must not downgrade a txn
                // that already took its final (Completed holds for Timer D).
                let (key, grace_key, held) = match self.txns.get_mut(branch) {
                    Some(txn)
                        if txn.role == TxnRole::Client
                            && txn.state != TxnState::Completed =>
                    {
                        txn.state = TxnState::Proceeding;
                        (
                            (txn.kind == TxnKind::Invite)
                                .then(|| txn.retransmit_key.take())
                                .flatten(),
                            txn.cancel_grace_key.take(),
                            txn.held_cancel.take(),
                        )
                    }
                    _ => (None, None, None),
                };
                self.cancel_timer(key);
                self.cancel_timer(grace_key);
                self.flush_held_cancel(endpoint, held).await;
            }
            return;
        }

        if !branch.is_empty() {
            // CANCEL responses reuse the INVITE branch — never match them to the
            // INVITE client txn (would tear it down on the 200 and miss the 487).
            if resp.cseq().method() == Method::Cancel {
                self.emit(TransactionEvent::Message {
                    message: Box::new(SipMessage::Response(resp)),
                    src,
                });
                return;
            }

            // Snapshot what we need before mutating.
            let client_match = self
                .txns
                .get(branch)
                .filter(|t| t.role == TxnRole::Client)
                .map(|t| (t.kind, t.state, t.original_request.clone(), t.destination));

            if let Some((kind, state, original_request, destination)) = client_match {
                if resp.status() < 200 {
                    // Provisional 1xx>100 — Proceeding. Ignore once Completed (a
                    // late provisional must not downgrade a txn that already took its
                    // final). INVITE stops retransmitting (§17.1.1.2); non-INVITE
                    // continues at T2 (§17.1.2.2), so only cancel retransmit for INVITE.
                    if state != TxnState::Completed {
                        let (key, grace_key, held) = match self.txns.get_mut(branch) {
                            Some(txn) => {
                                txn.state = TxnState::Proceeding;
                                (
                                    (kind == TxnKind::Invite)
                                        .then(|| txn.retransmit_key.take())
                                        .flatten(),
                                    txn.cancel_grace_key.take(),
                                    txn.held_cancel.take(),
                                )
                            }
                            None => (None, None, None),
                        };
                        self.cancel_timer(key);
                        self.cancel_timer(grace_key);
                        self.flush_held_cancel(endpoint, held).await;
                    }
                } else if kind == TxnKind::Invite && resp.status() >= 300 {
                    // Non-2xx INVITE final: (re-)ACK hop-by-hop (RFC 3261 §17.1.1.2).
                    if let (Some(orig), Some(dest)) = (original_request, destination) {
                        let ack = generate_ack_for_non_2xx(&orig, &resp);
                        // The recipe froze the ACK into its own image, which IS
                        // its wire form — send that instead of rendering it twice.
                        self.send_buffer(endpoint, ack.image(), dest).await;
                    }
                    if state == TxnState::Completed {
                        // A RETRANSMITTED non-2xx final (our first ACK was lost): we
                        // just re-ACKed it above; absorb without re-notifying.
                        return;
                    }
                    // FIRST non-2xx final: hold the txn in Completed for Timer D so
                    // retransmitted finals are re-ACKed + absorbed, not re-surfaced.
                    // The auto-ACK silenced the UAS's retransmission *trigger*, so
                    // without Timer D a lost ACK would have the UAS resend the final
                    // unanswered until its own Timer H, each resend re-emitting
                    // upstream as a duplicate.
                    let (r, t, g) = match self.txns.get_mut(branch) {
                        Some(txn) => {
                            txn.state = TxnState::Completed;
                            // A final ends the txn — a still-held CANCEL is moot
                            // (the UAS already rejected). Cleared; counted as
                            // dropped only if it never made the wire (a grace-
                            // sent copy is already accounted pre-1xx).
                            if txn.held_cancel.take().is_some_and(|h| !h.sent_pre1xx) {
                                self.metrics
                                    .held_cancels_dropped
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                            (
                                txn.retransmit_key.take(),
                                txn.timeout_key.take(),
                                txn.cancel_grace_key.take(),
                            )
                        }
                        None => (None, None, None),
                    };
                    self.cancel_timer(r);
                    self.cancel_timer(t);
                    self.cancel_timer(g);
                    let key = self.timers.insert(Timer::Cleanup(branch.to_string()), ms(TIMER_D));
                    if let Some(txn) = self.txns.get_mut(branch) {
                        txn.cleanup_key = Some(key);
                    }
                    // Critical: we auto-ACKed (silenced the UAS's resend), so this is
                    // the app's only delivery of the final.
                    self.emit_critical(TransactionEvent::Message {
                        message: Box::new(SipMessage::Response(resp)),
                        src,
                    });
                    return;
                } else {
                    // 2xx INVITE (the TU ACKs end-to-end) or any non-INVITE final:
                    // terminate the client txn immediately.
                    self.delete_txn(branch);
                    self.emit_critical(TransactionEvent::Message {
                        message: Box::new(SipMessage::Response(resp)),
                        src,
                    });
                    return;
                }
            }
            // (a response landing on a server txn is anomalous — pass through)
        }

        // Provisionals and unmatched responses are protocol-redelivered → lossy.
        self.emit(TransactionEvent::Message {
            message: Box::new(SipMessage::Response(resp)),
            src,
        });
    }
}

impl Owner {
    /// RFC 3261 §17.1 client-transaction timeout (Timer B/F) plus the
    /// [`TimeoutKind`] its expiry emits. Differentiated for INVITE: an INITIAL
    /// (out-of-dialog — no To-tag) INVITE is a call setup whose ring time the
    /// upper layer's no-answer timer owns, so it gets the configured
    /// out-of-dialog bound (`invite_initial_timeout_ms`, default 158 s — above
    /// any deployment setup/no-answer timeout) and a `Transaction` timeout. An
    /// in-dialog re-INVITE (To-tag present) and every non-INVITE keep the 32 s
    /// failure-detection timeout (`Response`).
    fn client_timeout(&self, kind: TxnKind, req: &SipRequest) -> (u64, TimeoutKind) {
        match kind {
            TxnKind::Invite if req.to().tag().is_none() => {
                (self.invite_initial_timeout_ms, TimeoutKind::Transaction)
            }
            TxnKind::Invite => (TIMER_B, TimeoutKind::Response),
            TxnKind::NonInvite => (TIMER_F, TimeoutKind::Response),
        }
    }
}

/// The top Via's `cr` (callRef) / `lg` (legId) custom params, URL-decoded. The
/// B2BUA's `build_call_via` URL-encodes both (callRefs contain `|`/`@`) and a
/// param value stays as written on the wire, so decoding here is what makes
/// `cancel_txns_for_call` match the natural callRef the caller passes (see the
/// cr/lg round-trip regression test).
fn extract_via_custom_params(req: &SipRequest) -> (Option<String>, Option<String>) {
    let top_via = req.top_via();
    let read = |name: &str| top_via.param(name).and_then(ParamValue::as_str).map(decode_param);
    (read("cr"), read("lg"))
}
