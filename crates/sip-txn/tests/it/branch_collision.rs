//! RFC 3261 §17.2.3: a server transaction is its top-Via branch, sent-by and
//! method. Two senders that pick one branch, or one sender that reuses a
//! branch for another method, open two server transactions, each with the
//! whole §17.2 behaviour: its own 100, absorption of its own copies, Timer G
//! on a non-2xx final, its ACK, and a CANCEL matching it alone (§9.2). The
//! sent-by is compared host case-insensitively and port as written; the
//! `received` and `rport` a hop adds are no part of it.

use crate::common;

use common::*;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{SipMessage, SipRequest};
use sip_txn::timers::{TIMER_H, TIMER_J};
use sip_txn::TransactionEvent;

const TRANSIT: u64 = 5;
const BRANCH: &str = "z9hG4bK-collide";
const TAG: &str = "uas-tag";

/// `raw` with its top Via's sent-by `10.0.0.1:5555` rewritten to `sent_by`.
fn from(raw: Vec<u8>, sent_by: &str) -> Vec<u8> {
    String::from_utf8(raw)
        .unwrap()
        .replacen("Via: SIP/2.0/UDP 10.0.0.1:5555", &format!("Via: SIP/2.0/UDP {sent_by}"), 1)
        .into_bytes()
}

fn request(method: &str, call_id: &str, to_tag: Option<&str>, sent_by: &str) -> Vec<u8> {
    from(inbound_request(method, BRANCH, call_id, to_tag), sent_by)
}

