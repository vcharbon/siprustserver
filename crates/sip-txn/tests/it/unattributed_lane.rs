//! `TransactionConfig::unattributed_lane`: a request that matches no
//! transaction and whose Request-URI states no `callRef`, other than an
//! initial INVITE, rides a bounded lane of its own, so a flood of such
//! requests takes neither the event queue's room nor its age. A request that
//! finds the lane full is dropped before any transaction or answer is made, so
//! its retransmission is judged afresh; a CANCEL of an INVITE that rode the
//! lane rides it behind that INVITE, or is dropped unanswered when the lane is
//! full. With no lane, every request rides the event queue.

use std::sync::Arc;
use std::time::Duration;

use crate::common;
use common::*;
use sip_message::SipMessage;
use sip_txn::{IdGen, TransactionConfig, TransactionEvent};
use tokio::sync::mpsc;

const TRANSIT: u64 = 5;
/// A request's flight in and its answer's flight back.
const ROUND_TRIP: u64 = 3 * TRANSIT;

async fn stack(lane: Option<usize>) -> Stack {
    Stack::build_with_config(
        TRANSIT,
        1024,
        TransactionConfig {
            udp_queue_max: 16,
            id_gen: Arc::new(IdGen::seeded(0x1a4e)),
            unattributed_lane: lane,
            ..common::config()
        },
    )
    .await
}

/// `raw`, an [`inbound_request`], its Request-URI naming the call `c1`.
fn naming_a_call(raw: &[u8]) -> Vec<u8> {
    let raw = std::str::from_utf8(raw).expect("utf-8");
    raw.replacen(
        "sip:b2bua@127.0.0.1:5070 SIP/2.0",
        "sip:b2bua@127.0.0.1:5070;callRef=c1 SIP/2.0",
        1,
    )
    .into_bytes()
}

/// What each event says: a request's method, a `Cancelled`, or another kind.
fn label(ev: &TransactionEvent) -> String {
    match ev {
        TransactionEvent::Message { message, .. } => match message.as_ref() {
            SipMessage::Request(req) => req.method().as_str().to_string(),
            SipMessage::Response(resp) => resp.status().to_string(),
        },
        TransactionEvent::Cancelled { .. } => "Cancelled".to_string(),
        _ => "other".to_string(),
    }
}

/// Everything waiting on `lane` now, by [`label`].
fn drain(lane: &mut mpsc::Receiver<TransactionEvent>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(ev) = lane.try_recv() {
        out.push(label(&ev));
    }
    out
}

