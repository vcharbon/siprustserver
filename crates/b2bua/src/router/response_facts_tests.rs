//! What a callee's provisional records through the real turn: a 180–189 to
//! the dialling INVITE marks the leg rung and arms its ring deadline from the
//! turn's instant; a 199 (RFC 6228, an early dialog's end) and a 100 do
//! neither.

use std::time::Duration;

use call::{AttemptCause, Call, Deadline, Direction, Ring, TimerKey, TimerType};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::test_support::{invite, node, src};
use crate::config::B2buaConfig;
use crate::effects::HandlerResult;
use crate::initial_invite::build_initial_call;
use crate::rule_chain::{rule_chain_turn, TurnInput};
use b2bua_sdk::event::CallEvent;

const RING_MS: i64 = 2_345;
/// The instant every turn here runs at.
const NOW_MS: i64 = 1_000;

/// A ringing call whose callee leg `b-1`, dialled under the tag `svc`, rings
/// under [`RING_MS`].
fn ringing_call() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut call = build_initial_call(
        &invite("w0", "w1", "response-facts"),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    );
    let mut b = call::testkit::dialled_leg("b-1", "svc");
    b.call_id = "bcid@x".into();
    call::helpers::add_b_leg(&mut call, b);
    let ring = Deadline::After(Duration::from_millis(RING_MS as u64));
    let (attempt, _) = call::helpers::open_attempt(&mut call, AttemptCause::Route, ring);
    call::helpers::join_attempt(&mut call, attempt, "b-1");
    call
}

/// The callee `bobtag`'s `status` on `b-1`'s INVITE.
fn provisional(status: u16, reason: &str) -> CallEvent {
    let raw = format!(
        "SIP/2.0 {status} {reason}\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Content-Length: 0\r\n\r\n"
    );
    let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(raw.as_bytes()) else {
        panic!("a response")
    };
    CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap())
}

/// One real turn of `event` from `b-1` over `call`, at [`NOW_MS`].
async fn turn(call: Call, event: &CallEvent) -> HandlerResult {
    let node = node("w0").await;
    let call_ref = call.call_ref.clone();
    let input = TurnInput {
        event,
        call_ref: &call_ref,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        resolved: Default::default(),
        matched_client_txn: true,
        now_ms: NOW_MS,
        owners: Default::default(),
    };
    let fw = node.core.router_ctx().framework();
    rule_chain_turn(&fw, call, input, &mut super::test_support::turn(1))
}

/// The ring deadline `call` holds on `b-1`.
fn deadline(call: &Call) -> Option<i64> {
    let key = TimerKey::leg(TimerType::NoAnswer, "b-1");
    call.timers.iter().find(|t| t.key == key).map(|t| t.fire_at)
}

#[tokio::test(start_paused = true)]
async fn a_183_rings_the_leg_and_arms_its_ring_from_the_turn() {
    let result = turn(ringing_call(), &provisional(183, "Session Progress")).await;
    assert_eq!(result.call.b_legs[0].ring, Ring::Rang);
    assert_eq!(deadline(&result.call), Some(NOW_MS + RING_MS));
}

#[tokio::test(start_paused = true)]
async fn a_199_rings_nothing_and_arms_nothing() {
    let result = turn(ringing_call(), &provisional(199, "Early Dialog Terminated")).await;
    assert_eq!(result.call.b_legs[0].ring, Ring::Silent);
    assert_eq!(deadline(&result.call), None);
}
