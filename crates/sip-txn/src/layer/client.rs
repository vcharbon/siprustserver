//! RFC 3261 §17.1 client (UAC) transactions: creation on `send_request`
//! (CANCEL/ACK go raw — they reuse their INVITE's branch), Timer A/E
//! retransmission, Timer B/F timeout, inbound-response matching (including the
//! non-2xx auto-ACK + Timer D hold), and per-call eviction. The server (UAS)
//! side does NOT live here — see `layer::server`.

use std::net::SocketAddr;

use bytes::Bytes;
use sip_message::generators::generate_ack_for_non_2xx;
use sip_message::header::ParamValue;
use sip_message::message_helpers::decode_param;
use sip_message::{serialize, Method, SipMessage, SipRequest, SipResponse};
use sip_net::UdpEndpoint;

use crate::event::{ClientTransactionHandle, TimeoutKind, TransactionEvent, TxnKind};
use crate::timers::{ms, INVITE_INITIAL_TIMEOUT, T1, T2, TIMER_B, TIMER_D, TIMER_F};

use super::owner::Owner;
use super::txn::{Timer, Transaction, TxnRole, TxnState};

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
        // — the same path the B2BUA already uses (OutboundTxnMode::Raw) — without
        // touching the map. This closes the branch-collision foot-gun at its source,
        // so the branch-only key never has to disambiguate by method.
        if msg.method == Method::Cancel || msg.method == Method::Ack {
            self.send_buffer(endpoint, &buf, dest).await;
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
            retransmit_buf: None,
            retransmit_interval_ms: T1,
            retransmit_elapsed_ms: T1,
            retransmit_max_ms: TIMER_B,
        };
        self.set_txn(txn);

        self.send_buffer(endpoint, &buf, dest).await;
        self.start_client_retransmit(&branch, buf, dest, client_timeout_ms(txn_type, &msg));

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

    fn start_client_retransmit(
        &mut self,
        branch: &str,
        buf: Bytes,
        dest: SocketAddr,
        max_ms: u64,
    ) {
        let r_key = self.timers.insert(Timer::ClientRetransmit(branch.to_string()), ms(T1));
        let t_key = self.timers.insert(Timer::ClientTimeout(branch.to_string()), ms(max_ms));
        if let Some(txn) = self.txns.get_mut(branch) {
            txn.retransmit_key = Some(r_key);
            txn.timeout_key = Some(t_key);
            txn.retransmit_buf = Some(buf);
            txn.retransmit_interval_ms = T1;
            txn.retransmit_elapsed_ms = T1;
            txn.retransmit_max_ms = max_ms;
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

    pub(super) fn fire_timeout(&mut self, branch: &str) {
        let (call_ref, leg_id, method, destination, timeout_kind) = match self.txns.get(branch) {
            Some(t) if t.state.is_active() => {
                let method = t
                    .original_request
                    .as_ref()
                    .map(|r| r.method.to_string())
                    .or_else(|| match t.kind {
                        TxnKind::Invite => Some("INVITE".to_string()),
                        TxnKind::NonInvite => None,
                    });
                // Discriminate WHICH timer fired from the txn's armed window
                // (`retransmit_max_ms`, set in `start_client_retransmit` from
                // `client_timeout_ms`): the long out-of-dialog INVITE backstop is
                // `INVITE_INITIAL_TIMEOUT`; Timer B/F is the short 64×T1 window.
                let timeout_kind = if t.retransmit_max_ms == INVITE_INITIAL_TIMEOUT {
                    TimeoutKind::Transaction
                } else {
                    TimeoutKind::Response
                };
                (t.call_ref.clone(), t.leg_id.clone(), method, t.destination, timeout_kind)
            }
            _ => return,
        };
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

    pub(super) fn do_cancel_txns_for_call(&mut self, call_ref: &str) {
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
            } else if self.delete_txn(&branch) {
                self.metrics
                    .txn_cancelled_on_call_evict
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        if resp.status == 100 {
            if !branch.is_empty() {
                let key = match self.txns.get_mut(branch) {
                    Some(txn) if txn.role == TxnRole::Client => {
                        txn.state = TxnState::Proceeding;
                        (txn.kind == TxnKind::Invite).then(|| txn.retransmit_key.take()).flatten()
                    }
                    _ => None,
                };
                self.cancel_timer(key);
            }
            return;
        }

        if !branch.is_empty() {
            // CANCEL responses reuse the INVITE branch — never match them to the
            // INVITE client txn (would tear it down on the 200 and miss the 487).
            if resp.cseq().method() == &Method::Cancel {
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
                if resp.status < 200 {
                    // Provisional 1xx>100 — Proceeding. Ignore once Completed (a
                    // late provisional must not downgrade a txn that already took its
                    // final). INVITE stops retransmitting (§17.1.1.2); non-INVITE
                    // continues at T2 (§17.1.2.2), so only cancel retransmit for INVITE.
                    if state != TxnState::Completed {
                        let key = match self.txns.get_mut(branch) {
                            Some(txn) => {
                                txn.state = TxnState::Proceeding;
                                (kind == TxnKind::Invite).then(|| txn.retransmit_key.take()).flatten()
                            }
                            None => None,
                        };
                        self.cancel_timer(key);
                    }
                } else if kind == TxnKind::Invite && resp.status >= 300 {
                    // Non-2xx INVITE final: (re-)ACK hop-by-hop (RFC 3261 §17.1.1.2).
                    if let (Some(orig), Some(dest)) = (original_request, destination) {
                        let ack = generate_ack_for_non_2xx(&orig, &resp);
                        // The recipe froze the ACK into its own image, which IS
                        // its wire form — send that instead of rendering it twice.
                        self.send_buffer(endpoint, &ack.raw, dest).await;
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
                    let (r, t) = match self.txns.get_mut(branch) {
                        Some(txn) => {
                            txn.state = TxnState::Completed;
                            (txn.retransmit_key.take(), txn.timeout_key.take())
                        }
                        None => (None, None),
                    };
                    self.cancel_timer(r);
                    self.cancel_timer(t);
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

/// RFC 3261 §17.1 client-transaction timeout (Timer B/F). Differentiated for
/// INVITE: an INITIAL (out-of-dialog — no To-tag) INVITE is a call setup whose
/// ring time the upper layer's no-answer timer owns, so it gets the long
/// [`INVITE_INITIAL_TIMEOUT`] backstop (below the 180 s Timer-C mark, above any
/// deployment no-answer timeout). An in-dialog re-INVITE (To-tag present) and
/// every non-INVITE keep the 32 s failure-detection timeout.
fn client_timeout_ms(kind: TxnKind, req: &SipRequest) -> u64 {
    match kind {
        TxnKind::Invite if req.to().tag().is_none() => INVITE_INITIAL_TIMEOUT,
        TxnKind::Invite => TIMER_B,
        TxnKind::NonInvite => TIMER_F,
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
