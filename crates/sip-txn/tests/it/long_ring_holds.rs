//! An INVITE that rings longer than the `TXN_MAX_AGE` margin keeps the hold
//! its final starts, for that hold's full length: Timer D on the client
//! (RFC 3261 §17.1.1.2), Timer G/H then Timer I on the server (§17.2.1), and
//! Timer M on an orphaned client that took a 2xx (RFC 6026 §7.2). The final
//! moves the transaction's one lifetime deadline to the end of its hold,
//! measured from the event that starts it, never from the transaction's
//! birth; the safety-net sweep reads that deadline and reaps nothing.
//!
//! Timing: the callee rings `RING_MS` (60 s, past the 35 s margin) under the
//! default INVITE bound, or `TELEPHONY_RING_MS` (180 s) under a 200 s bound;
//! the late message arrives `LATE_MS` (20 s, longer than one
//! `TXN_SWEEP_INTERVAL`) after the final, so a sweep runs inside every hold
//! below while the transaction is far older than the margin.

use crate::common;
use common::*;
use sip_message::generators::{
    generate_cancel, generate_response, GenerateResponseOpts, InviteClientTransactionHandle,
};
use sip_message::header::{Contact, Uri};
use sip_message::{SipMessage, SipRequest, SipResponse};
use sip_txn::timers::{TIMER_D, TIMER_I, TIMER_M, TXN_MAX_AGE, TXN_SWEEP_INTERVAL};
use std::sync::Arc;

use sip_txn::{IdGen, TransactionConfig, TransactionEvent, TxnKind};

const RING_MS: u64 = 60_000;
/// A telephony ring (Timer C > 3 min), inside `TELEPHONY_BOUND_MS`.
const TELEPHONY_RING_MS: u64 = 180_000;
const TELEPHONY_BOUND_MS: u64 = 200_000;
const _: () = assert!(TELEPHONY_RING_MS < TELEPHONY_BOUND_MS);
const LATE_MS: u64 = 20_000;
const _: () = assert!(RING_MS > TXN_MAX_AGE && LATE_MS > TXN_SWEEP_INTERVAL);
const _: () = assert!(LATE_MS < TIMER_D && LATE_MS < TIMER_M);

const CALLEE_TAG: &str = "callee-tag";

/// The callee's `status` answer to `req`, under the callee's To-tag; a 2xx
/// to an INVITE states the callee's Contact (RFC 3261 §12.1.1).
fn answer(req: &SipRequest, status: u16, reason: &str) -> SipResponse {
    let contact = ((200..300).contains(&status) && req.method() == "INVITE")
        .then(|| Contact::from_uri(Uri::sip_user("bob", "10.0.0.1").with_port(5555)));
    generate_response(
        req,
        status,
        reason,
        &GenerateResponseOpts {
            to_tag: Some(CALLEE_TAG.to_string()),
            contact,
            ..Default::default()
        },
    )
}

async fn inject_answer(stack: &Stack, req: &SipRequest, status: u16, reason: &str) {
    stack.inject(answer(req, status, reason).image()).await;
}

/// The `status` responses to an INVITE handed up to the consumer.
fn finals_handed_up(events: &[TransactionEvent], status: u16) -> usize {
    events
        .iter()
        .filter(|e| match e {
            TransactionEvent::Message { message, .. } => matches!(message.as_ref(),
                SipMessage::Response(r) if r.status() == status && r.cseq().method() == "INVITE"),
            _ => false,
        })
        .count()
}

fn requests_of(events: &[TransactionEvent], method: &str) -> Vec<SipRequest> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == method => Some(r.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn ack_branches(msgs: &[SipMessage]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match m {
            SipMessage::Request(r) if r.method() == "ACK" => {
                r.top_via().branch().map(str::to_string)
            }
            _ => None,
        })
        .collect()
}

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

