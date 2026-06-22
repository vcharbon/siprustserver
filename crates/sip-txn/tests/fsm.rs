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
        .await.unwrap();

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
        .await.unwrap();

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
        .await.unwrap();
    // Second INVITE reusing `branch` → displaces the first; its orphan retransmit
    // must be cancelled, not left to fire against this txn.
    stack
        .txn
        .send_request(
            invite_with_cr_lg("self|new", "cid-new", branch, "b-1"),
            addr(PEER),
            TxnKind::Invite,
        )
        .await.unwrap();
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

/// A non-INVITE client transaction CONTINUES retransmitting at T2 after a
/// provisional (RFC 3261 §17.1.2.2) — only INVITE stops. A 100 to a BYE must not
/// silence its Timer E, or a lost final is never re-elicited.
#[tokio::test(start_paused = true)]
async fn non_invite_keeps_retransmitting_after_provisional() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-bye-rtx";
    stack
        .txn
        .send_request(outbound_request("BYE", branch), addr(PEER), TxnKind::NonInvite)
        .await.unwrap();

    // A 100 Trying arrives early — must NOT stop the retransmit.
    stack
        .inject(&response_bytes(100, "Trying", "BYE", branch, "bye-rtx-call", false))
        .await;

    // By 2 s: initial + retransmits @500 ms and @1500 ms = 3 BYEs (Timer E runs on).
    elapse_ms(2_000).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "BYE"),
        3,
        "non-INVITE keeps retransmitting in Proceeding"
    );
}

/// A CANCEL fed through `send_request` reusing the INVITE's branch (RFC 3261
/// §9.1) is sent RAW and must NOT displace the live INVITE client txn at that
/// shared branch — no second, never-completing CANCEL txn is created.
#[tokio::test(start_paused = true)]
async fn send_request_cancel_is_raw_and_does_not_displace_the_invite() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-shared";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    assert_eq!(active(&stack), 1);
    elapse_ms(60).await;
    let _ = stack.drain_peer(); // the initial INVITE

    // CANCEL reusing the INVITE's branch through send_request → routed raw.
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::NonInvite)
        .await
        .unwrap();
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1, "CANCEL sent raw");
    assert_eq!(active(&stack), 1, "INVITE txn not displaced; no CANCEL txn created");

    // The INVITE's Timer-A retransmit still fires → its txn is intact, not displaced.
    elapse_ms(700).await;
    assert!(
        count_requests(&stack.drain_peer(), "INVITE") >= 1,
        "INVITE retransmit intact (the CANCEL did not displace it)"
    );
}

// ── Client timeout (Timer B) ────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn timer_b_emits_timeout_event() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    // An IN-DIALOG re-INVITE (To-tag present) keeps the 32 s Timer B.
    stack
        .txn
        .send_request(outbound_reinvite("z9hG4bK-tb"), addr(PEER), TxnKind::Invite)
        .await.unwrap();

    // Timer B fires at 64·T1 = 32 s with no final response.
    elapse_ms(35_000).await;

    // The Timeout event carries the method, the destination it was sent to, and
    // the discriminator (Timer B → Response) for the per-peer failure metric.
    let timeout = stack.drain_events().into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { method, destination, kind, .. } => {
            Some((method, destination, kind))
        }
        _ => None,
    });
    let (method, destination, kind) = timeout.expect("Timer B emits a Timeout event");
    assert_eq!(method, Some("INVITE".to_string()));
    assert_eq!(destination, Some(addr(PEER)), "Timeout forwards the txn destination");
    assert_eq!(kind, sip_txn::TimeoutKind::Response, "an in-dialog re-INVITE Timer B is a Response timeout");
    assert_eq!(active(&stack), 0, "timed-out txn is removed");
}

/// An INITIAL (out-of-dialog) INVITE must NOT expire at the 32 s Timer-B mark — a
/// callee may legitimately ring past it, and the upper layer's no-answer timer
/// (≤180 s) owns that deadline (a clean CANCEL→487). We keep only a hard backstop
/// at [`INVITE_INITIAL_TIMEOUT`] = 158 s (below the 180 s Timer-C mark), so the
/// no-answer always fires first and the 3-minute timer never beats us.
#[tokio::test(start_paused = true)]
async fn initial_invite_outlives_the_no_answer_window() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-init"), addr(PEER), TxnKind::Invite)
        .await.unwrap();

    // No Timeout at 35 s — still ringing.
    elapse_ms(35_000).await;
    assert!(
        !stack
            .drain_events()
            .iter()
            .any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "initial INVITE must not expire inside the ring window"
    );
    assert_eq!(active(&stack), 1, "still live, ringing");

    // The 158 s backstop eventually fires (total elapsed ~165 s, below 180 s).
    elapse_ms(130_000).await;
    let kind = stack.drain_events().into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { kind, .. } => Some(kind),
        _ => None,
    });
    assert_eq!(
        kind,
        Some(sip_txn::TimeoutKind::Transaction),
        "the initial-INVITE backstop fires below the 3-minute mark and is a Transaction timeout"
    );
    assert_eq!(active(&stack), 0);
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

