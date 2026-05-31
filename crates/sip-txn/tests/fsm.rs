//! Native FSM coverage for behaviours the source only exercises through full
//! B2BUA scenarios (which depend on the unported call + rules layers). These
//! pin the RFC 3261 §17 mechanics directly at the transaction-layer seam:
//! retransmission cadence, Timer B timeout, CANCEL→200+487, ACK absorption,
//! auto-ACK for non-2xx client finals, and cached-response retransmission.
//!
//! Authored (not migrated) — see MIGRATION_STATUS §Slice 4.

mod common;
use common::*;
use sip_message::SipMessage;
use sip_txn::{TransactionEvent, TxnKind};

const TRANSIT: u64 = 5;

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

fn has_message_request(events: &[TransactionEvent], method: &str) -> bool {
    events.iter().any(|e| match e {
        TransactionEvent::Message { message, .. } => {
            matches!(message.as_ref(), SipMessage::Request(r) if r.method == method)
        }
        _ => false,
    })
}

// ── Client retransmission (Timer A) ─────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn client_retransmits_on_timer_a_cadence() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-rtx"), addr(PEER), TxnKind::Invite)
        .await;

    // By 2 s the peer has seen the initial send + retransmits at 500 ms and
    // 1500 ms (the source's doubling cadence) = 3 INVITEs.
    elapse_ms(2_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "INVITE"), 3);
}

#[tokio::test(start_paused = true)]
async fn provisional_response_stops_retransmit() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-prov";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await;

    elapse_ms(700).await; // initial + retransmit @500
    assert_eq!(count_requests(&stack.drain_peer(), "INVITE"), 2);

    // A 100 Trying on the matching branch cancels the retransmit timer.
    stack
        .inject(&response_bytes(100, "Trying", "INVITE", branch, "prov-call", false))
        .await;
    elapse_ms(3_000).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "INVITE"),
        0,
        "no retransmit after 100 Trying"
    );
}

// ── Client timeout (Timer B) ────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn timer_b_emits_timeout_event() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-tb"), addr(PEER), TxnKind::Invite)
        .await;

    // Timer B fires at 64·T1 = 32 s with no final response.
    elapse_ms(35_000).await;

    let method = stack.drain_events().into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { method, .. } => Some(method),
        _ => None,
    });
    assert_eq!(method, Some(Some("INVITE".to_string())));
    assert_eq!(active(&stack), 0, "timed-out txn is removed");
}

// ── CANCEL (server INVITE) ──────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn cancel_sends_200_and_487_and_emits_cancelled() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-cxl";

    stack
        .inject(&inbound_request("INVITE", branch, "cxl-call", None))
        .await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "100 Trying for INVITE");
    assert!(has_message_request(&stack.drain_events(), "INVITE"));

    stack
        .inject(&inbound_request("CANCEL", branch, "cxl-call", None))
        .await;
    elapse_ms(60).await;

    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 1, "200 OK to the CANCEL");
    assert_eq!(count_responses(&out, 487), 1, "487 Request Terminated on the INVITE");
    assert!(
        stack
            .drain_events()
            .iter()
            .any(|e| matches!(e, TransactionEvent::Cancelled { .. })),
        "a Cancelled event is emitted"
    );
}

// ── ACK absorption (server INVITE) ──────────────────────────────────────────

async fn invite_then_final(stack: &mut Stack, branch: &str, call_id: &str, status: u16) {
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    let _ = stack.drain_events();
    assert_eq!(active(stack), 1);

    // The application sends the final response through its server txn.
    let resp = parse_response(&response_bytes(status, "Final", "INVITE", branch, call_id, true));
    stack.txn.send_response(resp, addr(PEER)).await;
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    assert_eq!(active(stack), 1, "completed txn pinned for Timer H");
}

#[tokio::test(start_paused = true)]
async fn ack_for_non_2xx_is_absorbed() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-ackn";
    invite_then_final(&mut stack, branch, "ackn-call", 480).await;

    stack
        .inject(&inbound_request("ACK", branch, "ackn-call", Some("peer-tag")))
        .await;
    elapse_ms(60).await;

    assert_eq!(active(&stack), 0, "ACK for non-2xx terminates the txn");
    assert!(
        !has_message_request(&stack.drain_events(), "ACK"),
        "ACK for non-2xx must NOT surface to the app"
    );
}

#[tokio::test(start_paused = true)]
async fn ack_for_2xx_passes_through() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-ack2";
    invite_then_final(&mut stack, branch, "ack2-call", 200).await;

    stack
        .inject(&inbound_request("ACK", branch, "ack2-call", Some("peer-tag")))
        .await;
    elapse_ms(60).await;

    assert_eq!(active(&stack), 0, "ACK for 2xx terminates the server txn");
    assert!(
        has_message_request(&stack.drain_events(), "ACK"),
        "ACK for 2xx is delivered to the app"
    );
}

// ── Auto-ACK for non-2xx (client INVITE) ────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn client_auto_acks_non_2xx_final() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-autoack";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await;
    elapse_ms(60).await;
    let _ = stack.drain_peer(); // the initial INVITE

    // Peer answers 480 — the transaction layer must ACK it hop-by-hop.
    stack
        .inject(&response_bytes(480, "Temporarily Unavailable", "INVITE", branch, "autoack-call", true))
        .await;
    elapse_ms(60).await;

    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "auto-ACK for non-2xx");
    let events = stack.drain_events();
    assert!(
        events.iter().any(|e| matches!(e,
            TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Response(r) if r.status == 480))),
        "the 480 still surfaces to the app"
    );
    assert_eq!(active(&stack), 0, "client txn removed on final response");
}

// ── Duplicate request → cached-response retransmit ──────────────────────────

#[tokio::test(start_paused = true)]
async fn duplicate_request_retransmits_cached_response() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-dup";

    stack.inject(&inbound_request("OPTIONS", branch, "dup-call", None)).await;
    elapse_ms(60).await;
    assert!(has_message_request(&stack.drain_events(), "OPTIONS"));

    // App answers 200 — cached on the server txn.
    let resp = parse_response(&response_bytes(200, "OK", "OPTIONS", branch, "dup-call", true));
    stack.txn.send_response(resp, addr(PEER)).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);

    // The retransmitted OPTIONS must replay the cached 200, not re-surface.
    stack.inject(&inbound_request("OPTIONS", branch, "dup-call", None)).await;
    elapse_ms(60).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 200),
        1,
        "cached 200 retransmitted for the duplicate"
    );
    assert!(
        stack.drain_events().is_empty(),
        "duplicate request must not surface a second time"
    );
}