/// A layer whose INVITE bound, both roles, admits a telephony ring.
async fn telephony_stack() -> Stack {
    Stack::build_with_config(
        5,
        64,
        TransactionConfig {
            udp_queue_max: 64,
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_initial_timeout_ms: TELEPHONY_BOUND_MS,
            ..Default::default()
        },
    )
    .await
}

/// The callee rings 60 s and declines 486; the layer's ACK is lost and the
/// callee repeats the 486 20 s later. Timer D still holds the transaction
/// (§17.1.1.2): the repeat is re-ACKed and absorbed, and the consumer hears
/// the final once. Timer D from the first final then purges it.
#[tokio::test(start_paused = true)]
async fn a_client_invite_re_acks_a_repeated_486_after_a_long_ring() {
    client_re_acks_a_repeated_486_after(Stack::build(5, 64, 64).await, RING_MS).await;
}

/// The same after a 180 s ring under a 200 s INVITE bound.
#[tokio::test(start_paused = true)]
async fn a_client_invite_re_acks_a_repeated_486_after_a_180_s_ring() {
    client_re_acks_a_repeated_486_after(telephony_stack().await, TELEPHONY_RING_MS).await;
}

async fn client_re_acks_a_repeated_486_after(mut stack: Stack, ring_ms: u64) {
    let branch = "z9hG4bK-long-ring-486";
    let invite = outbound_request("INVITE", branch);
    stack.txn.send_request(invite.clone(), addr(PEER), TxnKind::Invite).await.unwrap();
    inject_answer(&stack, &invite, 180, "Ringing").await;
    elapse_ms(ring_ms).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();

    inject_answer(&stack, &invite, 486, "Busy Here").await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "the 486 is ACKed");
    assert_eq!(finals_handed_up(&stack.drain_events(), 486), 1, "and handed up");

    // The ACK was lost; the callee's Timer G repeats the 486.
    elapse_ms(LATE_MS - 20).await;
    inject_answer(&stack, &invite, 486, "Busy Here").await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(count_requests(&out, "ACK"), 1, "Timer D re-ACKs the repeated 486: {out:?}");
    assert_eq!(
        finals_handed_up(&stack.drain_events(), 486),
        0,
        "the repeat is absorbed, not surfaced as a second final"
    );

    elapse_ms(TIMER_D - LATE_MS - 100).await;
    assert_eq!(active(&stack), 1, "Timer D still holds it");
    elapse_ms(200).await;
    assert_eq!(active(&stack), 0, "Timer D from the first final purges it");
    assert_eq!(stack.txn.metrics().sweep_reaped(), 0, "by its own timer");
}

/// The consumer rings 60 s and declines 486. Every ACK the caller sends is
/// lost for 20 s: Timer G keeps repeating the 486, and the ACK that finally
/// lands is absorbed (§17.2.1), stopping Timer G. Timer I then purges the
/// transaction.
#[tokio::test(start_paused = true)]
async fn a_server_invite_repeats_its_486_and_absorbs_a_late_ack_after_a_long_ring() {
    server_repeats_its_486_until_a_late_ack_after(Stack::build(5, 64, 64).await, RING_MS).await;
}

/// The same after a 180 s ring under a 200 s INVITE bound.
#[tokio::test(start_paused = true)]
async fn a_server_invite_repeats_its_486_and_absorbs_a_late_ack_after_a_180_s_ring() {
    server_repeats_its_486_until_a_late_ack_after(telephony_stack().await, TELEPHONY_RING_MS).await;
}

