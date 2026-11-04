//! An INVITE's first provisional (RFC 3261 §17.2.1): the server transaction
//! sends its 100 Trying when its consumer releases it, or itself at the
//! §17.2.1 bound, and never after another response. A 100 that leaves after
//! the INVITE arrived states the delay in its `Timestamp` (§8.2.6.1).

use std::sync::Arc;

use sip_message::SipMessage;
use sip_txn::timers::TRYING_BOUND;
use sip_txn::{IdGen, TransactionConfig};

use crate::common::*;

const TRANSIT: u64 = 5;

/// A stack at the layer's default bound.
async fn stack() -> Stack {
    let config = TransactionConfig::new(Arc::new(IdGen::seeded(0xC0FFEE)));
    assert_eq!(config.trying_bound_ms, TRYING_BOUND, "the default bound is §17.2.1's");
    Stack::build_with_config(TRANSIT, 64, config).await
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
