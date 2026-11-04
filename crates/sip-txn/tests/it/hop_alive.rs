//! An initial INVITE's first-response deadline (ADR-0044) met by a `100 Trying`
//! alone: the TU is told the hop is alive, once, at the deadline. A branch a
//! 1xx above 100 answered, or one nothing answered, never draws that notice.

use crate::common;
use std::sync::Arc;

use call::{TxnKind, TxnTimeoutKind};
use common::*;
use sip_txn::{IdGen, TransactionConfig, TransactionEvent};

const CALL_REF: &str = "w0|hop-alive|tag";
const CALL_ID: &str = "hop-alive@10.0.0.1";

async fn stack() -> Stack {
    Stack::build_with_config(
        5,
        64,
        TransactionConfig {
            udp_queue_max: 64,
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_first_response_timeout_ms: 5_000,
            ..common::config()
        },
    )
    .await
}

async fn dial(stack: &Stack, branch: &str) {
    stack
        .txn
        .send_request(
            invite_with_cr_lg(CALL_REF, CALL_ID, branch, "b-1"),
            addr(PEER),
            TxnKind::Invite,
        )
        .await
        .unwrap();
}

async fn answer(stack: &Stack, status: u16, reason: &str, branch: &str) {
    stack.inject(&response_bytes(status, reason, "INVITE", branch, CALL_ID, status > 100)).await;
}

fn hop_alive(events: &[TransactionEvent]) -> Vec<(String, String, String, Option<String>)> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::HopAlive { branch, call_ref, leg_id, incarnation_mark } => {
                Some((branch.clone(), call_ref.clone(), leg_id.clone(), incarnation_mark.clone()))
            }
            _ => None,
        })
        .collect()
}

fn timeouts(events: &[TransactionEvent]) -> Vec<TxnTimeoutKind> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Timeout { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_bare_100_is_reported_alive_at_the_deadline_once() {
    let mut stack = stack().await;
    dial(&stack, "z9hG4bK-alive").await;
    elapse_ms(1_000).await;
    answer(&stack, 100, "Trying", "z9hG4bK-alive").await;

    elapse_ms(3_900).await;
    assert!(hop_alive(&stack.drain_events()).is_empty(), "the deadline has not passed yet");

    elapse_ms(200).await;
    let events = stack.drain_events();
    assert_eq!(
        hop_alive(&events),
        vec![(
            "z9hG4bK-alive".to_string(),
            CALL_REF.to_string(),
            "b-1".to_string(),
            Some(INCARNATION_MARK.to_string())
        )],
        "the hop that answered only 100 is reported alive at the deadline"
    );
    assert!(timeouts(&events).is_empty(), "an alive hop is no dead hop");

    elapse_ms(60_000).await;
    let events = stack.drain_events();
    assert!(hop_alive(&events).is_empty(), "told once");
    assert!(timeouts(&events).is_empty(), "the long INVITE bound has not passed");
}

#[tokio::test(start_paused = true)]
async fn a_provisional_above_100_draws_no_alive_notice() {
    let mut stack = stack().await;
    dial(&stack, "z9hG4bK-rang").await;
    elapse_ms(500).await;
    answer(&stack, 100, "Trying", "z9hG4bK-rang").await;
    elapse_ms(500).await;
    answer(&stack, 180, "Ringing", "z9hG4bK-rang").await;

    elapse_ms(10_000).await;
    let events = stack.drain_events();
    assert!(hop_alive(&events).is_empty(), "the TU holds the 180: nothing to tell");
    assert!(timeouts(&events).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_silent_hop_is_reported_dead_not_alive() {
    let mut stack = stack().await;
    dial(&stack, "z9hG4bK-dead").await;

    elapse_ms(5_100).await;
    let events = stack.drain_events();
    assert!(hop_alive(&events).is_empty());
    assert_eq!(timeouts(&events), vec![TxnTimeoutKind::Response], "the hop is dead");
}