async fn server_repeats_its_486_until_a_late_ack_after(mut stack: Stack, ring_ms: u64) {
    let (branch, call_id) = ("z9hG4bK-long-ring-uas", "long-ring-uas");
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(20).await;
    let invites = requests_of(&stack.drain_events(), "INVITE");
    assert_eq!(invites.len(), 1, "the INVITE reaches the consumer");
    let invite = &invites[0];
    stack.txn.send_response(answer(invite, 180, "Ringing"), addr(PEER)).await.unwrap();

    elapse_ms(ring_ms - 20).await;
    stack.txn.send_response(answer(invite, 486, "Busy Here"), addr(PEER)).await.unwrap();
    elapse_ms(LATE_MS / 2 - 100).await;
    let _ = stack.drain_peer();

    // Timer G rungs after the 486: … +7.5, +11.5, +15.5, +19.5 s (T2 cap).
    elapse_ms(LATE_MS / 2).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 486), 3, "Timer G repeats the 486 until the ACK: {out:?}");

    elapse_ms(100).await;
    stack.inject(&inbound_request("ACK", branch, call_id, Some(CALLEE_TAG))).await;
    elapse_ms(20).await;
    assert!(
        requests_of(&stack.drain_events(), "ACK").is_empty(),
        "the ACK of the 486 is absorbed, never handed up"
    );

    elapse_ms(TIMER_I - 100).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 0, "the ACK stopped Timer G");
    assert_eq!(active(&stack), 1, "Timer I holds it");
    elapse_ms(200).await;
    assert_eq!(active(&stack), 0, "Timer I purges it");
    assert_eq!(stack.txn.metrics().sweep_reaped(), 0, "by its own timer");
}

/// The call is released 60 s into the ring with a CANCEL that crosses the
/// callee's 2xx. The orphaned INVITE ACKs the 2xx (§13.2.2.4), and every
/// ACK is lost, so the callee repeats the 2xx on its §13.3.1.4 schedule.
/// Timer M (RFC 6026 §7.2) holds the transaction for each repeat: each draws
/// the same bare ACK, and none reaches the consumer.
#[tokio::test(start_paused = true)]
async fn an_orphaned_invite_re_acks_every_2xx_repeat_after_a_long_ring() {
    let mut stack = Stack::build(5, 64, 64).await;
    let (call_ref, call_id, branch) = ("self|long-ring-2xx", "long-ring-2xx", "z9hG4bK-long-2xx");
    let invite = invite_with_cr_lg(call_ref, call_id, branch, "b-1");
    stack.txn.send_request(invite.clone(), addr(PEER), TxnKind::Invite).await.unwrap();
    inject_answer(&stack, &invite, 180, "Ringing").await;
    elapse_ms(RING_MS).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();

    let cancel =
        generate_cancel(&InviteClientTransactionHandle { original_invite: invite.clone() }, &[]);
    stack.txn.send_request(cancel.clone(), addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    inject_answer(&stack, &invite, 200, "OK").await;
    elapse_ms(20).await;
    // §9.2: the callee had already answered, so the CANCEL draws a 200 only.
    inject_answer(&stack, &cancel, 200, "OK").await;
    elapse_ms(20).await;
    let first = ack_branches(&stack.drain_peer());
    assert_eq!(first.len(), 1, "the orphan ACKs the 2xx");
    assert_ne!(first[0], branch, "§17.1.1.3: the 2xx ACK is its own transaction");

    // The callee's 2xx repeats (§13.3.1.4: T1 doubling, capped at T2, until
    // 64·T1), measured from its first send at the end of the ring.
    let mut at = 40;
    for repeat_at in [500, 1_500, 3_500, 7_500, 11_500, 15_500, 19_500, 23_500, 27_500, 31_500] {
        elapse_ms(repeat_at - at).await;
        inject_answer(&stack, &invite, 200, "OK").await;
        elapse_ms(20).await;
        at = repeat_at + 20;
        assert_eq!(
            ack_branches(&stack.drain_peer()),
            first,
            "the 2xx repeat {repeat_at} ms after the first draws the same ACK"
        );
    }
    assert_eq!(finals_handed_up(&stack.drain_events(), 200), 0, "an orphan's 2xx has no consumer");

    elapse_ms(TIMER_M - at - 100).await;
    assert_eq!(active(&stack), 1, "Timer M still holds it");
    elapse_ms(200).await;
    assert_eq!(stack.txn.metrics().orphaned_transactions(), 0, "Timer M purges the orphan");
    assert_eq!(active(&stack), 0);
    assert_eq!(stack.txn.metrics().sweep_reaped(), 0, "by its own timer");
}
