use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use super::DeferredParks;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, src};

/// A ringing call of `w0`'s own, as the bulk reclaim scanned it.
fn scanned(cid: &str) -> call::Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let mut ids = sip_ids::TurnIds::fixed(1);
    build_initial_call(&invite("w0", "w1", cid), src(), &config, &mut ids, 0)
}

#[test]
fn a_deferred_calls_invite_copies_are_its_own_until_its_arming_ran() {
    let mut parks = DeferredParks::default();
    let call = scanned("deferred");
    parks.defer(call.clone());

    assert!(parks.copies(&invite("w0", "w1", "deferred")), "a copy of its INVITE");
    assert!(parks.invite_here(&call.call_ref, 1), "its INVITE's CSeq");
    let image = String::from_utf8_lossy(invite("w0", "w1", "deferred").image()).to_string();
    let other = image.replace("CSeq: 1 INVITE", "CSeq: 2 INVITE");
    let Ok(SipMessage::Request(other)) = CustomParser::new().parse(other.as_bytes()) else {
        panic!("the INVITE parses");
    };
    assert!(!parks.copies(&other), "another CSeq is no copy");
    assert!(!parks.invite_here(&call.call_ref, 2));
    assert_eq!(parks.unarmed().count(), 1);

    parks.arming(&call.call_ref);
    assert_eq!(parks.unarmed().count(), 0, "its arming is queued");
    parks.held_back(&call.call_ref);
    assert_eq!(parks.unarmed().count(), 1, "held back, it waits for the next");

    parks.armed(&call.call_ref);
    assert!(!parks.copies(&invite("w0", "w1", "deferred")), "no longer deferred");
}
