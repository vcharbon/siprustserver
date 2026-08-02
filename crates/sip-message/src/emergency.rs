//! Emergency-call classification via Resource-Priority (RFC 4412) over a
//! parsed request — the signal both overload tiers consult to NEVER reject an
//! emergency call.

use crate::header::HeaderName;
use crate::types::SipRequest;

/// The emergency Resource-Priority r-values (RFC 4412 namespace.priority).
/// Compared ASCII-case-insensitively, as RFC 4412 r-values are. Shared with
/// the raw-datagram classifier in [`crate::sniff`] so the two sides can never
/// disagree on what counts as emergency.
pub(crate) const EMERGENCY_RPH_TOKENS: [&str; 3] = ["esnet.0", "wps.0", "q735.0"];

/// Whether a request carries an emergency Resource-Priority header
/// (esnet.0 / wps.0 / q735.0). Every `Resource-Priority` header is checked;
/// each value is read as the comma-separated r-value list of RFC 4412 and
/// each r-value compared whole (trimmed, case-insensitive) against the
/// emergency tokens.
pub fn is_emergency_request(req: &SipRequest) -> bool {
    req.raw(HeaderName::ResourcePriority).any(|value| {
        value
            .split(',')
            .any(|rv| EMERGENCY_RPH_TOKENS.iter().any(|tok| rv.trim().eq_ignore_ascii_case(tok)))
    })
}

#[cfg(test)]
mod parsed_tests {
    //! Pins [`is_emergency_request`]: case-insensitive header lookup and
    //! r-value match, whole-r-value comparison over the comma-split list,
    //! every Resource-Priority header checked, `false` when absent.

    use super::is_emergency_request;
    use crate::parser::SipParser;
    use crate::parser::custom::CustomParser;
    use crate::types::SipMessage;

    /// Parse a minimal INVITE carrying the given `Resource-Priority` header;
    /// `None` omits the header entirely.
    fn invite_with_rph(header_name: Option<&str>, value: Option<&str>) -> crate::types::SipRequest {
        let rph_line = match (header_name, value) {
            (Some(name), Some(v)) => format!("{name}: {v}\r\n"),
            _ => String::new(),
        };
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-emerg\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=emerg-from\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: emerg-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
{rph_line}\
Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture INVITE should parse") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected request"),
        }
    }

    #[test]
    fn each_canonical_token_is_emergency() {
        for tok in ["esnet.0", "wps.0", "q735.0"] {
            let req = invite_with_rph(Some("Resource-Priority"), Some(tok));
            assert!(is_emergency_request(&req), "{tok} should be emergency");
        }
    }

    #[test]
    fn header_name_lookup_is_case_insensitive() {
        let req = invite_with_rph(Some("resource-priority"), Some("esnet.0"));
        assert!(is_emergency_request(&req));
    }

    #[test]
    fn value_match_is_case_insensitive() {
        // RFC 4412 r-values are case-insensitive — an upper-cased token still
        // classifies as emergency.
        let req = invite_with_rph(Some("Resource-Priority"), Some("ESNET.0"));
        assert!(is_emergency_request(&req));
    }

    #[test]
    fn absent_header_is_not_emergency() {
        let req = invite_with_rph(None, None);
        assert!(!is_emergency_request(&req));
    }

    #[test]
    fn non_emergency_priority_value_is_not_emergency() {
        let req = invite_with_rph(Some("Resource-Priority"), Some("dsn.flash"));
        assert!(!is_emergency_request(&req));
    }

    #[test]
    fn token_matches_among_multiple_namespaces() {
        // An emergency r-value anywhere in the comma-separated list flags.
        let req = invite_with_rph(Some("Resource-Priority"), Some("dsn.flash, q735.0"));
        assert!(is_emergency_request(&req));
    }

    #[test]
    fn embedded_token_is_not_an_r_value() {
        // r-values are compared whole (comma-split, trimmed): a token embedded
        // in a longer r-value does not classify.
        let req = invite_with_rph(Some("Resource-Priority"), Some("esnet.01"));
        assert!(!is_emergency_request(&req));
    }

    #[test]
    fn every_resource_priority_header_is_checked() {
        // Two Resource-Priority headers; only the second carries an emergency
        // r-value — the request is still emergency.
        let raw = "INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-emerg2\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=emerg-from\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: emerg-call2@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Resource-Priority: dsn.flash\r\n\
Resource-Priority: wps.0\r\n\
Content-Length: 0\r\n\r\n";
        let req = match CustomParser::new()
            .parse(raw.as_bytes())
            .expect("fixture INVITE should parse")
        {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected request"),
        };
        assert!(is_emergency_request(&req));
    }
}
