//! An INVITE's first provisional (RFC 3261 §17.2.1): the server transaction
//! sends its 100 Trying when its consumer releases it, or itself at the
//! §17.2.1 bound, and never after another response. A 100 that leaves after
//! the INVITE arrived states the delay in its `Timestamp` (§8.2.6.1). Under
//! `consumer_arms_initial_trying` an initial INVITE's bound runs from its
//! consumer's arming alone (ADR-0070); an in-dialog INVITE's from its opening.

use std::sync::Arc;

use sip_message::SipMessage;
use sip_txn::timers::TRYING_BOUND;
use sip_txn::{IdGen, TransactionConfig, TransactionEvent};

use crate::common::*;

const TRANSIT: u64 = 5;
/// RFC 3261 T1: an INVITE's first copy is due this long after it, the
/// second 2·T1 after the first (Timer A, §17.1.1.2).
const T1: u64 = 500;

/// A stack at the layer's default bound.
async fn stack() -> Stack {
    let config = TransactionConfig::new(Arc::new(IdGen::seeded(0xC0FFEE)));
    assert_eq!(config.trying_bound_ms, TRYING_BOUND, "the default bound is §17.2.1's");
    assert!(!config.consumer_arms_initial_trying, "every bound runs from the opening");
    Stack::build_with_config(TRANSIT, 64, config).await
}

/// A stack at the default bound whose consumer arms an initial INVITE's.
async fn armed_stack() -> Stack {
    let config = TransactionConfig {
        consumer_arms_initial_trying: true,
        ..TransactionConfig::new(Arc::new(IdGen::seeded(0xC0FFEE)))
    };
    Stack::build_with_config(TRANSIT, 64, config).await
}

/// How many requests the consumer was handed.
fn messages(events: &[TransactionEvent]) -> usize {
    events.iter().filter(|e| matches!(e, TransactionEvent::Message { .. })).count()
}

/// An INVITE stating `Timestamp: 54.25`.
fn stamped_invite(branch: &str, call_id: &str) -> Vec<u8> {
    String::from_utf8(inbound_request("INVITE", branch, call_id, None))
        .expect("utf-8")
        .replace("Content-Length: 0", "Timestamp: 54.25\r\nContent-Length: 0")
        .into_bytes()
}

fn timestamps(msgs: &[SipMessage]) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match m {
            SipMessage::Response(r) if r.status() == 100 => Some(
                r.raw(sip_message::header::HeaderName::Timestamp).collect::<Vec<_>>().join(","),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_unreleased_100_leaves_at_the_bound_stating_its_delay() {
    let mut stack = stack().await;
    stack.inject(&stamped_invite("z9hG4bK-bound", "bound-call")).await;
    elapse_ms(TRANSIT + TRYING_BOUND - 10).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 0, "no 100 before the bound");
    let _ = stack.drain_events();
    elapse_ms(20).await;
    let sent = stack.drain_peer();
    assert_eq!(count_responses(&sent, 100), 1, "the bound sends the 100");
    assert_eq!(timestamps(&sent), ["54.25 0.200"], "a late 100 states its delay");
    elapse_ms(1_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 0, "one 100 only");
}

#[tokio::test(start_paused = true)]
async fn a_released_100_leaves_at_once_and_only_once() {
    let mut stack = stack().await;
    let raw = inbound_request("INVITE", "z9hG4bK-released", "released-call", None);
    stack.inject(&raw).await;
    elapse_ms(TRANSIT + 30).await;
    let _ = stack.drain_events();
    let invite = request_of(&raw);
    assert!(stack.txn.release_trying(&invite).await.unwrap(), "the release sends the 100");
    elapse_ms(TRANSIT + 1).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the released 100");
    assert!(!stack.txn.release_trying(&invite).await.unwrap(), "a second release sends none");
    elapse_ms(TRYING_BOUND * 2).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 0, "the bound sends none after it");
}

#[tokio::test(start_paused = true)]
async fn a_response_first_draws_no_100() {
    let mut stack = stack().await;
    let (branch, call_id) = ("z9hG4bK-ringing", "ringing-call");
    let raw = inbound_request("INVITE", branch, call_id, None);
    stack.inject(&raw).await;
    elapse_ms(TRANSIT + 10).await;
    let _ = stack.drain_events();
    let ringing = parse_response(&response_bytes(180, "Ringing", "INVITE", branch, call_id, true));
    stack.txn.send_response(ringing, addr(PEER)).await.unwrap();
    elapse_ms(TRYING_BOUND * 2).await;
    let sent = stack.drain_peer();
    assert_eq!(count_responses(&sent, 180), 1);
    assert_eq!(count_responses(&sent, 100), 0, "the 180 stood for the 100");
    assert!(!stack.txn.release_trying(&request_of(&raw)).await.unwrap());
}

/// An initial INVITE whose consumer has not armed its bound draws nothing,
/// its copy included, however long it waits; once armed, its 100 leaves at
/// the bound from the arming, and a later copy draws that 100 again.
#[tokio::test(start_paused = true)]
async fn an_initial_invite_draws_its_100_only_at_the_bound_from_its_arming() {
    let mut stack = armed_stack().await;
    let raw = inbound_request("INVITE", "z9hG4bK-armed", "armed-call", None);
    stack.inject(&raw).await;
    elapse_ms(T1).await;
    stack.inject(&raw).await;
    elapse_ms(T1).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 0, "no 100 before the arming");
    assert_eq!(messages(&stack.drain_events()), 1, "the copy absorbed, unanswered");

    stack.txn.arm_trying(&request_of(&raw)).await.unwrap();
    elapse_ms(TRYING_BOUND - 10).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 0, "no 100 before the armed bound");
    elapse_ms(20).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the armed bound sends the 100");

    elapse_ms(T1 - TRYING_BOUND - 10).await;
    stack.inject(&raw).await;
    elapse_ms(2 * TRANSIT + 1).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the copy draws the 100 again");
    assert_eq!(messages(&stack.drain_events()), 0, "the copy reaches no consumer");
}

/// A release before any arming sends the 100 at once, and an arming after it
/// sends none.
#[tokio::test(start_paused = true)]
async fn a_release_needs_no_arming_and_an_arming_after_it_sends_nothing() {
    let mut stack = armed_stack().await;
    let raw = inbound_request("INVITE", "z9hG4bK-released-unarmed", "released-unarmed", None);
    stack.inject(&raw).await;
    elapse_ms(TRANSIT + TRYING_BOUND * 2).await;
    let _ = stack.drain_events();
    let invite = request_of(&raw);
    assert!(stack.txn.release_trying(&invite).await.unwrap(), "the release sends the 100");
    stack.txn.arm_trying(&invite).await.unwrap();
    elapse_ms(TRYING_BOUND * 2).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the released 100 alone");
}

/// An in-dialog INVITE keeps its bound from its opening, armed or not.
#[tokio::test(start_paused = true)]
async fn an_in_dialog_invite_keeps_its_bound_from_its_opening() {
    let mut stack = armed_stack().await;
    let raw = inbound_request("INVITE", "z9hG4bK-reinvite", "reinvite-call", Some("uas-tag"));
    stack.inject(&raw).await;
    elapse_ms(TRANSIT + TRYING_BOUND - 10).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 0, "no 100 before the bound");
    let _ = stack.drain_events();
    elapse_ms(20).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the bound sends the 100");
}
