//! RFC 3261 §9.1 bounded by ADR-0028 — a CANCEL for an INVITE client
//! transaction that has received NO response is held by the layer and flushed
//! on the first provisional; if the branch stays response-less past the grace
//! window (`cancel_hold_grace_ms`, default 1 s) the CANCEL is sent REGARDLESS
//! — the wait is a courtesy, never a veto. A grace-sent CANCEL is re-sent once
//! on a late first provisional (the UAS has its server txn by then); only a
//! txn that takes a final first clears the hold unsent (§9.2 — cancellation
//! moot). `send_request` is the seam: the caller emits the CANCEL eagerly, the
//! client transaction decides when it goes on the wire.

mod common;
use common::*;
use sip_txn::timers::CANCEL_HOLD_GRACE;
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

/// A CANCEL sharing the branch of [`invite_with_cr_lg`] (evict-flush test).
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
        "no CANCEL may reach the wire inside the grace window before the first provisional (§9.1)"
    );
    assert_eq!(stack.txn.metrics().cancels_held(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);

    // First provisional (>100), inside the grace window — the held CANCEL
    // flushes exactly once, on the provisional (no later grace-expiry copy).
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 0);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 0);

    // The callee answers the CANCEL (§9.2) — its Timer-E ladder goes
    // quiescent, so the only datagram this window could show is a grace copy.
    stack.inject(&response_bytes(200, "OK", "CANCEL", branch, "handle-shape-test", true)).await;
    elapse_ms(20).await;

    // Cross the (disarmed) grace deadline: no second CANCEL may appear.
    elapse_ms(CANCEL_HOLD_GRACE + 100).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        0,
        "the provisional flush disarms the grace timer"
    );

    // Complete the flow: the callee answers the CANCELled INVITE with 487; the
    // layer auto-ACKs it (§17.1.1.3) and surfaces the final once.
    stack
        .inject(&response_bytes(
            487,
            "Request Terminated",
            "INVITE",
            branch,
            "handle-shape-test",
            true,
        ))
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
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 0);

    // Terminate the flow with the 487 → auto-ACK.
    stack
        .inject(&response_bytes(
            487,
            "Request Terminated",
            "INVITE",
            branch,
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
}

#[tokio::test(start_paused = true)]
async fn held_cancel_is_sent_at_grace_expiry_when_no_response_ever() {
    let mut stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-grace-tb";

    // In-dialog re-INVITE (To-tag present) — 32 s Timer B, the cheap variant of
    // the same client-txn state machine.
    stack.txn.send_request(outbound_reinvite(branch), addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.send_request(reinvite_cancel(branch), addr(PEER), TxnKind::Invite).await.unwrap();
    assert_eq!(stack.txn.metrics().cancels_held(), 1);

    // Inside the grace window nothing goes out…
    elapse_ms(CANCEL_HOLD_GRACE - 200).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);

    // …then the grace expiry puts the CANCEL on the wire — the peer answered
    // NOTHING, and every emitted CANCEL reaches the callee (ADR-0028).
    elapse_ms(400).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        1,
        "the grace expiry sends the CANCEL toward a response-less branch"
    );
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 1);

    // The peer never responds at all: the grace copy rides the Timer-E ladder
    // (a lost CANCEL toward a silent callee is re-sent — §17.1.2.2) until
    // Timer B (32 s) kills the transaction, ladder included, and the caller
    // hears the Timeout — with no CANCEL dropped (it already made the wire).
    // Ladder fires at 0.5/1.5/3.5/7.5 s after the grace copy, then the T2
    // plateau: 9 re-sends before the txn dies at 32 s.
    elapse_ms(35_000).await;
    let msgs = stack.drain_peer();
    assert!(count_requests(&msgs, "INVITE") >= 1, "Timer A retransmits ran");
    assert_eq!(count_requests(&msgs, "CANCEL"), 9, "the ladder re-sends until the txn dies");
    assert_eq!(stack.txn.metrics().cancel_retransmits(), 9);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 0);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);
    assert!(
        stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "Timer B timeout still surfaces to the caller"
    );
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

#[tokio::test(start_paused = true)]
async fn grace_sent_cancel_is_resent_once_on_late_first_provisional() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-grace-reflush";

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

    // Grace expiry: the pre-1xx copy goes out (a UAS without the server txn
    // would 481 it).
    elapse_ms(CANCEL_HOLD_GRACE + 100).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 1);

    // The late first provisional re-releases it ONCE — the UAS has built its
    // server transaction by now, so this is the matchable send.
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_reflushed(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0, "re-send, not a fresh flush");

    // A SECOND provisional must not produce a third copy.
    stack
        .inject(&response_bytes(
            183,
            "Session Progress",
            "INVITE",
            branch,
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);

    // Terminate the flow with the 487 → auto-ACK.
    stack
        .inject(&response_bytes(
            487,
            "Request Terminated",
            "INVITE",
            branch,
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
}

#[tokio::test(start_paused = true)]
async fn held_cancel_is_flushed_when_its_call_is_evicted_inside_the_grace_window() {
    let stack = Stack::build(5, 64, 64).await;
    let branch = "z9hG4bK-evict-flush";
    let (cr, call_id, lg) = ("call-evict-flush", "evict-flush-id", "leg-b");

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

    // The call is evicted while the CANCEL still sits inside its grace window:
    // eviction flushes it to the wire first — teardown must not swallow a
    // CANCEL the callee is owed (ADR-0028).
    stack.txn.cancel_txns_for_call(cr).await.unwrap();
    elapse_ms(20).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "CANCEL"),
        1,
        "evicting the call sends the still-held CANCEL"
    );
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 0);
    assert_eq!(stack.txn.metrics().active_transactions(), 0);

    // The grace deadline crossing later is a no-op (txn gone).
    elapse_ms(CANCEL_HOLD_GRACE + 100).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);
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

    // The callee rejects (inside the grace window) before ever sending a
    // provisional: the final ends the transaction (auto-ACKed), cancellation
    // is moot (§9.2) — the ONLY path that clears a held CANCEL unsent.
    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    let msgs = stack.drain_peer();
    assert_eq!(count_requests(&msgs, "ACK"), 1, "non-2xx final auto-ACKed");
    assert_eq!(count_requests(&msgs, "CANCEL"), 0);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);

    // The disarmed grace deadline crossing later must not resurrect it.
    elapse_ms(CANCEL_HOLD_GRACE + 100).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);
}