/// RFC 3261 §9.2: a CANCEL matching no active INVITE server txn gets a 481 and
/// has NO effect — never a 200 + a spurious Cancelled that tears a call down.
#[tokio::test(start_paused = true)]
async fn unmatched_cancel_gets_481_and_emits_nothing() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack
        .inject(&inbound_request("CANCEL", "z9hG4bK-stray", "stray-call", None))
        .await;
    elapse_ms(60).await;

    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 481), 1, "unmatched CANCEL → 481");
    assert_eq!(count_responses(&out, 200), 0, "no 200 for an unmatched CANCEL");
    assert!(
        !stack
            .drain_events()
            .iter()
            .any(|e| matches!(e, TransactionEvent::Cancelled { .. })),
        "an unmatched CANCEL must not surface a Cancelled"
    );
}

/// A CANCEL arriving after the INVITE was answered (200 raced the CANCEL, then
/// the CANCEL retransmits) finds the server txn Completed — not active — so §9.2
/// applies: 481, NOT a 487 that would tear the established call down.
#[tokio::test(start_paused = true)]
async fn cancel_after_answer_does_not_tear_down_the_call() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-late-cxl";
    let call_id = "late-call";

    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();
    let resp = parse_response(&response_bytes(200, "OK", "INVITE", branch, call_id, true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();

    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 481), 1, "late CANCEL → 481");
    assert_eq!(count_responses(&out, 487), 0, "no 487 — the call was answered");
    assert!(
        !stack
            .drain_events()
            .iter()
            .any(|e| matches!(e, TransactionEvent::Cancelled { .. })),
        "no Cancelled for a CANCEL after answer"
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
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
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

/// A second final on an already-Completed server txn (a 200 racing the
/// autonomous 487, or a duplicate relayed final) must be DROPPED — not put a
/// second final with a different To-tag on the wire and flip the ACK classifier.
#[tokio::test(start_paused = true)]
async fn duplicate_final_on_completed_server_txn_is_dropped() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-dupfinal";
    let call_id = "dupfinal-call";

    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();

    // First final: 487 → Completed, classifier = non-2xx.
    let first = parse_response(&response_bytes(487, "Request Terminated", "INVITE", branch, call_id, true));
    stack.txn.send_response(first, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 487), 1);

    // Second, conflicting final: 200 → must be dropped (no wire, no state flip).
    let second = parse_response(&response_bytes(200, "OK", "INVITE", branch, call_id, true));
    stack.txn.send_response(second, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 0, "conflicting final dropped");
    assert_eq!(count_responses(&out, 487), 0, "no re-send");

    // The ACK for 487 is still absorbed as non-2xx (classifier intact, not 200).
    stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    assert!(
        !has_message_request(&stack.drain_events(), "ACK"),
        "ACK for 487 absorbed, not surfaced as a 2xx ACK"
    );
    assert_eq!(active(&stack), 0);
}

// ── Auto-ACK for non-2xx (client INVITE) ────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn client_auto_acks_non_2xx_final() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-autoack";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await.unwrap();
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
    // Held in Completed for Timer D (re-ACK/absorb window), not deleted on the spot.
    assert_eq!(active(&stack), 1, "client txn held in Completed for Timer D");
}

/// After ACKing a non-2xx INVITE final the client txn stays in Completed for
/// Timer D (32 s): a retransmitted final (our ACK was lost) is RE-ACKed and
/// absorbed — not re-surfaced — then the txn terminates at Timer D.
#[tokio::test(start_paused = true)]
async fn non_2xx_invite_final_absorbs_retransmits_for_timer_d() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-timerd";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();

    // First 486 → auto-ACK, surfaces once, txn held for Timer D.
    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "timerd-call", true))
        .await;
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "auto-ACK for the 486");
    assert_eq!(
        stack
            .drain_events()
            .iter()
            .filter(|e| matches!(e, TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Response(r) if r.status == 486)))
            .count(),
        1,
        "the 486 surfaces exactly once"
    );
    assert_eq!(active(&stack), 1, "held in Completed for Timer D");

    // Retransmitted 486 (first ACK lost) → RE-ACK, absorbed (no second Message).
    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "timerd-call", true))
        .await;
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "retransmitted 486 re-ACKed");
    assert!(stack.drain_events().is_empty(), "retransmitted final must not re-surface");

    // Timer D (32 s) fires → the client txn terminates.
    elapse_ms(33_000).await;
    assert_eq!(active(&stack), 0, "Timer D cleaned up the client txn");
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
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
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

// ── cancel_txns_for_call spares server txns (Timer-J absorption) ────────────

