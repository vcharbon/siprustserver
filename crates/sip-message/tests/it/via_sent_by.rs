//! The Via sent-by port check reads the sent-by alone (RFC 3261 §20.42:
//! `sent-by = host [COLON port]`, `host` an IPv6 reference in brackets): a
//! group of an IPv6 address is never taken for a port, in the sent-by or in
//! a `received`/`maddr` parameter, while a port followed by letters is still
//! refused.

use sip_message::{CustomParser, SipMessage, SipParser};

fn request(via: &str) -> Result<SipMessage, sip_message::SipParseError> {
    let text = format!(
        "OPTIONS sip:bob@example.com SIP/2.0\r\nVia: {via}\r\nMax-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=1\r\nTo: <sip:bob@example.com>\r\nCall-ID: a@b\r\n\
         CSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n"
    );
    CustomParser::new().parse(text.as_bytes())
}

#[test]
fn an_ipv6_sent_by_whose_group_starts_with_digits_parses() {
    for via in [
        "SIP/2.0/UDP [2001:db8:1:6d0::5]:5060;branch=z9hG4bK1",
        "SIP/2.0/UDP [2001:db8:1:6d0::5];branch=z9hG4bK1",
        "SIP/2.0/UDP host.example.com:5060;received=2001:db8:1:6d0::5;branch=z9hG4bK1",
        "SIP/2.0/UDP host.example.com;maddr=2001:db8:0:7ab::1;branch=z9hG4bK1",
    ] {
        assert!(request(via).is_ok(), "{via}: {:?}", request(via).err().map(|e| e.reason));
    }
}

#[test]
fn a_port_followed_by_letters_is_still_refused() {
    for via in [
        "SIP/2.0/UDP host.example.com:5060abc;branch=z9hG4bK1",
        "SIP/2.0/UDP [2001:db8::5]:5060x;branch=z9hG4bK1",
    ] {
        let err = request(via).expect_err(via);
        assert!(err.reason.contains("Trailing non-digit after Via port"), "{}", err.reason);
    }
}
