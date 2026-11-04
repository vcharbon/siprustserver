//! An Accepted seed (RFC 6026 §7.1) whose identity a server INVITE with no
//! final already holds: the occupant takes the seed's 2xx as its final. One a
//! received copy of the INVITE opened answers that copy with it once; a seeded
//! one sends nothing, the 2xx's own retransmission ladder (RFC 3261
//! §13.3.1.4) being its only sender.

use crate::common::*;
use sip_message::parser::custom::CustomParser;
use sip_message::SipParser;
use sip_txn::{Reoffer, TxnSeed};

const TRANSIT: u64 = 5;
const CALL_REF: &str = "w0|seeded-call@10.0.0.9|atag";
/// The To-tag the dead node's provisional gave the caller's early dialog.
const SEED_TO_TAG: &str = "seed-to-tag";

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

/// A server INVITE seed for the a-leg INVITE the peer admitted on `branch`
/// and answered provisionally under `SEED_TO_TAG`, carrying the rebuilt
/// request.
fn server_seed(branch: &str, call_id: &str) -> TxnSeed {
    TxnSeed::ServerInvite {
        request: request_of(&inbound_request("INVITE", branch, call_id, None)),
        to_tag: Some(SEED_TO_TAG.to_string()),
        provisional: None,
        leg_id: Some("a".to_string()),
    }
}

/// An Accepted seed for the INVITE the peer answered 2xx on `branch`.
fn accepted_seed(branch: &str, call_id: &str) -> TxnSeed {
    TxnSeed::ServerAccepted {
        response: parse_response(&response_bytes(200, "OK", "INVITE", branch, call_id, true)),
        leg_id: Some("a".to_string()),
    }
}

/// The INVITE copy that triggered the materialisation opened a transaction
/// with no final on the Accepted seed's identity: it takes the seed's 2xx and
/// sends it once to the copy's source, and the re-offer of that copy is then
/// absorbed with nothing more sent. An occupant that has sent its final keeps
/// it, and the seed is skipped.
#[tokio::test(start_paused = true)]
async fn an_occupant_with_no_final_takes_the_accepted_seeds_2xx() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-seed-adopted", "seed-adopted");
    let raw = inbound_request("INVITE", branch, call_id, Some("peer-tag"));
    stack.inject(&raw).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the copy opened a transaction");
    let _ = stack.drain_events();

    let seeded = stack.txn.seed(CALL_REF, vec![accepted_seed(branch, call_id)]).await.unwrap();
    assert_eq!(seeded, 1, "the occupant took the seed");
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "the 2xx answers the copy once");
    let copy = CustomParser::new().parse(&raw).expect("parse");
    assert_eq!(stack.txn.reoffer(copy, addr(PEER)).await.unwrap(), Reoffer::Matched);
    elapse_ms(60).await;
    assert!(stack.drain_peer().is_empty(), "the re-offered copy draws nothing more");
    stack.inject(&raw).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "a copy from the wire draws it");
    assert_eq!(active(&stack), 1);

    // An occupant answered on its own keeps its final.
    let (other, other_id) = ("z9hG4bK-seed-answered", "seed-answered");
    stack.inject(&inbound_request("INVITE", other, other_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    let reject = parse_response(&response_bytes(486, "Busy Here", "INVITE", other, other_id, true));
    stack.txn.send_response(reject, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    let seeded = stack.txn.seed(CALL_REF, vec![accepted_seed(other, other_id)]).await.unwrap();
    assert_eq!(seeded, 0, "a final already sent outranks the seed");
    assert_eq!(stack.txn.metrics().txn_seed_skipped(), 1);
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 0, "no 2xx after the 486");
}

/// A server INVITE a seed rebuilt, with no final, is the occupant of an
/// Accepted seed's identity: it takes the seed's 2xx and sends nothing, the
/// 2xx's own retransmission ladder (RFC 3261 §13.3.1.4) being the only
/// sender; a copy of the INVITE from the wire then draws the 2xx.
#[tokio::test(start_paused = true)]
async fn a_seeded_occupant_takes_the_accepted_seeds_2xx_without_sending_it() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-seed-reseeded", "seed-reseeded");
    let seeded = stack.txn.seed(CALL_REF, vec![server_seed(branch, call_id)]).await.unwrap();
    assert_eq!(seeded, 1);
    elapse_ms(60).await;
    let _ = stack.drain_peer();

    let seeded = stack.txn.seed(CALL_REF, vec![accepted_seed(branch, call_id)]).await.unwrap();
    assert_eq!(seeded, 1, "the seeded occupant took the seed");
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 0, "the take sends nothing");
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "a copy from the wire draws it");
    assert_eq!(active(&stack), 1);
}
