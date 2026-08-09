//! RFC 3261 §9.1 — a CANCEL for an INVITE client transaction that has received
//! NO response is held by the layer and flushed on the first provisional; a
//! transaction that instead takes a final or dies at Timer B owes no CANCEL, so
//! the held datagram is dropped. `send_request` is the seam: the caller emits
//! the CANCEL eagerly, the client transaction decides when (whether) it goes on
//! the wire.

mod common;
use common::*;
use sip_txn::{TransactionEvent, TxnKind};

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

    // CANCEL requested while the branch is response-less — held, not sent.
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(100).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        0,
        "no CANCEL may reach the wire before the first provisional (§9.1)"
    );
    assert_eq!(stack.txn.metrics().cancels_held(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);

    // First provisional (>100) — the held CANCEL flushes.
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 0);

    // Complete the flow: the callee answers the CANCELled INVITE with 487; the
    // layer auto-ACKs it (§17.1.1.3) and surfaces the final once.
    stack
        .inject(&response_bytes(487, "Request Terminated", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
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

    // Terminate the flow with the 487 → auto-ACK.
    stack
        .inject(&response_bytes(487, "Request Terminated", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
}

#[tokio::test(start_paused = true)]
async fn held_cancel_is_dropped_at_timer_b() {
    let mut stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-hold-tb";

    // In-dialog re-INVITE (To-tag present) — 32 s Timer B, the cheap variant of
    // the same client-txn state machine.
    stack
        .txn
        .send_request(outbound_reinvite(branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .txn
        .send_request(reinvite_cancel(branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    assert_eq!(stack.txn.metrics().cancels_held(), 1);

    // The peer never responds: Timer B (32 s) kills the transaction. A dead
    // transaction is owed no CANCEL — the held one is dropped, and the caller
    // hears the Timeout that tears the leg down.
    elapse_ms(35_000).await;
    let msgs = stack.drain_peer();
    assert!(count_requests(&msgs, "INVITE") >= 1, "Timer A retransmits ran");
    assert_eq!(
        count_requests(&msgs, "CANCEL"),
        0,
        "a held CANCEL must never surface after Timer B"
    );
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);
    assert!(
        stack
            .drain_events()
            .iter()
            .any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "Timer B timeout still surfaces to the caller"
    );
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
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
    // transaction (auto-ACKed), so the held CANCEL is moot and dropped.
    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    let msgs = stack.drain_peer();
    assert_eq!(count_requests(&msgs, "ACK"), 1, "non-2xx final auto-ACKed");
    assert_eq!(count_requests(&msgs, "CANCEL"), 0);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);
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

    // Terminate the flow with the 487 → auto-ACK.
    stack
        .inject(&response_bytes(487, "Request Terminated", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
}