/// Tearing a call down (`cancel_txns_for_call`) must cancel only CLIENT txns —
/// a Completed SERVER txn keeps its Timer-J retransmit-absorption window so a
/// BYE retransmit after teardown replays the cached 200 instead of building a
/// fresh txn that 481s upstream (the unexpected-481-on-BYE wire signature).
#[tokio::test(start_paused = true)]
async fn cancel_txns_for_call_spares_server_timer_j_absorption() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let cr = "w0|bye-cr";
    let branch = "z9hG4bK-bye";

    // Inbound BYE attributed to the call (R-URI callref) → server txn.
    stack.inject(&inbound_with_callref("BYE", branch, "cid-bye", cr)).await;
    elapse_ms(60).await;
    assert!(has_message_request(&stack.drain_events(), "BYE"));

    // App answers 200 → server txn Completed, 200 cached, Timer J armed.
    let resp = parse_response(&response_bytes(200, "OK", "BYE", branch, "cid-bye", true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);

    // Call torn down — the server txn must SURVIVE this.
    stack.txn.cancel_txns_for_call(cr).await.unwrap();

    // Retransmitted BYE → cached 200 replayed, NOT re-surfaced to the app.
    stack.inject(&inbound_with_callref("BYE", branch, "cid-bye", cr)).await;
    elapse_ms(60).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 200),
        1,
        "cached 200 replayed for the BYE retransmit"
    );
    assert!(
        stack.drain_events().is_empty(),
        "retransmitted BYE must not re-surface (no orphan 481 path)"
    );
}

/// A one-shot Timeout must be DEFERRED, never dropped, when the events queue is
/// full — the post-failover storm saturates it exactly when reclaimed calls time
/// out, and a lost Timeout strands the leg until the 1 h GlobalDuration backstop.
#[tokio::test(start_paused = true)]
async fn timeout_survives_a_full_event_queue() {
    // udp_queue_max 16 → event-queue capacity max(64, 16*4) = 64; generous recv
    // queue so all injected OPTIONS reach the owner and fill the event queue.
    let mut stack = Stack::build(TRANSIT, 16, 1024).await;
    // An in-dialog client txn whose Timer B (32 s) will fire.
    stack
        .txn
        .send_request(outbound_reinvite("z9hG4bK-tofull"), addr(PEER), TxnKind::Invite)
        .await.unwrap();

    // Saturate the events queue with undrained inbound OPTIONS (lossy Messages).
    for i in 0..70 {
        stack
            .inject(&inbound_request(
                "OPTIONS",
                &format!("z9hG4bK-q{i}"),
                &format!("cid-q{i}"),
                None,
            ))
            .await;
    }
    elapse_ms(60).await;

    // Timer B fires (32 s) into the full queue → the Timeout must DEFER, not drop.
    elapse_ms(33_000).await;
    assert!(
        !stack
            .drain_events()
            .iter()
            .any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "the full-queue instant must have deferred the Timeout, not squeezed it in"
    );
    // Capacity returned (we just drained) → the retry tick redelivers it.
    elapse_ms(150).await;
    assert!(
        stack
            .drain_events()
            .iter()
            .any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "deferred Timeout redelivered once queue capacity returned"
    );
}

/// The auto-sent 100 Trying is cached, so a retransmitted INVITE replays it (RFC
/// 3261 §17.2.1) instead of being absorbed silently — the 100 already silenced
/// the UAC's own retransmit timer, so a black hole here fails the call.
#[tokio::test(start_paused = true)]
async fn retransmitted_invite_replays_the_cached_100() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-100cache";
    stack.inject(&inbound_request("INVITE", branch, "cid-100", None)).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "100 Trying for the INVITE");
    let _ = stack.drain_events();

    // Retransmitted INVITE (same branch) before any final → replays the cached 100.
    stack.inject(&inbound_request("INVITE", branch, "cid-100", None)).await;
    elapse_ms(60).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 100),
        1,
        "cached 100 replayed for the INVITE retransmit"
    );
    assert!(
        stack.drain_events().is_empty(),
        "retransmitted INVITE must not re-surface to the app"
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
    assert_eq!(stack.txn.active_txn_count_for_call(cr).await.unwrap(), 2, "both txns counted");
    assert_eq!(
        stack.txn.active_txn_count_for_call("w0z-other|t").await.unwrap(),
        0,
        "a different call_ref is isolated in the index"
    );

    // Arm the watch while txns are live → NO immediate CallQuiesced.
    stack.txn.watch_self_release(cr).await.unwrap();
    assert!(!drained_quiesced(&mut stack, cr), "must not fire while txns are in flight");

    // Clear the FIRST txn (200 → non-INVITE server Timer J eviction). Count → 1.
    let r1 = parse_response(&response_bytes(200, "OK", "OPTIONS", "z9hG4bK-r1", "cid-sr", true));
    stack.txn.send_response(r1, addr(PEER)).await.unwrap();
    elapse_ms(33_000).await; // past TIMER_J (64*T1 = 32 s)
    assert_eq!(stack.txn.active_txn_count_for_call(cr).await.unwrap(), 1, "one txn cleared");
    assert!(!drained_quiesced(&mut stack, cr), "one txn still live → no self-release");

    // Clear the SECOND: the last `delete_txn` for the call fires CallQuiesced.
    let r2 = parse_response(&response_bytes(200, "OK", "OPTIONS", "z9hG4bK-r2", "cid-sr", true));
    stack.txn.send_response(r2, addr(PEER)).await.unwrap();
    elapse_ms(33_000).await;
    assert_eq!(stack.txn.active_txn_count_for_call(cr).await.unwrap(), 0, "index drained to 0");
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
    stack.txn.watch_self_release(cr).await.unwrap();

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
