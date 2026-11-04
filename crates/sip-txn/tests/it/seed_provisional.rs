//! A server INVITE seed carrying the latest provisional its record holds
//! (ADR-0055 §2): the record may be ahead of the wire, so the provisional
//! leaves once at the seed, and a copy of the INVITE draws it again (RFC 3261
//! §17.2.1).

use crate::common::*;
use sip_txn::{TransactionEvent, TxnSeed};

const TRANSIT: u64 = 5;
const CALL_REF: &str = "w0|seeded-call@10.0.0.9|atag";

/// The seed's 180 leaves once at the seed toward the top Via's response
/// target, no timer repeats it, and a copy of the INVITE draws it again with
/// nothing surfacing.
#[tokio::test(start_paused = true)]
async fn a_server_seeds_provisional_leaves_once_and_answers_a_copy() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-seed-180";
    let call_id = "seed-180";
    let ringing = parse_response(&response_bytes(180, "Ringing", "INVITE", branch, call_id, true));
    let seed = TxnSeed::ServerInvite {
        request: request_of(&inbound_request("INVITE", branch, call_id, None)),
        to_tag: Some("peer-tag".to_string()),
        provisional: Some(Box::new(ringing)),
        leg_id: Some("a".to_string()),
    };
    assert_eq!(stack.txn.seed(CALL_REF, vec![seed]).await.unwrap(), 1);
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 180), 1, "the 180 leaves at the seed");
    elapse_ms(5_000).await;
    assert!(stack.drain_peer().is_empty(), "nothing repeats a provisional on a timer");

    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 180), 1, "a copy of the INVITE draws the 180");
    assert_eq!(count_responses(&out, 100), 0, "not a 100");
    let surfaced = stack
        .drain_events()
        .into_iter()
        .filter(|e| matches!(e, TransactionEvent::Message { .. }))
        .count();
    assert_eq!(surfaced, 0, "the copy surfaces nothing");
}