#[tokio::test(start_paused = true)]
async fn held_cancel_is_flushed_when_the_timeout_outruns_the_grace_window() {
    // A grace window configured LONGER than the txn's give-up timer: Timer B
    // fires while the CANCEL is still held. The timeout death path must put it
    // on the wire first — no teardown may swallow an emitted CANCEL (ADR-0028).
    let mut stack = Stack::build_with_config(
        5,
        64,
        sip_txn::TransactionConfig {
            udp_queue_max: 64,
            id_gen: std::sync::Arc::new(sip_txn::IdGen::seeded(0xC0FFEE)),
            cancel_hold_grace_ms: Some(60_000),
            ..Default::default()
        },
    )
    .await;
    let branch = "z9hG4bK-tb-outruns-grace";

    // In-dialog re-INVITE (To-tag present) — 32 s Timer B, far below the grace.
    stack.txn.send_request(outbound_reinvite(branch), addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.send_request(reinvite_cancel(branch), addr(PEER), TxnKind::Invite).await.unwrap();
    assert_eq!(stack.txn.metrics().cancels_held(), 1);

    elapse_ms(35_000).await;
    let msgs = stack.drain_peer();
    assert_eq!(
        count_requests(&msgs, "CANCEL"),
        1,
        "Timer B outrunning the grace window still sends the held CANCEL"
    );
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 0);
    assert!(
        stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "Timer B timeout still surfaces to the caller"
    );
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

/// A stack configured for the literal RFC 3261 §9.1 wait
/// (`cancel_hold_grace_ms: None`) — the pre-amendment ADR-0028 behavior, kept
/// selectable (`B2BUA_CANCEL_STRICT_RFC_WAIT`) so both policy families stay
/// covered.
async fn strict_stack() -> Stack {
    Stack::build_with_config(
        5,
        64,
        sip_txn::TransactionConfig {
            udp_queue_max: 64,
            id_gen: std::sync::Arc::new(sip_txn::IdGen::seeded(0xC0FFEE)),
            cancel_hold_grace_ms: None,
            ..Default::default()
        },
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn strict_policy_holds_the_cancel_forever_and_drops_it_at_timer_b() {
    let mut stack = strict_stack().await;
    let branch = "z9hG4bK-strict-tb";

    // In-dialog re-INVITE (To-tag present) — 32 s Timer B.
    stack.txn.send_request(outbound_reinvite(branch), addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.send_request(reinvite_cancel(branch), addr(PEER), TxnKind::Invite).await.unwrap();
    assert_eq!(stack.txn.metrics().cancels_held(), 1);

    // No grace timer exists under the strict policy: the peer never responds,
    // Timer B (32 s) kills the transaction, and the held CANCEL dies with it —
    // it must NEVER surface on the wire (the literal §9.1 wait).
    elapse_ms(35_000).await;
    let msgs = stack.drain_peer();
    assert!(count_requests(&msgs, "INVITE") >= 1, "Timer A retransmits ran");
    assert_eq!(
        count_requests(&msgs, "CANCEL"),
        0,
        "strict §9.1: a held CANCEL never surfaces without a provisional"
    );
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 0);
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 0);
    assert!(
        stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "Timer B timeout still surfaces to the caller"
    );
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

#[tokio::test(start_paused = true)]
async fn strict_policy_still_flushes_on_the_first_provisional() {
    let stack = strict_stack().await;
    let branch = "z9hG4bK-strict-180";

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
    // Well past the (absent) grace window: still nothing on the wire.
    elapse_ms(CANCEL_HOLD_GRACE * 3).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);

    // The provisional-triggered flush is policy-independent.
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed(), 1);

    // Terminate the flow with the 487 → auto-ACK.
    stack
        .inject(&response_bytes(
            487,
            "Request Terminated",
            "INVITE",
            branch,
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
}

#[tokio::test(start_paused = true)]
async fn strict_policy_drops_the_held_cancel_on_call_evict() {
    let stack = strict_stack().await;
    let branch = "z9hG4bK-strict-evict";
    let (cr, call_id, lg) = ("call-strict-evict", "strict-evict-id", "leg-b");

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

    // Under the strict policy eviction keeps the old semantics: the held
    // CANCEL dies with the transaction, unsent.
    stack.txn.cancel_txns_for_call(cr).await.unwrap();
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 0);
    assert_eq!(stack.txn.metrics().held_cancels_dropped(), 1);
    assert_eq!(stack.txn.metrics().held_cancels_flushed_pre1xx(), 0);
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
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
        .inject(&response_bytes(
            487,
            "Request Terminated",
            "INVITE",
            branch,
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1);
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
    // suppressed — §9.1/§9.2: the UAS already answered, and sending it would
    // put a pre-1xx CANCEL on the wire. Not an always-send exception: the
    // final already resolved the leg, so no ring can persist.
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
