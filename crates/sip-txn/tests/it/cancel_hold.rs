//! RFC 3261 §9.1 — a CANCEL for an INVITE client transaction that has
//! received NO response is held by the layer and goes on the wire on the
//! branch's first provisional, however late; with no provisional it never
//! goes, and the INVITE ends at Timer B. A final taking the transaction first
//! clears the hold unsent (§9.2 — cancellation moot). `send_request` is the
//! seam: the caller emits the CANCEL eagerly, the client transaction decides
//! when it goes on the wire.

use crate::common;
use call::TxnKind;
use common::*;
use sip_txn::TransactionEvent;

/// A CANCEL for the re-INVITE minted by [`outbound_reinvite`] (same branch /
/// Call-ID / tags — RFC 3261 §9.1 identity).
fn reinvite_cancel(branch: &str) -> sip_message::SipRequest {
    parse_request(&format!(
        "CANCEL sip:bob@192.0.2.20:5060 SIP/2.0\n\
         Via: SIP/2.0/UDP 127.0.0.1:15070;branch={branch}\n\
         Max-Forwards: 70\n\
         From: <sip:b2bua@127.0.0.1:15070>;tag=b2bua-tag\n\
         To: <sip:bob@192.0.2.20:5060>;tag=remote-bob\n\
         Call-ID: reinvite-test\n\
         CSeq: 2 CANCEL\n\
         Content-Length: 0\n\n"
    ))
}

/// A CANCEL sharing the branch of [`invite_with_cr_lg`].
fn cr_lg_cancel(
    call_ref: &str,
    call_id: &str,
    branch: &str,
    leg_id: &str,
) -> sip_message::SipRequest {
    parse_request(&format!(
        "CANCEL sip:bob@192.0.2.20:5060 SIP/2.0\n\
         Via: SIP/2.0/UDP 127.0.0.1:15071;branch={branch};cr={call_ref};lg={leg_id}\n\
         Max-Forwards: 70\n\
         From: <sip:b2bua@127.0.0.1:15071>;tag=b2bua-{leg_id}\n\
         To: <sip:bob@192.0.2.20:5060>\n\
         Call-ID: {call_id}\n\
         CSeq: 1 CANCEL\n\
         Content-Length: 0\n\n"
    ))
}

/// The callee answers the CANCELled INVITE with 487; the layer ACKs it
/// (§17.1.1.3).
async fn reject_487_and_expect_ack(stack: &Stack, branch: &str, call_id: &str) {
    stack.inject(&response_bytes(487, "Request Terminated", "INVITE", branch, call_id, true)).await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
}

#[tokio::test(start_paused = true)]
async fn cancel_for_responseless_invite_is_held_then_flushed_on_180() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-hold-180";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "INVITE"), 1);

    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(100).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        0,
        "no CANCEL reaches the wire before the first provisional (§9.1)"
    );
    assert_eq!(stack.txn.metrics().cancels_held(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);

    // The first provisional releases the held CANCEL, once.
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 0);

    // The callee answers the CANCEL (§9.2): its Timer-E ladder stops.
    stack.inject(&response_bytes(200, "OK", "CANCEL", branch, "handle-shape-test", true)).await;
    elapse_ms(5_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0, "one CANCEL, answered");

    reject_487_and_expect_ack(&stack, branch, "handle-shape-test").await;
}

#[tokio::test(start_paused = true)]
async fn held_cancel_flushes_on_100_trying() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-hold-100";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(100).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);

    // A bare 100 Trying is a provisional — it releases the held CANCEL too.
    stack
        .inject(&response_bytes(100, "Trying", "INVITE", branch, "handle-shape-test", false))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 1);

    reject_487_and_expect_ack(&stack, branch, "handle-shape-test").await;
}

#[tokio::test(start_paused = true)]
async fn a_held_cancel_waits_any_time_for_the_first_provisional() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-hold-late-180";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    // Twenty seconds of silence, Timer A still re-sending the INVITE: the
    // CANCEL stays held.
    elapse_ms(20_000).await;
    let msgs = stack.drain_peer();
    assert!(count_requests(&msgs, "INVITE") > 1, "Timer A runs beside the held CANCEL");
    assert_eq!(count_requests(&msgs, "CANCEL"), 0, "no provisional, no CANCEL (§9.1)");

    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 1);

    reject_487_and_expect_ack(&stack, branch, "handle-shape-test").await;
}

