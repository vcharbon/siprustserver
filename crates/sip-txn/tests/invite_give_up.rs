//! RFC 3261 §9.1 / §17.1.1.3 for an INVITE client transaction that gives up:
//! its TU abandons the INVITE with a CANCEL, which may provoke a 487. The
//! transaction is held 64·T1 past its `Timeout`, so the CANCEL still matches
//! it and the 487 is ACKed and handed up. It stops retransmitting at the
//! give-up and never times out a second time.

mod common;
use std::sync::Arc;

use common::*;
use sip_message::SipMessage;
use sip_txn::{IdGen, TimeoutKind, TransactionConfig, TransactionEvent, TxnKind};

const BOUND_MS: u64 = 40_000;

async fn stack() -> Stack {
    Stack::build_with_config(
        5,
        64,
        TransactionConfig {
            udp_queue_max: 64,
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_initial_timeout_ms: BOUND_MS,
            invite_first_response_timeout_ms: 5_000,
            ..Default::default()
        },
    )
    .await
}

fn timeouts(events: &[TransactionEvent]) -> Vec<TimeoutKind> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Timeout { kind, .. } => Some(*kind),
            _ => None,
        })
        .collect()
}

/// The `status` finals handed up, with whether the layer matched them.
fn finals(events: &[TransactionEvent], status: u16) -> Vec<bool> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, matched_client_txn, .. } => {
                match message.as_ref() {
                    SipMessage::Response(r) if r.status() == status => Some(*matched_client_txn),
                    _ => None,
                }
            }
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_ringing_invite_that_gave_up_acks_the_487_its_cancel_provokes() {
    let mut stack = stack().await;
    let branch = "z9hG4bK-gave-up-487";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(BOUND_MS + 100).await;
    assert_eq!(timeouts(&stack.drain_events()), vec![TimeoutKind::Transaction]);
    let _ = stack.drain_peer();

    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1);
    stack.inject(&response_bytes(200, "OK", "CANCEL", branch, "handle-shape-test", true)).await;
    stack
        .inject(&response_bytes(
            487,
            "Request Terminated",
            "INVITE",
            branch,
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;

    let out = stack.drain_peer();
    assert_eq!(count_requests(&out, "ACK"), 1, "the 487 is ACKed (RFC 3261 §17.1.1.3): {out:?}");
    assert_eq!(count_requests(&out, "CANCEL"), 0, "the CANCEL's answer ends its ladder");
    let events = stack.drain_events();
    assert_eq!(finals(&events, 487), vec![true], "the 487 is handed up as the INVITE's final");
    assert!(timeouts(&events).is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_invite_that_gave_up_unanswered_stops_retransmitting_and_never_times_out_again() {
    let mut stack = stack().await;
    let branch = "z9hG4bK-gave-up-silent";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(5_100).await;
    assert_eq!(timeouts(&stack.drain_events()), vec![TimeoutKind::Response]);
    let _ = stack.drain_peer();

    // The late provisional does not start a new bound on the given-up INVITE.
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(BOUND_MS).await;
    let out = stack.drain_peer();
    assert_eq!(count_requests(&out, "INVITE"), 0, "no Timer A re-send past the give-up: {out:?}");
    assert!(timeouts(&stack.drain_events()).is_empty(), "the give-up fires once");
}

#[tokio::test(start_paused = true)]
async fn an_invite_that_gave_up_is_forgotten_64_t1_later() {
    let mut stack = stack().await;
    let branch = "z9hG4bK-gave-up-forgotten";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(BOUND_MS + 100).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();

    elapse_ms(32_000).await;
    stack
        .inject(&response_bytes(
            487,
            "Request Terminated",
            "INVITE",
            branch,
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 0, "the transaction is gone");
    assert_eq!(finals(&stack.drain_events(), 487), vec![false], "the 487 matches nothing");
}
