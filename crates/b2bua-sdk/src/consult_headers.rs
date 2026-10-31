//! The headers a decision consult forwards from a SIP message: every wire line
//! but the structural ones, which a consult carries as typed fields when it
//! carries them at all.

use sip_message::header::HeaderName;
use sip_message::SipHeader;

/// The structural headers no consult forwards as a header line. `Max-Forwards`
/// is not one: a decision that cannot see the hop count cannot answer a spent
/// one.
pub const STANDARD_HEADERS: &[HeaderName] = &[
    HeaderName::From,
    HeaderName::To,
    HeaderName::Via,
    HeaderName::Contact,
    HeaderName::ContentType,
    HeaderName::CallId,
    HeaderName::CSeq,
    HeaderName::ContentLength,
];

/// `headers` without the [`STANDARD_HEADERS`], one `(name, value)` per wire
/// line in wire order and wire spelling: the order of a multi-line header is
/// part of its meaning (RFC 7044 `History-Info` index chains).
pub fn consult_headers(headers: &[SipHeader]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|h| !STANDARD_HEADERS.iter().any(|n| n.matches(&h.name)))
        .map(|h| (h.name.to_string(), h.value.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::{CustomParser, SipMessage, SipParser};

    #[test]
    fn a_final_keeps_its_non_structural_lines_in_wire_order() {
        let raw = "SIP/2.0 486 Busy Here\r\n\
                   v: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK1\r\n\
                   From: <sip:a@192.0.2.1>;tag=a\r\nTo: <sip:b@192.0.2.2>;tag=b\r\n\
                   i: c1\r\nCSeq: 1 INVITE\r\n\
                   Reason: Q.850;cause=17\r\nWarning: 399 x \"y\"\r\n\
                   Reason: SIP;cause=486\r\nMax-Forwards: 70\r\nl: 0\r\n\r\n";
        let SipMessage::Response(resp) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
            unreachable!()
        };
        assert_eq!(
            consult_headers(resp.headers()),
            [
                ("Reason".to_string(), "Q.850;cause=17".to_string()),
                ("Warning".to_string(), "399 x \"y\"".to_string()),
                ("Reason".to_string(), "SIP;cause=486".to_string()),
                ("Max-Forwards".to_string(), "70".to_string()),
            ]
        );
    }
}
