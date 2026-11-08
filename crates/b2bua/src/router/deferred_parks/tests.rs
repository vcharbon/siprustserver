use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::DeferredParks;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, src};
use b2bua_sdk::event::CallEvent;

/// A ringing call of `w0`'s own, as the bulk reclaim scanned it.
fn scanned(cid: &str) -> call::Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    build_initial_call(&invite("w0", "w1", cid), src(), &config, &mut ids, 0)
}

#[test]
fn a_deferred_calls_invite_copies_are_held_until_its_arming_then_follow_it_until_it_ran() {
    let mut parks = DeferredParks::default();
    let call = scanned("deferred");
    parks.defer(call.clone());

    let copied = parks.copied(&invite("w0", "w1", "deferred")).expect("a copy of its INVITE");
    assert_eq!(copied.hit.call_ref, call.call_ref);
    assert!(!copied.arm_queued, "held while no arming is queued");
    let image = String::from_utf8_lossy(invite("w0", "w1", "deferred").image()).to_string();
    let other = image.replace("CSeq: 1 INVITE", "CSeq: 2 INVITE");
    let Ok(SipMessage::Request(other)) = CustomParser::new().parse(other.as_bytes()) else {
        panic!("the INVITE parses");
    };
    assert!(parks.copied(&other).is_none(), "another CSeq is no copy");
    assert_eq!(parks.unarmed().count(), 1);

    let event = CallEvent::sip(SipMessage::Request(invite("w0", "w1", "deferred")), src());
    parks.hold(&call.call_ref, event);
    assert_eq!(parks.arming(&call.call_ref).len(), 1, "the held copy is taken in again");
    assert!(parks.copied(&invite("w0", "w1", "deferred")).expect("still deferred").arm_queued);
    assert_eq!(parks.unarmed().count(), 0);

    parks.held_back(&call.call_ref);
    assert!(!parks.copied(&invite("w0", "w1", "deferred")).expect("deferred").arm_queued);

    parks.armed(&call.call_ref);
    assert!(parks.copied(&invite("w0", "w1", "deferred")).is_none(), "no longer deferred");
}
