//! Port of `tests/sip/transaction-layer-cancel-on-evict.test.ts` —
//! `cancel_txns_for_call` tears down a call's client retransmit + Timer B/F so
//! an evicted call can't leave orphan timers firing minutes later.
//!
//! T4 (CallState.remove drives the cancel) is **not ported**: it exercises the
//! call layer's eviction path (`CallState`), which is a later slice. The
//! transaction-layer behaviour it relies on is covered directly by T1–T3, T5.

mod common;
use common::*;
use sip_txn::{TransactionEvent, TxnKind};

fn timeout_call_refs(events: &[TransactionEvent]) -> Vec<Option<String>> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Timeout { call_ref, .. } => Some(call_ref.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn t1_cancellation_prevents_timer_b_timeout() {
    let mut stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-T1";
    let invite = invite_with_cr_lg(call_ref, "callid-T1", "z9hG4bK-T1", "b-1");

    stack
        .txn
        .send_request(invite, addr("192.0.2.20:5060"), TxnKind::Invite)
        .await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 1);

    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
    assert_eq!(stack.txn.metrics().txn_cancelled_on_call_evict(), 1);

    // Advance well past Timer B (32 s). A live timer would have fired.
    elapse_ms(35_000).await;
    assert!(
        timeout_call_refs(&stack.drain_events()).is_empty(),
        "cancelled transaction must not emit a timeout"
    );
}

#[tokio::test(start_paused = true)]
async fn t2_cancel_is_idempotent() {
    let stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-T2";
    let invite = invite_with_cr_lg(call_ref, "callid-T2", "z9hG4bK-T2", "b-1");

    stack
        .txn
        .send_request(invite, addr("192.0.2.20:5060"), TxnKind::Invite)
        .await.unwrap();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    let first = stack.txn.metrics().txn_cancelled_on_call_evict();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    assert_eq!(stack.txn.metrics().txn_cancelled_on_call_evict(), first);
}

#[tokio::test(start_paused = true)]
async fn t3_cancel_targets_only_the_owning_callref() {
    let mut stack = Stack::build(5, 64, 64).await;
    let ref_a = "self|call-A";
    let ref_b = "self|call-B";

    stack
        .txn
        .send_request(
            invite_with_cr_lg(ref_a, "callid-A", "z9hG4bK-A", "b-1"),
            addr("192.0.2.20:5060"),
            TxnKind::Invite,
        )
        .await.unwrap();
    stack
        .txn
        .send_request(
            invite_with_cr_lg(ref_b, "callid-B", "z9hG4bK-B", "b-1"),
            addr("192.0.2.21:5060"),
            TxnKind::Invite,
        )
        .await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 2);

    stack.txn.cancel_txns_for_call(ref_a).await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 1);

    // Drive the initial-INVITE backstop for call B (158 s) — only B should time
    // out (these are out-of-dialog INVITEs, so they get the long expiry).
    elapse_ms(160_000).await;

    let refs = timeout_call_refs(&stack.drain_events());
    assert_eq!(refs.len(), 1, "exactly one timeout (call B)");
    assert_eq!(refs[0].as_deref(), Some(ref_b));
}

#[tokio::test(start_paused = true)]
async fn t5_url_encoded_cr_lg_round_trip_matches_decoded_callref() {
    // Production `buildCallVia` URL-encodes `cr=` (callRefs contain `|`/`@`);
    // the parser stores Via params raw. Pre-fix the cancel matched the encoded
    // string against the decoded callRef the caller passes — a silent no-op.
    let stack = Stack::build(5, 64, 64).await;
    let decoded = "worker-0|UUID-1234@5.1.1.1|tag";
    let encoded = "worker-0%7CUUID-1234%405.1.1.1%7Ctag"; // encodeURIComponent
    let invite = invite_with_cr_lg(encoded, "callid-T5", "z9hG4bK-T5", "b-1");

    stack
        .txn
        .send_request(invite, addr("192.0.2.20:5060"), TxnKind::Invite)
        .await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 1);

    // Caller passes the natural (decoded) callRef.
    stack.txn.cancel_txns_for_call(decoded).await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
    assert!(stack.txn.metrics().txn_cancelled_on_call_evict() >= 1);
}
