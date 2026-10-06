//! RFC 3261 §9.1 / §17.1.1.3 for an INVITE client transaction that gave up
//! and whose CANCEL leaves late: the 64·T1 hold restarts at the CANCEL's
//! first send, and runs its full length however old the transaction is. A
//! 487 the CANCEL provokes, arriving just inside that hold, is ACKed and
//! handed up.
//!
//! Timing: with a 40 s INVITE bound the safety-net sweep (every
//! `TXN_SWEEP_INTERVAL`, 10 s, from the layer's spawn) sees a 75 s-old
//! transaction at its 80 s tick. The CANCEL leaves just before the give-up
//! hold ends, so its restarted hold runs past 80 s, and the 487 lands after
//! that tick.

use crate::common;
use std::sync::Arc;

use common::*;
use sip_message::generators::{
    generate_cancel, generate_response, GenerateResponseOpts, InviteClientTransactionHandle,
};
use sip_message::{SipMessage, SipRequest};
use sip_txn::timers::{T4, TIMER_B, TIMER_D};
use sip_txn::{IdGen, TimeoutKind, TransactionConfig, TransactionEvent, TxnKind};

const BOUND_MS: u64 = 40_000;
/// The callee's Timer G repeats its 487 at 0.5, 1.5, 3.5, 7.5 s, then every
/// T2 until 64·T1 (§17.2.1); its last copy leaves 31.5 s after the first.
const LAST_487_COPY_MS: u64 = 31_500;
const _: () = assert!(LAST_487_COPY_MS < TIMER_B);

const CALLEE_TAG: &str = "callee-tag";

async fn stack() -> Stack {
    Stack::build_with_config(
        5,
        64,
        TransactionConfig {
            udp_queue_max: 64,
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_initial_timeout_ms: BOUND_MS,
            ..Default::default()
        },
    )
    .await
}

async fn inject_answer(stack: &Stack, req: &SipRequest, status: u16, reason: &str) {
    let opts = GenerateResponseOpts { to_tag: Some(CALLEE_TAG.to_string()), ..Default::default() };
    stack.inject(generate_response(req, status, reason, &opts).image()).await;
}

fn timeouts(events: &[TransactionEvent]) -> Vec<TimeoutKind> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Timeout { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

/// The 487s to the INVITE handed up, with whether the layer matched them.
fn handed_up_487s(events: &[TransactionEvent]) -> Vec<bool> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, matched_client_txn, .. } => {
                match message.as_ref() {
                    SipMessage::Response(r)
                        if r.status() == 487 && r.cseq().method() == "INVITE" =>
                    {
                        Some(*matched_client_txn)
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

fn cancel_of(invite: &SipRequest) -> SipRequest {
    generate_cancel(&InviteClientTransactionHandle { original_invite: invite.clone() }, &[])
}

/// The callee answers 200 to the CANCEL; its 487 and every Timer G copy but
/// the last are lost. The last copy is ACKed and handed up, and Timer D, from
/// that final, replaces the give-up hold: a duplicate of the copy the network
/// delivers just inside T4 (RFC 3261 §17.1.2.2), past the CANCEL's 64·T1, is
/// still re-ACKed and absorbed (§17.1.1.2). Timer D then purges it.
async fn the_last_487_copy_is_acked(stack: &mut Stack, invite: &SipRequest, cancel: &SipRequest) {
    inject_answer(stack, cancel, 200, "OK").await;
    elapse_ms(LAST_487_COPY_MS).await;
    let _ = stack.drain_peer();
    inject_answer(stack, invite, 487, "Request Terminated").await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(count_requests(&out, "ACK"), 1, "the 487 is ACKed (§17.1.1.3): {out:?}");
    let events = stack.drain_events();
    assert_eq!(handed_up_487s(&events), vec![true], "the 487 is handed up as the INVITE's final");
    assert!(timeouts(&events).is_empty(), "the give-up fired once");

    elapse_ms(T4 - 100 - 20).await;
    inject_answer(stack, invite, 487, "Request Terminated").await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(count_requests(&out, "ACK"), 1, "Timer D re-ACKs the duplicate: {out:?}");
    assert!(handed_up_487s(&stack.drain_events()).is_empty(), "and absorbs it");

    elapse_ms(TIMER_D - (T4 - 100) - 100).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "Timer D still holds it");
    elapse_ms(200).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "Timer D purges it");
    assert_eq!(stack.txn.metrics().sweep_reaped(), 0, "by its own timer");
}

/// The INVITE rings to its 40 s bound and gives up; the consumer's CANCEL
/// leaves 31 s later, 1 s before the give-up hold would end, and restarts it.
#[tokio::test(start_paused = true)]
async fn a_ringing_invite_holds_64_t1_from_a_cancel_sent_late_in_its_give_up_hold() {
    let mut stack = stack().await;
    let invite = outbound_request("INVITE", "z9hG4bK-late-cancel-ringing");
    stack.txn.send_request(invite.clone(), addr(PEER), TxnKind::Invite).await.unwrap();
    inject_answer(&stack, &invite, 180, "Ringing").await;
    elapse_ms(BOUND_MS + 100).await;
    assert_eq!(timeouts(&stack.drain_events()), vec![TimeoutKind::Transaction]);

    elapse_ms(TIMER_B - 1_100).await;
    let cancel = cancel_of(&invite);
    stack.txn.send_request(cancel.clone(), addr(PEER), TxnKind::Invite).await.unwrap();
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1, "the CANCEL leaves");

    the_last_487_copy_is_acked(&mut stack, &invite, &cancel).await;
}

/// The INVITE draws nothing and gives up at Timer B; the consumer's CANCEL
/// is held (§9.1: no provisional yet). A provisional arriving 1 s before the
/// give-up hold would end sends the CANCEL, which restarts the hold.
#[tokio::test(start_paused = true)]
async fn an_unanswered_invite_holds_64_t1_from_a_cancel_a_late_provisional_releases() {
    let mut stack = stack().await;
    let invite = outbound_request("INVITE", "z9hG4bK-late-cancel-silent");
    stack.txn.send_request(invite.clone(), addr(PEER), TxnKind::Invite).await.unwrap();
    elapse_ms(TIMER_B + 100).await;
    assert_eq!(timeouts(&stack.drain_events()), vec![TimeoutKind::Response]);

    let cancel = cancel_of(&invite);
    stack.txn.send_request(cancel.clone(), addr(PEER), TxnKind::Invite).await.unwrap();
    elapse_ms(TIMER_B - 1_100).await;
    let out = stack.drain_peer();
    assert_eq!(count_requests(&out, "CANCEL"), 0, "held: no provisional yet");

    inject_answer(&stack, &invite, 180, "Ringing").await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1, "the provisional releases it");
    let _ = stack.drain_events();

    the_last_487_copy_is_acked(&mut stack, &invite, &cancel).await;
}