#[tokio::test(start_paused = true)]
async fn a_cancel_toward_a_branch_that_answers_nothing_never_reaches_the_wire() {
    let mut stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-silent-tb";

    // In-dialog re-INVITE (To-tag present) — 32 s Timer B.
    stack.txn.send_request(outbound_reinvite(branch), addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.send_request(reinvite_cancel(branch), addr(PEER), TxnKind::Invite).await.unwrap();
    assert_eq!(stack.txn.metrics().cancels_held(), 1);

    // The peer never responds: Timer B gives the INVITE up, and the held
    // CANCEL dies with the transaction 64·T1 later, never on the wire.
    elapse_ms(35_000).await;
    let msgs = stack.drain_peer();
    assert!(count_requests(&msgs, "INVITE") >= 1, "Timer A retransmits ran");
    assert!(
        stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "Timer B timeout surfaces to the caller"
    );
    elapse_ms(32_000).await;
    let msgs = [msgs, stack.drain_peer()].concat();
    assert_eq!(
        count_requests(&msgs, "CANCEL"),
        0,
        "a held CANCEL never goes without a provisional"
    );
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_held_past_timer_b_goes_on_a_late_provisional() {
    let mut stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-late-after-tb";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(33_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);
    assert!(
        stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "Timer B timeout surfaces to the caller"
    );

    // The given-up INVITE is held 64·T1: a provisional arriving in that hold
    // releases the CANCEL, and the 487 it provokes is ACKed.
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 1);

    reject_487_and_expect_ack(&stack, branch, "handle-shape-test").await;
}

#[tokio::test(start_paused = true)]
async fn eviction_keeps_the_held_cancel_on_the_orphan_until_its_provisional() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-evict-held";
    let (cr, call_id, lg) = ("call-evict-held", "evict-held-id", "leg-b");

    stack
        .txn
        .send_request(invite_with_cr_lg(cr, call_id, branch, lg), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .txn
        .send_request(cr_lg_cancel(cr, call_id, branch, lg), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;
    stack.drain_peer();

    // The eviction sends nothing: the CANCEL stays parked on the orphaned
    // INVITE, which still waits for the branch's first provisional (§9.1).
    stack.txn.cancel_txns_for_call(cr).await.unwrap();
    elapse_ms(5_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 0);
    assert_eq!(stack.txn.metrics().orphaned_transactions(), 1);

    // The provisional releases it, on the orphan as on a live transaction.
    stack.inject(&response_bytes(180, "Ringing", "INVITE", branch, call_id, true)).await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 1);

    reject_487_and_expect_ack(&stack, branch, call_id).await;
}

#[tokio::test(start_paused = true)]
async fn held_cancel_is_dropped_when_a_final_arrives_first() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-hold-486";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;

    // The callee rejects before ever sending a provisional: the final ends the
    // transaction (auto-ACKed), cancellation is moot (§9.2).
    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    let msgs = stack.drain_peer();
    assert_eq!(count_requests(&msgs, "ACK"), 1, "non-2xx final auto-ACKed");
    assert_eq!(count_requests(&msgs, "CANCEL"), 0);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);

    elapse_ms(5_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);
}

#[tokio::test(start_paused = true)]
async fn cancel_after_provisional_passes_straight_through() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-nohold";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    stack.drain_peer();

    // The branch has a provisional — the CANCEL goes out immediately, unheld.
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().cancels_held(), 0);

    reject_487_and_expect_ack(&stack, branch, "handle-shape-test").await;
}

#[tokio::test(start_paused = true)]
async fn cancel_racing_a_completed_final_is_suppressed() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-race-486";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;

    // The callee rejects with no provisional ever: the txn takes its final
    // (Completed, Timer-D hold) and the layer auto-ACKs it.
    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);

    // A CANCEL emitted after the final (the TU's turn raced the 486) is
    // suppressed — §9.1/§9.2: the UAS already answered.
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        0,
        "a CANCEL for a Completed INVITE client txn never reaches the wire"
    );
    assert_eq!(stack.txn.metrics().cancels_suppressed_on_final(), 1);
    assert_eq!(stack.txn.metrics().cancels_held(), 0);
}

/// A 2xx with no provisional ends the INVITE client txn; a CANCEL the
/// TU emits after it (the TU had not yet processed the 2xx) must not reach the
/// wire (RFC 3261 §9.1: no provisional was received, and a final was).
#[tokio::test(start_paused = true)]
async fn a_cancel_after_a_bare_2xx_never_reaches_the_wire() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-bare-2xx";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;
    let _ = stack.drain_peer();
    stack.inject(&response_bytes(200, "OK", "INVITE", branch, "handle-shape-test", true)).await;
    elapse_ms(20).await;
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(100).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        0,
        "no CANCEL on a branch that drew no provisional and already took its 2xx"
    );
}
