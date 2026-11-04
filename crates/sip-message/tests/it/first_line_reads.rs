//! A header RFC 3261 §7.3.1 allows once is read from its first line: a later
//! line of it, malformed or not, takes no part in the value a reader sees, on
//! a parsed message and on a draft alike. Reading every value still sees it.

use sip_message::draft::RequestDraft;
use sip_message::header::{self, CSeq, CallId, Expires, HeaderValue, MaxForwards, To, Uri, Via};
use sip_message::{CustomParser, HeaderName, Method, SipMessage, SipParser};

#[test]
fn a_parsed_message_reads_a_single_header_from_its_first_line() {
    let raw = "SUBSCRIBE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.5:5060;branch=z9hG4bK-a\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: c1\r\n\
CSeq: 1 SUBSCRIBE\r\n\
Contact: <sip:alice@192.0.2.5>\r\n\
Event: presence\r\n\
Event: ;id=1\r\n\
Content-Length: 0\r\n\r\n";
    let SipMessage::Request(request) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
        panic!("a request")
    };
    let event = request.header::<header::Event>().expect("carried").expect("the first line reads");
    assert_eq!(event.to_wire(), "presence");
    assert!(request.list::<header::Event>().is_err(), "the second line is malformed");
}

#[test]
fn a_draft_reads_a_single_header_from_its_first_line() {
    let draft = RequestDraft::new(Method::Options, Uri::sip("example.com"))
        .push(Via::udp("10.0.0.1", 5060).with_branch("z9hG4bK-1"))
        .push(MaxForwards::new(70))
        .push(header::From::from_uri(Uri::sip("example.com")).with_tag("1"))
        .push(To::from_uri(Uri::sip("example.com")))
        .push(CallId::new("c1"))
        .push(CSeq::new(1, Method::Options))
        .push_raw(HeaderName::Expires, "60")
        .push_raw(HeaderName::Expires, "abc");
    let expires = draft.header::<Expires>().expect("carried").expect("the first line reads");
    assert_eq!(expires.value(), 60);
    assert!(draft.values::<Expires>().is_err(), "the second line is malformed");
}
