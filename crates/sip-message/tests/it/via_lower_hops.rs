//! The transport allowlist and the port-0 refusal judge the hop a message came
//! from, the topmost Via. A lower Via records an earlier hop: RFC 3261 §25.1
//! admits any `other-transport` there and RFC 4475 §3.1.1.10 has parsers
//! accept it, so a lower hop over a transport this element does not speak, or
//! one naming port 0, leaves the message readable.

use sip_message::{CustomParser, SipMessage, SipParser};

const TOP: &str = "SIP/2.0/UDP 192.0.2.10:5060;branch=z9hG4bK-top-1";

fn request(vias: &[&str]) -> Result<SipMessage, sip_message::SipParseError> {
    let via_lines: String = vias.iter().map(|v| format!("Via: {v}\r\n")).collect();
    let text = format!(
        "OPTIONS sip:bob@example.com SIP/2.0\r\n{via_lines}Max-Forwards: 70\r\n\
         From: <sip:alice@example.com>;tag=1\r\nTo: <sip:bob@example.com>\r\nCall-ID: a@b\r\n\
         CSeq: 1 OPTIONS\r\nContent-Length: 0\r\n\r\n"
    );
    CustomParser::new().parse(text.as_bytes())
}

#[test]
fn a_lower_via_over_another_transport_is_read() {
    for (lower, transport) in [
        ("SIP/2.0/TLS-SCTP origin.example.com;branch=z9hG4bK-origin-1", "TLS-SCTP"),
        ("SIP/2.0/UNKNOWN origin.example.com;branch=z9hG4bK-origin-1", "UNKNOWN"),
    ] {
        let msg = request(&[TOP, lower]).unwrap_or_else(|e| panic!("{lower}: {}", e.reason));
        let transports: Vec<&str> = msg.via().iter().map(|v| v.transport()).collect();
        assert_eq!(transports, ["UDP", transport]);
    }
}

#[test]
fn a_lower_via_naming_port_zero_is_read() {
    let lower = "SIP/2.0/UDP origin.example.com:0;branch=z9hG4bK-origin-1";
    let msg = request(&[TOP, lower]).unwrap_or_else(|e| panic!("{lower}: {}", e.reason));
    assert_eq!(msg.via().iter().nth(1).and_then(|v| v.port()), Some(0));
}

#[test]
fn the_topmost_via_still_meets_the_allowlist_and_a_real_port() {
    for (top, why) in [
        ("SIP/2.0/TLS-SCTP 192.0.2.10;branch=z9hG4bK-top-1", "not in allowed set"),
        ("SIP/2.0/UDP 192.0.2.10:0;branch=z9hG4bK-top-1", "Via port out of range: 0"),
    ] {
        let err = request(&[top]).expect_err(top);
        assert!(err.reason.contains(why), "{top}: {}", err.reason);
    }
}
