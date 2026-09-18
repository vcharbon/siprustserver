//! `cancel_txns_for_call` releases a call's client transactions from the call
//! without cutting them short: each is orphaned and closes its own protocol
//! obligations (RFC 3261 §17.1.1.3 / §13.2.2.4 for a final it still draws) on
//! its own timers, so an evicted call leaves no peer stranded and no
//! transaction past its window.

mod common;
use common::*;
use sip_message::{SipMessage, SipRequest};
use sip_txn::timers::{TIMER_B, TIMER_D, TIMER_M};
use sip_txn::{TransactionEvent, TxnKind};

/// The ACKs among `msgs`, in wire order.
fn acks(msgs: Vec<SipMessage>) -> Vec<SipRequest> {
    msgs.into_iter()
        .filter_map(|m| match m {
            SipMessage::Request(r) if r.method() == "ACK" => Some(r),
            _ => None,
        })
        .collect()
}

fn timeout_call_refs(events: &[TransactionEvent]) -> Vec<Option<String>> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Timeout { call_ref, .. } => Some(call_ref.clone()),
            _ => None,
        })
        .collect()
}

/// An active INVITE outlives its call as an orphan: Timer A keeps repeating it
/// (§17.1.1.2 Calling — the request may not have arrived), Timer B purges it
/// with a `Timeout` naming no call, and nothing of it survives.
#[tokio::test(start_paused = true)]
async fn t1_an_evicted_active_invite_runs_to_its_own_timer_b() {
    let mut stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-T1";
    let invite = invite_with_cr_lg(call_ref, "callid-T1", "z9hG4bK-T1", "b-1");

    stack.txn.send_request(invite, addr("192.0.2.20:5060"), TxnKind::Invite).await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 1);

    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "orphaned, still resident");
    assert_eq!(stack.txn.metrics().orphaned_transactions(), 1);
    assert_eq!(stack.txn.metrics().txn_orphaned_on_call_evict(), 1);
    assert_eq!(stack.txn.active_txn_count_for_call(call_ref).await.unwrap(), 0);

    elapse_ms(TIMER_B + 100).await;
    assert_eq!(
        timeout_call_refs(&stack.drain_events()),
        vec![None],
        "Timer B purges the orphan with a Timeout attributed to no call"
    );
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
    assert_eq!(stack.txn.metrics().orphaned_transactions(), 0);
    assert_eq!(stack.txn.metrics().timer_queue_len(), 0);
}

/// A non-2xx final to an orphaned INVITE draws the hop ACK on its branch and
/// Timer D holds the orphan to re-ACK the repeats (§17.1.1.2); the final is
/// surfaced to nobody. Timer D from the first final purges it.
#[tokio::test(start_paused = true)]
async fn t6_an_orphaned_invite_acks_its_non_2xx_and_is_purged_at_timer_d() {
    let mut stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-T6";
    let branch = "z9hG4bK-T6";
    let invite = invite_with_cr_lg(call_ref, "callid-T6", branch, "b-1");
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    stack.drain_peer();

    stack
        .inject(&response_bytes(487, "Request Terminated", "INVITE", branch, "callid-T6", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "the hop ACK");
    assert!(stack.drain_events().is_empty(), "an orphan's final has no consumer");

    stack
        .inject(&response_bytes(487, "Request Terminated", "INVITE", branch, "callid-T6", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "the repeat is re-ACKed");
    assert_eq!(stack.txn.metrics().orphaned_transactions(), 1);

    elapse_ms(TIMER_D + 100).await;
    assert_eq!(stack.txn.metrics().orphaned_transactions(), 0, "purged at Timer D");
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
    assert!(stack.drain_events().is_empty());
}

/// A 2xx to an orphaned INVITE draws the bare ACK the vanished UAC core owed
/// (§13.2.2.4), on a fresh branch, and Timer M (RFC 6026 §7.2) holds the
/// orphan to re-pass that same ACK to each repeat of the 2xx.
#[tokio::test(start_paused = true)]
async fn t7_an_orphaned_invite_acks_its_2xx_and_is_purged_at_timer_m() {
    let mut stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-T7";
    let branch = "z9hG4bK-T7";
    let invite = invite_with_cr_lg(call_ref, "callid-T7", branch, "b-1");
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.unwrap();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    stack.drain_peer();

    stack.inject(&response_bytes(200, "OK", "INVITE", branch, "callid-T7", true)).await;
    elapse_ms(20).await;
    let first = acks(stack.drain_peer());
    assert_eq!(first.len(), 1, "the bare ACK");
    let ack_branch = first[0].top_via().branch().unwrap().to_string();
    assert_ne!(ack_branch, branch, "§17.1.1.3: the 2xx ACK is its own transaction");
    assert!(stack.drain_events().is_empty(), "an orphan's 2xx has no consumer");

    stack.inject(&response_bytes(200, "OK", "INVITE", branch, "callid-T7", true)).await;
    elapse_ms(20).await;
    let again = acks(stack.drain_peer());
    assert_eq!(again.len(), 1, "the repeat is re-ACKed");
    assert_eq!(again[0].top_via().branch().unwrap(), ack_branch, "the same ACK, re-passed");
    assert_eq!(stack.txn.metrics().orphaned_transactions(), 1);

    elapse_ms(TIMER_M + 100).await;
    assert_eq!(stack.txn.metrics().orphaned_transactions(), 0, "purged at Timer M");
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
}

#[tokio::test(start_paused = true)]
async fn t2_cancel_is_idempotent() {
    let stack = Stack::build(5, 64, 64).await;
    let call_ref = "self|call-T2";
    let invite = invite_with_cr_lg(call_ref, "callid-T2", "z9hG4bK-T2", "b-1");

    stack.txn.send_request(invite, addr("192.0.2.20:5060"), TxnKind::Invite).await.unwrap();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    let first = stack.txn.metrics().txn_orphaned_on_call_evict();
    stack.txn.cancel_txns_for_call(call_ref).await.unwrap();
    assert_eq!(stack.txn.metrics().txn_orphaned_on_call_evict(), first);
    assert_eq!(stack.txn.metrics().orphaned_transactions(), 1);
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
        .await
        .unwrap();
    stack
        .txn
        .send_request(
            invite_with_cr_lg(ref_b, "callid-B", "z9hG4bK-B", "b-1"),
            addr("192.0.2.21:5060"),
            TxnKind::Invite,
        )
        .await
        .unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 2);

    stack.txn.cancel_txns_for_call(ref_a).await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 2, "A is orphaned, not gone");
    assert_eq!(stack.txn.active_txn_count_for_call(ref_a).await.unwrap(), 0);
    assert_eq!(stack.txn.active_txn_count_for_call(ref_b).await.unwrap(), 1);

    // Drive the initial-INVITE backstop (158 s) both were sent under: A's
    // orphan times out naming no call, B naming B.
    elapse_ms(160_000).await;

    let mut refs = timeout_call_refs(&stack.drain_events());
    refs.sort();
    assert_eq!(refs, vec![None, Some(ref_b.to_string())], "A's orphan and B, each its own");
    assert_eq!(stack.txn.metrics().active_transactions(), 0);
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

    stack.txn.send_request(invite, addr("192.0.2.20:5060"), TxnKind::Invite).await.unwrap();
    assert_eq!(stack.txn.metrics().active_transactions(), 1);

    // Caller passes the natural (decoded) callRef.
    stack.txn.cancel_txns_for_call(decoded).await.unwrap();
    assert_eq!(stack.txn.active_txn_count_for_call(decoded).await.unwrap(), 0);
    assert!(stack.txn.metrics().txn_orphaned_on_call_evict() >= 1);
}
