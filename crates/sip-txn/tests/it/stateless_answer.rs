//! A final sent as a stateless UAS sends it (RFC 3261 §8.2.7), at the
//! transaction-layer seam: the server transaction its request opened leaves
//! with no trace — no copy record (ADR-0063 §1), no remembered To-tag, nothing
//! on the unattributed books — so every copy of the request is a first
//! arrival, handed up and answered once. An INVITE whose 100 already left
//! keeps its transaction and owes its final Timer G (§17.2.1).

use std::sync::Arc;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{SipMessage, SipResponse};
use sip_txn::timers::TRYING_BOUND;
use sip_txn::{IdGen, TransactionConfig, TransactionEvent};

use crate::common::*;

const TRANSIT: u64 = 5;
/// The copies of a request sent in a burst: more than the 10 a compliant
/// non-INVITE client re-sends (§17.1.2.2), so a copy record would absorb the
/// last ones (ADR-0063 §1).
const COPIES: usize = 16;

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

/// The requests of `method` among `events`, each handed up as a `Message`.
fn handed_up(events: &[TransactionEvent], method: &str) -> usize {
    events
        .iter()
        .filter(|e| match e {
            TransactionEvent::Message { message, .. } => {
                matches!(message.as_ref(), SipMessage::Request(r) if r.method() == method)
            }
            _ => false,
        })
        .count()
}

/// The 481 a request naming no dialog draws (§12.2.2), under `to_tag` when
/// its To carries none.
fn gone(raw: &[u8], to_tag: Option<&str>) -> SipResponse {
    let opts = GenerateResponseOpts { to_tag: to_tag.map(str::to_string), ..Default::default() };
    generate_response(&request_of(raw), 481, "Call/Transaction Does Not Exist", &opts)
        .expect("the request echoes")
}

fn to_tags(msgs: &[SipMessage], status: u16) -> Vec<String> {
    msgs.iter()
        .filter_map(|m| match m {
            SipMessage::Response(r) if r.status() == status => r.to().tag().map(str::to_string),
            _ => None,
        })
        .collect()
}

/// Each copy of a tagged BYE, answered statelessly, is handed up again and
/// draws its one 481: no transaction absorbs it, and no copy record left by
/// the one before it absorbs the copies past Timer E's re-sends.
#[tokio::test(start_paused = true)]
async fn every_copy_of_a_request_answered_statelessly_is_a_first_arrival() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let bye = inbound_request("BYE", "z9hG4bK-sl-copies", "sl-copies@u", Some("b-gone"));

    for copy in 0..COPIES {
        stack.inject(&bye).await;
        elapse_ms(10).await;
        assert_eq!(handed_up(&stack.drain_events(), "BYE"), 1, "copy {copy} is handed up");
        stack.txn.send_stateless_response(gone(&bye, None), addr(PEER)).await.unwrap();
        elapse_ms(10).await;
        assert_eq!(active(&stack), 0, "copy {copy}: no transaction outlives its 481");
        let peer = stack.drain_peer();
        assert_eq!(count_responses(&peer, 481), 1, "copy {copy} draws its 481, once");
    }

    elapse_ms(33_000).await;
    assert!(stack.drain_peer().is_empty(), "no answer is re-sent");
}

/// A tagged request answered statelessly leaves no To-tag remembered for its
/// dialog: a CANCEL of the same Call-ID and From-tag answered after it keeps
/// the tag it was composed with.
#[tokio::test(start_paused = true)]
async fn a_request_answered_statelessly_leaves_no_remembered_to_tag() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let bye = inbound_request("BYE", "z9hG4bK-sl-tag-bye", "sl-tag@u", Some("b-gone"));
    stack.inject(&bye).await;
    elapse_ms(10).await;
    let _ = stack.drain_events();
    stack.txn.send_stateless_response(gone(&bye, None), addr(PEER)).await.unwrap();
    elapse_ms(10).await;
    assert_eq!(to_tags(&stack.drain_peer(), 481), vec!["b-gone".to_string()]);

    let cancel = inbound_request("CANCEL", "z9hG4bK-sl-tag-cancel", "sl-tag@u", None);
    stack.inject(&cancel).await;
    elapse_ms(10).await;
    assert_eq!(handed_up(&stack.drain_events(), "CANCEL"), 1, "no INVITE matches the CANCEL");
    let answer = gone(&cancel, Some("own-tag"));
    stack.txn.send_stateless_response(answer, addr(PEER)).await.unwrap();
    elapse_ms(10).await;
    assert_eq!(
        to_tags(&stack.drain_peer(), 481),
        vec!["own-tag".to_string()],
        "no tag of the BYE's dialog was remembered to bind the CANCEL's answer to"
    );
    assert_eq!(active(&stack), 0);
}

