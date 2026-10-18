//! The ACK for a 2xx built from the 2xx alone (RFC 3261 §13.2.2.4): the
//! Request-URI is the 2xx's Contact, the route set its reversed
//! Record-Route, the Via the 2xx's top Via on a fresh branch, and From, To,
//! Call-ID and the CSeq number echo the 2xx.

use sip_message::generators::generate_ack_for_2xx_from_response;
use sip_message::header::HeaderName;
use sip_message::{CustomParser, SipMessage, SipParser, SipResponse};

fn response(record_route: &str, contact: &str) -> SipResponse {
    let raw = format!(
        "SIP/2.0 200 OK\r\n\
         Via: SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-reinvite\r\n\
         {record_route}\
         From: <sip:b2bua@192.0.2.10>;tag=local-1\r\n\
         To: <sip:bob@192.0.2.20>;tag=remote-1\r\n\
         Call-ID: call-1@192.0.2.10\r\n\
         CSeq: 7 INVITE\r\n\
         {contact}\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("a response") {
        SipMessage::Response(r) => r,
        SipMessage::Request(_) => panic!("expected a response"),
    }
}

#[test]
fn the_ack_follows_the_2xx_contact_and_its_reversed_record_route() {
    let resp = response(
        "Record-Route: <sip:192.0.2.30;lr>, <sip:192.0.2.40;lr>\r\n",
        "Contact: <sip:bob@192.0.2.20:5070>\r\n",
    );
    let ack = generate_ack_for_2xx_from_response(&resp, "z9hG4bK-ack").expect("an ACK");
    assert_eq!(ack.method().as_str(), "ACK");
    assert_eq!(ack.request_uri().to_string(), "sip:bob@192.0.2.20:5070");
    let routes: Vec<String> = ack.raw_text(HeaderName::Route).map(|r| r.to_string()).collect();
    assert_eq!(routes.join(", "), "<sip:192.0.2.40;lr>, <sip:192.0.2.30;lr>");
    assert_eq!(ack.top_via().branch(), Some("z9hG4bK-ack"));
    assert_eq!(ack.top_via().sent_by().host(), "192.0.2.10");
    assert_eq!(ack.cseq().seq(), 7);
    assert_eq!(ack.cseq().method().as_str(), "ACK");
    assert_eq!(ack.call_id().as_str(), "call-1@192.0.2.10");
    assert_eq!(ack.to().tag(), Some("remote-1"));
    assert_eq!(ack.from().tag(), Some("local-1"));
}

#[test]
fn a_2xx_with_no_contact_draws_no_ack() {
    let resp = response("", "");
    assert!(generate_ack_for_2xx_from_response(&resp, "z9hG4bK-ack").is_none());
}