#[tokio::test(start_paused = true)]
async fn requests_that_name_no_call_ride_the_lane_and_the_rest_the_event_queue() {
    let mut stack = stack(Some(16)).await;
    let mut lane = stack.events.take_unattributed_lane().expect("the layer runs a lane");
    assert!(stack.events.take_unattributed_lane().is_none(), "the lane is handed over once");

    for raw in [
        inbound_request("BYE", "z9hG4bK-bye", "stray-bye@u", Some("t1")),
        inbound_request("MESSAGE", "z9hG4bK-msg", "stray-msg@u", None),
        inbound_request("CANCEL", "z9hG4bK-cancel", "stray-cancel@u", None),
        inbound_request("INVITE", "z9hG4bK-reinvite", "stray-reinvite@u", Some("t2")),
        inbound_request("ACK", "z9hG4bK-ack", "stray-ack@u", Some("t3")),
        naming_a_call(&inbound_request("BYE", "z9hG4bK-keyed", "keyed@u", Some("t4"))),
        inbound_request("INVITE", "z9hG4bK-new", "new@u", None),
    ] {
        stack.inject(&raw).await;
        elapse_ms(1).await;
    }
    elapse_ms(TRANSIT + 1).await;

    assert_eq!(drain(&mut lane), ["BYE", "MESSAGE", "CANCEL", "INVITE", "ACK"], "in order");
    let queued: Vec<String> = stack.drain_events().iter().map(label).collect();
    assert_eq!(queued, ["BYE", "INVITE"], "the keyed BYE and the new call");
    assert_eq!(stack.txn.metrics().unattributed_lane_drops(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_full_lane_drops_a_request_before_any_transaction_and_its_copy_is_taken_afresh() {
    let mut stack = stack(Some(2)).await;
    let mut lane = stack.events.take_unattributed_lane().expect("the layer runs a lane");
    let strays: Vec<Vec<u8>> = (0..3)
        .map(|i| inbound_request("BYE", &format!("z9hG4bK-s{i}"), &format!("s{i}@u"), Some("t")))
        .collect();
    for raw in &strays {
        stack.inject(raw).await;
    }
    elapse_ms(TRANSIT + 1).await;
    assert_eq!(stack.txn.metrics().unattributed_lane_drops(), 1, "the newest is dropped");
    assert_eq!(stack.txn.metrics().active_transactions(), 2, "the dropped one opened none");
    assert_eq!(drain(&mut lane), ["BYE", "BYE"]);

    stack.inject(&strays[2]).await;
    elapse_ms(TRANSIT + 1).await;
    assert_eq!(drain(&mut lane), ["BYE"], "its copy is taken afresh, not absorbed");
    assert_eq!(stack.txn.metrics().unattributed_lane_drops(), 1);
    assert!(stack.drain_events().is_empty(), "nothing reaches the event queue");
}

#[tokio::test(start_paused = true)]
async fn a_flood_on_the_lane_leaves_the_event_queue_its_room_and_its_age() {
    let mut stack = stack(Some(4)).await;
    let _lane = stack.events.take_unattributed_lane().expect("the layer runs a lane");
    for i in 0..100 {
        let raw = inbound_request("BYE", &format!("z9hG4bK-f{i}"), &format!("f{i}@u"), Some("t"));
        stack.inject(&raw).await;
    }
    elapse_ms(TRANSIT + 1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let metrics = stack.txn.metrics().clone();
    assert_eq!(metrics.unattributed_lane_drops(), 96, "all but the lane's bound are shed");
    assert_eq!(metrics.event_queue_depth(), 0, "the event queue holds none");
    assert_eq!(metrics.event_queue_delay(), Duration::ZERO, "and reads no age");

    let keyed = naming_a_call(&inbound_request("BYE", "z9hG4bK-keyed", "keyed@u", Some("t")));
    stack.inject(&keyed).await;
    elapse_ms(TRANSIT + 1).await;
    let queued: Vec<String> = stack.drain_events().iter().map(label).collect();
    assert_eq!(queued, ["BYE"], "the BYE naming a call is queued");
}

#[tokio::test(start_paused = true)]
async fn a_tagged_invite_the_full_lane_drops_draws_no_trying_until_its_copy_is_taken() {
    let mut stack = Stack::build_with_config(
        TRANSIT,
        1024,
        TransactionConfig {
            udp_queue_max: 16,
            id_gen: Arc::new(IdGen::seeded(0x1a4e)),
            unattributed_lane: Some(1),
            trying_bound_ms: 200,
            ..common::config()
        },
    )
    .await;
    let mut lane = stack.events.take_unattributed_lane().expect("the layer runs a lane");
    stack.inject(&inbound_request("BYE", "z9hG4bK-fill", "fill@u", Some("t"))).await;
    let reinvite = inbound_request("INVITE", "z9hG4bK-re", "re@u", Some("t"));
    stack.inject(&reinvite).await;
    elapse_ms(TRANSIT + 400).await;
    assert_eq!(stack.txn.metrics().unattributed_lane_drops(), 1);
    assert_eq!(stack.txn.metrics().active_transactions(), 1, "the INVITE opened none");
    assert_eq!(count_responses(&stack.drain_peer(), 100), 0, "no 100 answers it");

    assert_eq!(drain(&mut lane), ["BYE"]);
    stack.inject(&reinvite).await;
    elapse_ms(TRANSIT + 400).await;
    assert_eq!(drain(&mut lane), ["INVITE"], "its copy is taken afresh");
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the bound's 100 answers it");
}

#[tokio::test(start_paused = true)]
async fn a_cancel_of_an_invite_on_the_lane_rides_behind_it() {
    let mut stack = stack(Some(4)).await;
    let mut lane = stack.events.take_unattributed_lane().expect("the layer runs a lane");
    stack.inject(&inbound_request("INVITE", "z9hG4bK-re", "re@u", Some("t"))).await;
    elapse_ms(TRANSIT + 1).await;
    stack.inject(&inbound_request("CANCEL", "z9hG4bK-re", "re@u", Some("t"))).await;
    elapse_ms(ROUND_TRIP).await;

    assert_eq!(drain(&mut lane), ["INVITE", "Cancelled"], "behind the INVITE it cancels");
    assert!(stack.drain_events().is_empty(), "nothing on the event queue");
    let sent = stack.drain_peer();
    assert_eq!(count_responses(&sent, 200), 1, "the CANCEL is answered");
    assert_eq!(count_responses(&sent, 487), 1, "and its INVITE ended");
}

#[tokio::test(start_paused = true)]
async fn a_cancel_finding_the_lane_full_is_dropped_unanswered_and_its_copy_judged_again() {
    let mut stack = stack(Some(1)).await;
    let mut lane = stack.events.take_unattributed_lane().expect("the layer runs a lane");
    stack.inject(&inbound_request("INVITE", "z9hG4bK-re", "re@u", Some("t"))).await;
    elapse_ms(TRANSIT + 1).await;
    let cancel = inbound_request("CANCEL", "z9hG4bK-re", "re@u", Some("t"));
    stack.inject(&cancel).await;
    elapse_ms(ROUND_TRIP).await;
    assert_eq!(stack.txn.metrics().unattributed_lane_drops(), 1, "the CANCEL is dropped");
    let sent = stack.drain_peer();
    assert_eq!(count_responses(&sent, 200), 0, "unanswered");
    assert_eq!(count_responses(&sent, 487), 0, "its INVITE goes on");

    assert_eq!(drain(&mut lane), ["INVITE"]);
    stack.inject(&cancel).await;
    elapse_ms(ROUND_TRIP).await;
    assert_eq!(drain(&mut lane), ["Cancelled"], "its copy cancels the INVITE");
    let sent = stack.drain_peer();
    assert_eq!(count_responses(&sent, 200), 1);
    assert_eq!(count_responses(&sent, 487), 1);
}

#[tokio::test(start_paused = true)]
async fn with_no_lane_a_request_naming_no_call_rides_the_event_queue() {
    let mut stack = stack(None).await;
    assert!(stack.events.take_unattributed_lane().is_none(), "no lane");
    stack.inject(&inbound_request("BYE", "z9hG4bK-bye", "stray@u", Some("t"))).await;
    elapse_ms(TRANSIT + 1).await;
    let queued: Vec<String> = stack.drain_events().iter().map(label).collect();
    assert_eq!(queued, ["BYE"]);
}
