//! A transaction no final or give-up ended still leaves the map, at the
//! backstop it took on entering it: the INVITE bound plus `TXN_MAX_AGE` for
//! an INVITE, `TXN_MAX_AGE` for a non-INVITE, measured from its admission or
//! its seed. Its own cleanup timer removes it there, so the safety-net sweep
//! reaps nothing.

use std::sync::Arc;

use crate::common;
use common::*;
use sip_txn::timers::TXN_MAX_AGE;
use sip_txn::{IdGen, TransactionConfig, TxnSeed};

const TRANSIT: u64 = 5;
const BOUND_MS: u64 = 1_000;

async fn stack() -> Stack {
    Stack::build_with_config(
        TRANSIT,
        64,
        TransactionConfig {
            udp_queue_max: 64,
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_initial_timeout_ms: BOUND_MS,
            ..Default::default()
        },
    )
    .await
}

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

/// An OPTIONS the consumer never answers: its server transaction absorbs the
/// retransmissions until `TXN_MAX_AGE` after its admission, then leaves.
#[tokio::test(start_paused = true)]
async fn an_unanswered_non_invite_server_transaction_leaves_at_its_backstop() {
    let mut stack = stack().await;
    // Off the sweep's 10 s grid, so the cleanup timer, not a sweep tick,
    // is what removes it.
    elapse_ms(3_000).await;
    stack.inject(&inbound_request("OPTIONS", "z9hG4bK-unanswered", "unanswered", None)).await;
    elapse_ms(TRANSIT + TXN_MAX_AGE - 100).await;
    let _ = stack.drain_events();
    assert_eq!(active(&stack), 1, "the backstop has not passed");

    elapse_ms(200).await;
    assert_eq!(active(&stack), 0, "the backstop removes it");
    assert_eq!(stack.txn.metrics().sweep_reaped(), 0, "by its own timer");
}

/// A server INVITE seeded 3 s after the layer started, holding no request to
/// answer a CANCEL with, and never answered: its backstop runs from the seed.
#[tokio::test(start_paused = true)]
async fn a_seeded_server_invite_leaves_at_its_backstop_measured_from_the_seed() {
    let stack = stack().await;
    elapse_ms(3_000).await;
    let seed = TxnSeed::ServerInvite {
        branch: "z9hG4bK-seeded-unanswered".to_string(),
        sent_by: peer_sent_by(),
        call_id: "seeded-unanswered".to_string(),
        from_tag: "caller-tag".to_string(),
        to_tag: None,
        leg_id: None,
        original_request: None,
    };
    assert_eq!(stack.txn.seed("w0|seeded-unanswered|atag", vec![seed]).await.unwrap(), 1);

    elapse_ms(BOUND_MS + TXN_MAX_AGE - 100).await;
    assert_eq!(active(&stack), 1, "the backstop has not passed");
    elapse_ms(200).await;
    assert_eq!(active(&stack), 0, "the backstop removes it");
    assert_eq!(stack.txn.metrics().sweep_reaped(), 0, "by its own timer");
}
