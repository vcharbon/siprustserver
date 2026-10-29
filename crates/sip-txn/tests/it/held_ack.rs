//! The ACK of a client INVITE's first non-2xx final under a consumer that
//! releases it (`ack_bound_ms`, RFC 3261 §17.1.1.2): held until released or
//! until its bound passes, a copy of the final absorbed meanwhile and
//! re-ACKed once the ACK left; and the seed of an INVITE that took such a
//! final, which ACKs each copy the callee repeats.

use crate::common::{self, *};
use call::TxnKind;
use sip_txn::{TransactionConfig, TxnSeed};

const TRANSIT: u64 = 5;
const BOUND: u64 = 200;

async fn holding() -> Stack {
    let config = TransactionConfig { ack_bound_ms: BOUND, ..common::config() };
    Stack::build_with_config(TRANSIT, 64, config).await
}

async fn rejected(stack: &Stack, branch: &str) {
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;
    let _ = stack.drain_peer();
    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
}

#[tokio::test(start_paused = true)]
async fn a_held_ack_leaves_on_release_and_a_repeat_meanwhile_draws_none() {
    let mut stack = holding().await;
    let branch = "z9hG4bK-held-release";
    rejected(&stack, branch).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 0, "the ACK is held");
    assert!(!stack.drain_events().is_empty(), "the final went up");

    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 0, "a repeat while held draws none");

    assert!(stack.txn.release_ack(branch).await.unwrap(), "the release sends it");
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "one ACK");
    assert!(!stack.txn.release_ack(branch).await.unwrap(), "a second release sends nothing");

    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "a later repeat is re-ACKed");
}

#[tokio::test(start_paused = true)]
async fn a_held_ack_nobody_releases_leaves_at_its_bound() {
    let stack = holding().await;
    let branch = "z9hG4bK-held-bound";
    rejected(&stack, branch).await;
    elapse_ms(BOUND - 60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 0, "held inside the bound");
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "sent at the bound");
}

#[tokio::test(start_paused = true)]
async fn the_default_layer_acks_a_non_2xx_as_it_arrives() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-held-none";
    rejected(&stack, branch).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "ACKed at once");
}

#[tokio::test(start_paused = true)]
async fn a_rejected_invite_seed_acks_each_repeat_of_the_final_and_hands_none_up() {
    let mut stack = holding().await;
    let branch = "z9hG4bK-seed-rejected";
    let seed = TxnSeed::ClientRejected {
        invite: outbound_request("INVITE", branch),
        dest: addr(PEER),
        bound_ms: 1_000,
    };
    assert_eq!(stack.txn.seed("call-1", vec![seed]).await.unwrap(), 1);
    elapse_ms(20).await;
    assert!(stack.drain_peer().is_empty(), "nothing leaves at the seed");

    for _ in 0..2 {
        stack
            .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
            .await;
        elapse_ms(20).await;
        assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "each repeat is ACKed");
    }
    assert!(stack.drain_events().is_empty(), "no repeat goes up");
    elapse_ms(1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "held for its bound alone");
}
