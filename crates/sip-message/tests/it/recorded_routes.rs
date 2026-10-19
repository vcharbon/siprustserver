//! The recorded route set refuses a route no request can be addressed to: a
//! Record-Route URI that reads as a name-addr but fails the Request-URI checks
//! (RFC 3261 §12.2.1.1 makes a strict hop the Request-URI).

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipResponse};

fn answer_recording(route: &str) -> SipResponse {
    let raw = format!(
        "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-rr\r\n\
         From: <sip:alice@192.0.2.1>;tag=a1\r\nTo: <sip:bob@192.0.2.2>;tag=b1\r\n\
         Call-ID: rr-1\r\nCSeq: 1 INVITE\r\nContact: <sip:bob@192.0.2.2:5070>\r\n\
         Record-Route: {route}\r\nContent-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()) {
        Ok(SipMessage::Response(r)) => r,
        other => panic!("the strict reader takes a 200 recording {route:?}: {other:?}"),
    }
}

#[test]
fn a_recorded_route_that_is_no_request_target_refuses_the_set() {
    for route in [
        "<sip:10.0.0.9:5060x>",
        "<sip:@>",
        "<sip:10.0.0.\u{7f}9>",
        "<sip:192.0.2.7;lr>, <sip:10.0.0.9:5060x>",
    ] {
        let answer = answer_recording(route);
        assert!(answer.record_route_set().is_err(), "{route:?} is no route set");
    }
}

#[test]
fn an_addressable_recorded_route_reads() {
    let answer = answer_recording("<sip:192.0.2.7;lr>, <sip:192.0.2.8:5062>");
    let set = answer.record_route_set().expect("a route set");
    assert_eq!(set.iter().count(), 2);
}
