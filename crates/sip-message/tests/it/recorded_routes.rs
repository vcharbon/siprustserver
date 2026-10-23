//! A Record-Route recording a route no request can be addressed to refuses the
//! message that carries it: a Record-Route URI that fails the grammar or the
//! Request-URI checks (RFC 3261 §12.2.1.1 makes a strict hop the
//! Request-URI) is read at parse time.

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipResponse};

fn answer_recording(route: &str) -> Result<SipResponse, String> {
    let raw = format!(
        "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.1:5060;branch=z9hG4bK-rr\r\n\
         From: <sip:alice@192.0.2.1>;tag=a1\r\nTo: <sip:bob@192.0.2.2>;tag=b1\r\n\
         Call-ID: rr-1\r\nCSeq: 1 INVITE\r\nContact: <sip:bob@192.0.2.2:5070>\r\n\
         Record-Route: {route}\r\nContent-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()) {
        Ok(SipMessage::Response(r)) => Ok(r),
        Ok(SipMessage::Request(_)) => Err("read as a request".to_string()),
        Err(e) => Err(e.reason),
    }
}

#[test]
fn a_recorded_route_that_is_no_request_target_refuses_the_message() {
    for route in [
        "<sip:10.0.0.9:5060x>",
        "<sip:@>",
        "<sip:10.0.0.\u{7f}9>",
        "<sip:192.0.2.7;lr>, <sip:10.0.0.9:5060x>",
        "<sip:192.0.2.7;lr?Subject=x>",
    ] {
        assert!(answer_recording(route).is_err(), "{route:?} is no route set");
    }
}

#[test]
fn an_addressable_recorded_route_reads() {
    let answer = answer_recording("<sip:192.0.2.7;lr>, <sip:192.0.2.8:5062>").expect("a 200");
    let set = answer.record_route_set().expect("a route set");
    assert_eq!(set.iter().count(), 2);
}
