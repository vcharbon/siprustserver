use call::IndexLookup;
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
fn a_deferred_call_is_found_by_its_keys_and_its_invite_copies_until_its_arming_ran() {
    let mut parks = DeferredParks::default();
    let call = scanned("deferred");
    parks.defer(call.clone());

    let lookup = IndexLookup::Peer { call_id: &call.a_leg.call_id, tag: "alicetag" };
    let found = parks.find(lookup).expect("found by the caller's dialog key");
    assert_eq!(found.call_ref, call.call_ref);
    let copied = parks.copied(&invite("w0", "w1", "deferred")).expect("a copy of its INVITE");
    assert_eq!(copied.hit.call_ref, call.call_ref);
    assert!(copied.unanswered && !copied.arm_queued);
    let image = String::from_utf8_lossy(invite("w0", "w1", "deferred").image()).to_string();
    let other = image.replace("CSeq: 1 INVITE", "CSeq: 2 INVITE");
    let Ok(SipMessage::Request(other)) = CustomParser::new().parse(other.as_bytes()) else {
        panic!("the INVITE parses");
    };
    assert!(parks.copied(&other).is_none(), "another CSeq is no copy");

    let event = CallEvent::sip(SipMessage::Request(invite("w0", "w1", "deferred")), src());
    parks.hold(&call.call_ref, event);
    assert_eq!(parks.arming(&call.call_ref, true).len(), 1, "the held copy is taken in again");
    assert!(parks.copied(&invite("w0", "w1", "deferred")).expect("still deferred").arm_queued);

    parks.armed(&call.call_ref);
    assert!(parks.find(lookup).is_none(), "its keys go with it");
    assert!(parks.copied(&invite("w0", "w1", "deferred")).is_none());
}

#[test]
fn a_call_whose_arming_was_discarded_is_no_longer_deferred() {
    let mut parks = DeferredParks::default();
    let call = scanned("discarded");
    parks.defer(call.clone());
    let event = CallEvent::sip(SipMessage::Request(invite("w0", "w1", "discarded")), src());
    parks.hold(&call.call_ref, event);

    assert_eq!(parks.arming(&call.call_ref, false).len(), 1, "what it held is taken in again");
    assert!(parks.copied(&invite("w0", "w1", "discarded")).is_none());
}