/// Requests of `method` handed up, with their source.
fn requests(events: &[TransactionEvent], method: &str) -> Vec<(SipRequest, std::net::SocketAddr)> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, src, .. } => match message.as_ref() {
                SipMessage::Request(r) if r.method() == method => Some((r.clone(), *src)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn cancelled(events: &[TransactionEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Cancelled { call_id, .. } => Some(call_id.clone()),
            _ => None,
        })
        .collect()
}

/// Responses of `status` on the wire whose Call-ID is `call_id`.
fn on_wire(wire: &[SipMessage], status: u16, call_id: &str) -> usize {
    wire.iter()
        .filter(|m| {
            matches!(m, SipMessage::Response(r)
                if r.status() == status && r.call_id().as_str() == call_id)
        })
        .count()
}

async fn answer(stack: &Stack, req: &SipRequest, src: std::net::SocketAddr, status: u16) {
    let opts = GenerateResponseOpts { to_tag: Some(TAG.to_string()), ..Default::default() };
    let resp = generate_response(req, status, "Answer", &opts);
    stack.txn.send_response(resp, src).await.unwrap();
}

/// The one request of `method` handed up.
async fn surfaced(stack: &mut Stack, method: &str) -> (SipRequest, std::net::SocketAddr) {
    elapse_ms(20).await;
    let events = stack.drain_events();
    let mut got = requests(&events, method);
    assert_eq!(got.len(), 1, "one {method} surfaces: {events:?}");
    got.remove(0)
}

/// A's INVITE rings on a branch; C sends an INVITE on that branch from
/// another sent-by. C's INVITE opens its own transaction: its own 100, its
/// copy absorbed, and its CANCEL answered 200 + 487 with `Cancelled` for C
/// alone; A's INVITE rings on.
#[tokio::test(start_paused = true)]
async fn a_second_sender_on_a_ringing_branch_gets_its_own_invite_transaction() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let a = request("INVITE", "a@u", None, "10.0.0.1:5555");
    stack.inject(&a).await;
    let (a_req, a_src) = surfaced(&mut stack, "INVITE").await;
    assert_eq!(on_wire(&stack.drain_peer(), 100, "a@u"), 1);

    let c = request("INVITE", "c@u", None, "10.0.0.2:5555");
    stack.inject(&c).await;
    let (c_req, _) = surfaced(&mut stack, "INVITE").await;
    assert_eq!(c_req.call_id().as_str(), "c@u");
    assert_eq!(on_wire(&stack.drain_peer(), 100, "c@u"), 1, "C draws its own 100");

    stack.inject(&c).await;
    elapse_ms(20).await;
    assert!(stack.drain_events().is_empty(), "C's copy is absorbed");
    assert_eq!(on_wire(&stack.drain_peer(), 100, "c@u"), 1, "and replays C's 100");

    stack.inject(&request("CANCEL", "c@u", None, "10.0.0.2:5555")).await;
    elapse_ms(20).await;
    assert_eq!(cancelled(&stack.drain_events()), vec!["c@u".to_string()]);
    let wire = stack.drain_peer();
    assert_eq!((on_wire(&wire, 200, "c@u"), on_wire(&wire, 487, "c@u")), (1, 1));
    assert_eq!(on_wire(&wire, 487, "a@u"), 0, "A's INVITE is not cancelled");
    stack.inject(&request("ACK", "c@u", Some(TAG), "10.0.0.2:5555")).await;

    stack.inject(&a).await;
    elapse_ms(20).await;
    assert!(stack.drain_events().is_empty(), "A's copy is absorbed by A's transaction");
    assert_eq!(on_wire(&stack.drain_peer(), 100, "a@u"), 1, "and replays A's 100");

    answer(&stack, &a_req, a_src, 486).await;
    stack.inject(&request("ACK", "a@u", Some(TAG), "10.0.0.1:5555")).await;
    elapse_ms(TIMER_H + 1_000).await;
    assert!(stack.drain_events().is_empty(), "both ACKs are absorbed");
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// The TU's 486 to a colliding INVITE leaves through that INVITE's own
/// transaction: Timer G repeats it until C's ACK.
#[tokio::test(start_paused = true)]
async fn a_colliding_invite_answered_486_repeats_it_on_timer_g() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack.inject(&request("INVITE", "a@u", None, "10.0.0.1:5555")).await;
    let (a_req, a_src) = surfaced(&mut stack, "INVITE").await;
    stack.inject(&request("INVITE", "c@u", None, "10.0.0.2:5555")).await;
    let (c_req, c_src) = surfaced(&mut stack, "INVITE").await;
    stack.drain_peer();

    answer(&stack, &c_req, c_src, 486).await;
    elapse_ms(20).await;
    assert_eq!(on_wire(&stack.drain_peer(), 486, "c@u"), 1);
    elapse_ms(600).await;
    assert_eq!(on_wire(&stack.drain_peer(), 486, "c@u"), 1, "Timer G repeats the 486");

    stack.inject(&request("ACK", "c@u", Some(TAG), "10.0.0.2:5555")).await;
    elapse_ms(5_000).await;
    assert_eq!(on_wire(&stack.drain_peer(), 486, "c@u"), 0, "C's ACK ends Timer G");
    assert!(stack.drain_events().is_empty(), "C's ACK is absorbed");

    answer(&stack, &a_req, a_src, 486).await;
    stack.inject(&request("ACK", "a@u", Some(TAG), "10.0.0.1:5555")).await;
    elapse_ms(TIMER_H + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A CANCEL on A's branch from another sent-by names no INVITE here: it is
/// handed up for the TU to answer (§9.2), and A's INVITE is not cancelled.
#[tokio::test(start_paused = true)]
async fn a_cancel_from_another_sent_by_does_not_cancel_the_invite() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack.inject(&request("INVITE", "a@u", None, "10.0.0.1:5555")).await;
    let (a_req, a_src) = surfaced(&mut stack, "INVITE").await;
    stack.drain_peer();

    stack.inject(&request("CANCEL", "a@u", None, "10.0.0.2:5555")).await;
    elapse_ms(20).await;
    let events = stack.drain_events();
    assert!(cancelled(&events).is_empty(), "no Cancelled: {events:?}");
    assert_eq!(requests(&events, "CANCEL").len(), 1, "the CANCEL is the TU's to answer");
    assert_eq!(on_wire(&stack.drain_peer(), 487, "a@u"), 0, "A's INVITE rings on");

    answer(&stack, &a_req, a_src, 486).await;
    stack.inject(&request("ACK", "a@u", Some(TAG), "10.0.0.1:5555")).await;
    elapse_ms(TIMER_H + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// After a BYE from another sent-by collides on its branch, a copy of the
/// first BYE still draws the first BYE's cached 200 and never surfaces again:
/// the colliding request displaced nothing.
#[tokio::test(start_paused = true)]
async fn a_collision_leaves_the_resident_transaction_in_place() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let a = request("BYE", "a@u", Some("dialog"), "10.0.0.1:5555");
    stack.inject(&a).await;
    let (a_req, a_src) = surfaced(&mut stack, "BYE").await;
    answer(&stack, &a_req, a_src, 200).await;

    stack.inject(&request("BYE", "c@u", Some("dialog"), "10.0.0.2:5555")).await;
    let (c_req, c_src) = surfaced(&mut stack, "BYE").await;
    answer(&stack, &c_req, c_src, 200).await;
    elapse_ms(20).await;
    stack.drain_peer();

    stack.inject(&a).await;
    elapse_ms(20).await;
    assert!(stack.drain_events().is_empty(), "A's copy does not surface twice");
    let wire = stack.drain_peer();
    assert_eq!((on_wire(&wire, 200, "a@u"), on_wire(&wire, 200, "c@u")), (1, 0));

    elapse_ms(TIMER_J + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A copy whose top Via spells the host in another case, or carries the
/// `rport` or `received` a hop adds (§18.2.1, RFC 3581), is the same
/// transaction's retransmission.
#[tokio::test(start_paused = true)]
async fn a_copy_with_another_host_case_rport_or_received_is_absorbed() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack.inject(&request("BYE", "a@u", Some("dialog"), "Peer.Example.com:5555")).await;
    let (req, src) = surfaced(&mut stack, "BYE").await;
    answer(&stack, &req, src, 200).await;
    elapse_ms(20).await;
    stack.drain_peer();

    for sent_by in [
        "peer.example.COM:5555",
        "Peer.Example.com:5555;rport",
        "Peer.Example.com:5555;received=192.0.2.7;rport=6000",
    ] {
        stack.inject(&request("BYE", "a@u", Some("dialog"), sent_by)).await;
        elapse_ms(20).await;
        assert!(stack.drain_events().is_empty(), "{sent_by}: absorbed");
        assert_eq!(on_wire(&stack.drain_peer(), 200, "a@u"), 1, "{sent_by}: the 200 replays");
    }

    elapse_ms(TIMER_J + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// An omitted port and an explicit 5060 are two sent-bys, as an omitted
/// default and an explicit one are two URIs (§19.1.4): the second request is
/// another transaction.
#[tokio::test(start_paused = true)]
async fn an_omitted_port_and_an_explicit_default_are_two_sent_bys() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack.inject(&request("BYE", "a@u", Some("dialog"), "10.0.0.1")).await;
    let (req, src) = surfaced(&mut stack, "BYE").await;
    answer(&stack, &req, src, 200).await;

    stack.inject(&request("BYE", "a@u", Some("dialog"), "10.0.0.1:5060")).await;
    let (req, src) = surfaced(&mut stack, "BYE").await;
    answer(&stack, &req, src, 200).await;

    elapse_ms(TIMER_J + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}

/// A NOTIFY is Trying; an INFO in the same dialog reuses its branch and is
/// another transaction. Forgetting the discarded INFO by its key leaves the
/// NOTIFY's transaction, which still absorbs its copy.
#[tokio::test(start_paused = true)]
async fn forgetting_a_request_leaves_another_method_on_its_branch() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let notify = request("NOTIFY", "d@u", Some("dialog"), "10.0.0.1:5555");
    stack.inject(&notify).await;
    let (notify_req, notify_src) = surfaced(&mut stack, "NOTIFY").await;
    let info = request("INFO", "d@u", Some("dialog"), "10.0.0.1:5555");
    stack.inject(&info).await;
    surfaced(&mut stack, "INFO").await;
    assert_eq!(stack.txn.metrics().active_transactions(), 2);

    stack.txn.forget_unanswered(&key_of(&info), "d@u", "caller-tag");
    elapse_ms(10).await;
    assert_eq!(stack.txn.metrics().unanswered_forgotten(), 1);
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "the NOTIFY's stays");

    stack.inject(&notify).await;
    elapse_ms(20).await;
    assert!(stack.drain_events().is_empty(), "the NOTIFY's copy is absorbed");

    answer(&stack, &notify_req, notify_src, 200).await;
    elapse_ms(TIMER_J + 1_000).await;
    assert_eq!(stack.txn.metrics().active_transactions(), 0, "every transaction ended");
}
