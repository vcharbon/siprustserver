//! The ACK of a 2xx built from the INVITE alone (RFC 3261 §13.2.2.4) carries
//! the answer it is given as `application/sdp` (RFC 3264 §4), and no body
//! without one.

use crate::generated::generate_response;
use sip_message::generators::{generate_ack_for_2xx_from_invite, GenerateResponseOpts};
use sip_message::header::HeaderName;
use sip_message::{hydrate_request, SipHeader, SipRequest};

const ANSWER: &str = "v=0\r\no=b2bua 1 1 IN IP4 10.0.0.1\r\ns=-\r\nc=IN IP4 10.0.0.1\r\nt=0 0\r\nm=audio 0 RTP/AVP 0\r\na=inactive\r\n";

fn hdr(name: &str, value: &str) -> SipHeader {
    SipHeader::new(name, value)
}

/// An INVITE carrying no offer (RFC 3261 §13.2.1).
fn offerless_invite() -> SipRequest {
    let headers = vec![
        hdr("Via", "SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKdelayed"),
        hdr("Max-Forwards", "70"),
        hdr("From", "<sip:b2bua@10.0.0.1:5060>;tag=b2bua-local"),
        hdr("To", "<sip:bob@192.0.2.20:5060>"),
        hdr("Call-ID", "call-delayed-1"),
        hdr("CSeq", "1 INVITE"),
        hdr("Contact", "<sip:b2bua@10.0.0.1:5060>"),
        hdr("Content-Length", "0"),
    ];
    hydrate_request("INVITE", "sip:bob@192.0.2.20:5060", headers, Vec::new())
        .expect("the INVITE hydrates")
}

fn content_type(ack: &SipRequest) -> Option<&str> {
    ack.raw(HeaderName::ContentType).next()
}

#[test]
fn an_ack_built_from_the_invite_carries_the_answer_it_is_given() {
    let invite = offerless_invite();
    let opts = GenerateResponseOpts { to_tag: Some("uas-tag".to_string()), ..Default::default() };
    let ok = generate_response(&invite, 200, "OK", &opts);

    let answer = ANSWER.as_bytes().to_vec();
    let answered = generate_ack_for_2xx_from_invite(&invite, &ok, "z9hG4bKack", Some(answer))
        .expect("the answered ACK builds");
    assert_eq!(content_type(&answered), Some("application/sdp"));
    assert_eq!(answered.sdp(), Some(ANSWER.as_bytes()));

    let bare = generate_ack_for_2xx_from_invite(&invite, &ok, "z9hG4bKack", None)
        .expect("the bare ACK builds");
    assert!(bare.body().is_empty());
    assert_eq!(content_type(&bare), None);
    assert_eq!(bare.raw(HeaderName::ContentLength).next(), Some("0"));
}
