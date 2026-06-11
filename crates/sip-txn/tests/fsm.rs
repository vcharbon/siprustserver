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

// ── Same-branch displacement releases the replaced txn's timers ─────────────

/// A second `send_request` reusing a live Via branch DISPLACES the first txn.
/// The displaced txn's retransmit/Timer-B entries must be physically removed in
/// lockstep — otherwise they linger in the shared `DelayQueue` keyed by the same
/// branch string and fire against the REPLACEMENT, forking its retransmit chain
/// (and, once their freed slab slots are reused, aliasing unrelated timers — the
/// CLAUDE.md no-generation `Key` hazard). Observable here as a doubled retransmit.
#[tokio::test(start_paused = true)]
async fn same_branch_displacement_does_not_fork_retransmits() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-displace";

    // First INVITE on `branch` → immediate send + retransmit armed @500 ms.
    stack
        .txn
        .send_request(
            invite_with_cr_lg("self|old", "cid-old", branch, "b-1"),
            addr(PEER),
            TxnKind::Invite,
        )
        .await;
    // Second INVITE reusing `branch` → displaces the first; its orphan retransmit
    // must be cancelled, not left to fire against this txn.
    stack
        .txn
        .send_request(
            invite_with_cr_lg("self|new", "cid-new", branch, "b-1"),
            addr(PEER),
            TxnKind::Invite,
        )
        .await;
    assert_eq!(active(&stack), 1, "displaced, not doubled");

    // Two initial sends + exactly ONE retransmit (the live txn's, @500 ms) by
    // 700 ms. A leaked orphan retransmit would double the @500 ms fire → 4.
    elapse_ms(700).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "INVITE"),
        3,
        "one retransmit chain, not a forked pair"
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

// ── #9: call_ref → txn-count index (acting-backup self-release gate) ─────────

/// An inbound in-dialog request whose Request-URI carries the `callref` param the
/// B2BUA's Contact stamps — the key the txn layer attributes the server txn to
/// (ADR-0014 self-release counting). `extract_ruri_call_ref` percent-decodes it,
/// so a plain value round-trips unchanged.
fn inbound_with_callref(method: &str, branch: &str, call_id: &str, call_ref: &str) -> Vec<u8> {
    format!(
        "{method} sip:b2bua@127.0.0.1:5070;callref={call_ref} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
         To: <sip:b2bua@127.0.0.1:5070>;tag=dlg-tag\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 {method}\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn drained_quiesced(stack: &mut Stack, call_ref: &str) -> bool {
    stack.drain_events().iter().any(|e| {
        matches!(e, TransactionEvent::CallQuiesced { call_ref: cr } if cr == call_ref)
    })
}

/// The `call_ref → txn-count` index stays in lockstep with the `txns` map, so the
/// acting-backup self-release gate is EXACT: two concurrent in-dialog txns for one
/// call count as 2, and the armed watch fires `CallQuiesced` only once BOTH clear
/// — never after just the first. This is the txn-layer guarantee the B2BUA leans
/// on to avoid shedding a takeover copy while a transaction is still in flight.
#[tokio::test(start_paused = true)]
async fn self_release_fires_only_after_the_last_call_txn_clears() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let cr = "w0z-cid|caller-tag";

    // Two in-dialog requests for the SAME call, distinct branches → 2 server txns.
    stack.inject(&inbound_with_callref("OPTIONS", "z9hG4bK-r1", "cid-sr", cr)).await;
    stack.inject(&inbound_with_callref("OPTIONS", "z9hG4bK-r2", "cid-sr", cr)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    assert_eq!(stack.txn.active_txn_count_for_call(cr).await, 2, "both txns counted");
    assert_eq!(
        stack.txn.active_txn_count_for_call("w0z-other|t").await,
        0,
        "a different call_ref is isolated in the index"
    );

    // Arm the watch while txns are live → NO immediate CallQuiesced.
    stack.txn.watch_self_release(cr).await;
    assert!(!drained_quiesced(&mut stack, cr), "must not fire while txns are in flight");

    // Clear the FIRST txn (200 → non-INVITE server Timer J eviction). Count → 1.
    let r1 = parse_response(&response_bytes(200, "OK", "OPTIONS", "z9hG4bK-r1", "cid-sr", true));
    stack.txn.send_response(r1, addr(PEER)).await;
    elapse_ms(33_000).await; // past TIMER_J (64*T1 = 32 s)
    assert_eq!(stack.txn.active_txn_count_for_call(cr).await, 1, "one txn cleared");
    assert!(!drained_quiesced(&mut stack, cr), "one txn still live → no self-release");

    // Clear the SECOND: the last `delete_txn` for the call fires CallQuiesced.
    let r2 = parse_response(&response_bytes(200, "OK", "OPTIONS", "z9hG4bK-r2", "cid-sr", true));
    stack.txn.send_response(r2, addr(PEER)).await;
    elapse_ms(33_000).await;
    assert_eq!(stack.txn.active_txn_count_for_call(cr).await, 0, "index drained to 0");
    assert!(drained_quiesced(&mut stack, cr), "last txn cleared → CallQuiesced");
}

/// A FULL events queue must DEFER `CallQuiesced`, never destroy it. It is the
/// only self-release trigger a takeover copy gets, and the queue saturates
/// exactly when takeover copies exist (the post-failover storm) — the old
/// drop-newest `emit` stranded the copy double-serving until its 1 h
/// `GlobalDuration` backstop. The watch stays armed until the send lands, and
/// the `QuiescedRetry` tick re-offers once the consumer drains the backlog.
#[tokio::test(start_paused = true)]
async fn call_quiesced_survives_a_full_event_queue() {
    // udp_queue_max 16 → event-queue capacity max(64, 16*4) = 64.
    let mut stack = Stack::build(TRANSIT, 16, 64).await;
    let cr = "w0z-full|caller-tag";

    // Saturate the bounded events queue: 70 undrained inbound OPTIONS (each
    // emits one Message event; 65+ are drop-newest discarded — that class is
    // legitimately lossy, CallQuiesced is not).
    for i in 0..70 {
        stack
            .inject(&inbound_request(
                "OPTIONS",
                &format!("z9hG4bK-fq{i}"),
                &format!("cid-fq{i}"),
                None,
            ))
            .await;
    }
    elapse_ms(60).await;

    // The watch's immediate-fire path (no txns for `cr`) hits the full queue.
    stack.txn.watch_self_release(cr).await;

    // The backlog the consumer now drains does NOT contain the notice…
    assert!(
        !drained_quiesced(&mut stack, cr),
        "the full-queue instant must not have squeezed the notice in"
    );
    // …but the deferred delivery lands on the next retry tick.
    elapse_ms(150).await;
    assert!(
        drained_quiesced(&mut stack, cr),
        "deferred CallQuiesced is re-delivered once queue capacity returns"
    );
}