/// A tagged request whose Request-URI names no call is filed unattributed
/// while its consumer resolves it, and a watched call naming its Call-ID
/// counts it; answered statelessly, it leaves the books with its transaction.
#[tokio::test(start_paused = true)]
async fn a_request_answered_statelessly_leaves_the_unattributed_books() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let call_ids = ["sl-unattributed@u".to_string()];
    let info = inbound_request("INFO", "z9hG4bK-sl-unattributed", &call_ids[0], Some("b-gone"));
    stack.inject(&info).await;
    elapse_ms(10).await;
    let _ = stack.drain_events();
    let serving = stack.txn.active_txn_count_for_call("watched-call", &call_ids).await.unwrap();
    assert_eq!(serving, 1, "the unanswered request counts for a call naming its Call-ID");

    stack.txn.send_stateless_response(gone(&info, None), addr(PEER)).await.unwrap();
    elapse_ms(10).await;
    let serving = stack.txn.active_txn_count_for_call("watched-call", &call_ids).await.unwrap();
    assert_eq!(serving, 0, "nothing of it is left on the unattributed books");
    assert_eq!(active(&stack), 0);
}

/// A tagged INVITE answered statelessly before its 100's bound: the 481
/// leaves alone, with no 100 before it and no Timer G re-send after it, and
/// its ACK, which no transaction absorbs, is handed up.
#[tokio::test(start_paused = true)]
async fn a_tagged_invite_answered_before_its_100_leaves_no_transaction() {
    let config = TransactionConfig {
        trying_bound_ms: TRYING_BOUND,
        ..TransactionConfig::new(Arc::new(IdGen::seeded(0xC0FFEE)))
    };
    let mut stack = Stack::build_with_config(TRANSIT, 64, config).await;
    let (branch, call_id) = ("z9hG4bK-sl-invite-early", "sl-invite-early@u");
    let invite = inbound_request("INVITE", branch, call_id, Some("b-gone"));
    stack.inject(&invite).await;
    elapse_ms(10).await;
    assert_eq!(handed_up(&stack.drain_events(), "INVITE"), 1);

    stack.txn.send_stateless_response(gone(&invite, None), addr(PEER)).await.unwrap();
    elapse_ms(3_000).await;
    let peer = stack.drain_peer();
    assert_eq!(count_responses(&peer, 100), 0, "no 100 before the 481 (§8.2.7)");
    assert_eq!(count_responses(&peer, 481), 1, "the 481 leaves once: no Timer G (§8.2.7)");
    assert_eq!(active(&stack), 0, "no transaction waits on the ACK");

    stack.inject(&inbound_request("ACK", branch, call_id, Some("b-gone"))).await;
    elapse_ms(10).await;
    assert_eq!(handed_up(&stack.drain_events(), "ACK"), 1, "the ACK is the consumer's");
}

/// A tagged INVITE whose 100 left at its bound before the consumer answered:
/// the 481 goes through its transaction, which re-sends it on Timer G until
/// the ACK, absorbs the ACK and leaves at Timer I (§17.2.1).
#[tokio::test(start_paused = true)]
async fn a_tagged_invite_answered_after_its_100_keeps_its_transaction() {
    let config = TransactionConfig {
        trying_bound_ms: TRYING_BOUND,
        ..TransactionConfig::new(Arc::new(IdGen::seeded(0xC0FFEE)))
    };
    let mut stack = Stack::build_with_config(TRANSIT, 64, config).await;
    let (branch, call_id) = ("z9hG4bK-sl-invite-late", "sl-invite-late@u");
    let invite = inbound_request("INVITE", branch, call_id, Some("b-gone"));
    stack.inject(&invite).await;
    elapse_ms(TRYING_BOUND + 50).await;
    assert_eq!(handed_up(&stack.drain_events(), "INVITE"), 1);
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the bound's 100 left");

    stack.txn.send_stateless_response(gone(&invite, None), addr(PEER)).await.unwrap();
    elapse_ms(700).await;
    assert_eq!(count_responses(&stack.drain_peer(), 481), 2, "the 481 and its Timer G re-send");
    assert_eq!(active(&stack), 1, "the transaction waits on the ACK");

    stack.inject(&inbound_request("ACK", branch, call_id, Some("b-gone"))).await;
    elapse_ms(10).await;
    assert!(stack.drain_events().is_empty(), "the transaction absorbs its ACK");
    elapse_ms(5_100).await;
    assert_eq!(active(&stack), 0, "Timer I ended it");
    assert!(stack.drain_peer().is_empty());
}
