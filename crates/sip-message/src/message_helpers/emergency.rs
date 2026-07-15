//! Emergency-call classification via Resource-Priority (RFC 4412): over a
//! parsed request ([`is_emergency_request`]) and over the raw datagram
//! before any parse ([`buffer_has_emergency_marker`]) — the signal the
//! Tier-1 overload brake consults to NEVER 503 an emergency packet.

use super::bytes::find_subslice;
use super::headers::get_header;
use crate::types::SipRequest;

/// The canonical emergency Resource-Priority namespace.value tokens. Matched
/// case-SENSITIVELY — canonical casing is the upstream stamping contract.
const EMERGENCY_RPH_TOKENS: [&str; 3] = ["esnet.0", "wps.0", "q735.0"];

/// Whether a request carries an emergency Resource-Priority header
/// (esnet.0 / wps.0 / q735.0). Case-insensitive name, case-sensitive value.
pub fn is_emergency_request(req: &SipRequest) -> bool {
    match get_header(&req.headers, "resource-priority") {
        None => false,
        Some(value) => EMERGENCY_RPH_TOKENS.iter().any(|tok| value.contains(tok)),
    }
}

/// Cheap byte scan: does the raw datagram carry an emergency signal?
///
/// Runs on the RAW datagram *before* the SIP parser, in the Tier-1 UDP
/// brake's hot path — an allocation-free byte scan, not a parse. Two
/// signals, cheapest first:
///   1. the dispatcher-side markers `;emerg=1` (Request-URI) or `;em=1` (Via
///      custom param) that the B2BUA stamps once an emergency call is
///      admitted (see `b2bua::stack_identity`), which every subsequent
///      in-dialog packet carries — a plain substring match anywhere in the
///      buffer; then
///   2. an **initial** INVITE's `Resource-Priority` header (case-sensitive
///      canonical name) whose value contains one of the canonical emergency
///      tokens (`esnet.0` / `wps.0` / `q735.0`). The token scan is confined
///      to that header *field* — tolerant of HCOLON whitespace before the
///      colon and of obs-fold continuation lines (RFC 3261 §7.3.1), and
///      recognising both CRLF and bare-LF line endings — so a token on a
///      *different* header line or in the body cannot spoof an emergency.
pub fn buffer_has_emergency_marker(raw: &[u8]) -> bool {
    // Cheap path: dispatcher-side markers stamped on admitted calls.
    if find_subslice(raw, b";emerg=1").is_some() {
        return true;
    }
    if find_subslice(raw, b";em=1").is_some() {
        return true;
    }

    // Initial INVITE: Resource-Priority header (case-sensitive canonical name).
    // Scan every occurrence of the canonical name — a textual hit is not
    // guaranteed to be the actual header (it could be a substring elsewhere) —
    // and for each that forms a real header (optional LWS then a colon),
    // confine the token scan to that header's *field* via [`header_field_end`].
    const RP_NAME: &[u8] = b"Resource-Priority";
    let mut from = 0;
    while let Some(rel) = find_subslice(&raw[from..], RP_NAME) {
        let name_at = from + rel;
        let after_name = name_at + RP_NAME.len();
        from = after_name; // always advances (RP_NAME is non-empty) → no infinite loop

        // HCOLON: optional whitespace then a mandatory colon. Anything else means
        // this textual hit was not the header name (skip it, keep scanning).
        let mut j = after_name;
        while j < raw.len() && (raw[j] == b' ' || raw[j] == b'\t') {
            j += 1;
        }
        if j >= raw.len() || raw[j] != b':' {
            continue;
        }
        let field = &raw[j + 1..header_field_end(raw, j + 1)];
        if EMERGENCY_RPH_TOKENS.iter().any(|tok| find_subslice(field, tok.as_bytes()).is_some()) {
            return true;
        }
    }
    false
}

/// Exclusive end index of the header field that begins at `start`, honouring
/// obs-fold continuation lines (a CRLF/LF immediately followed by SP/HTAB
/// continues the field — RFC 3261 §7.3.1) and recognising both CRLF and
/// bare-LF line endings. The returned span is the field content only (a
/// terminating CR is excluded), so the caller scans the whole logical value
/// — including a folded continuation — while a token on the *next* header
/// line or in the body stays out of range.
fn header_field_end(raw: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < raw.len() {
        if raw[i] == b'\n' {
            // Line ended. A leading SP/HTAB on the next line folds it into this
            // field; otherwise the field ends at this line terminator.
            match raw.get(i + 1) {
                Some(b' ') | Some(b'\t') => {
                    i += 1;
                    continue;
                }
                _ => return if i > start && raw[i - 1] == b'\r' { i - 1 } else { i },
            }
        }
        i += 1;
    }
    raw.len()
}

#[cfg(test)]
mod parsed_tests {
    //! Pins [`is_emergency_request`]: case-insensitive header lookup,
    //! case-sensitive value match, substring match against the three
    //! canonical RPH tokens, `false` when absent.

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
    fn value_match_is_case_sensitive() {
        // Canonical casing is required by the contract — an upper-cased token
        // must NOT be treated as emergency.
        let req = invite_with_rph(Some("Resource-Priority"), Some("ESNET.0"));
        assert!(!is_emergency_request(&req));
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
    fn token_matches_as_substring_among_multiple() {
        // A canonical token embedded in a multi-namespace RPH value still flags.
        let req = invite_with_rph(Some("Resource-Priority"), Some("dsn.flash, q735.0"));
        assert!(is_emergency_request(&req));
    }
}

#[cfg(test)]
mod buffer_tests {
    //! Pins the byte-scan contract of [`buffer_has_emergency_marker`],
    //! including the brake-integration cases: an emergency INVITE bypasses,
    //! a plain INVITE does not, the stamped markers match anywhere, and the
    //! RPH token scan is field-confined (fold- and LF-tolerant).

    use super::buffer_has_emergency_marker;

    /// Build a minimal INVITE datagram. `rph` is the optional
    /// `(header name, value)` pair — the name varies to pin case-sensitivity.
    fn invite_buf(rph: Option<(&str, &str)>) -> Vec<u8> {
        let rph_line = match rph {
            Some((name, value)) => format!("{name}: {value}\r\n"),
            None => String::new(),
        };
        format!(
            "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-brake-0\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag-0\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: brake-test-0@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.1:5555>\r\n\
Max-Forwards: 70\r\n\
{rph_line}\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn each_canonical_rph_token_flags_emergency() {
        for tok in ["esnet.0", "wps.0", "q735.0"] {
            let buf = invite_buf(Some(("Resource-Priority", tok)));
            assert!(buffer_has_emergency_marker(&buf), "{tok} should flag emergency");
        }
    }

    #[test]
    fn plain_invite_is_not_emergency() {
        assert!(!buffer_has_emergency_marker(&invite_buf(None)));
    }

    #[test]
    fn rph_header_name_is_case_sensitive() {
        // Canonical-casing contract: a lower-cased header name is not matched.
        let buf = invite_buf(Some(("resource-priority", "esnet.0")));
        assert!(!buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn rph_token_match_is_case_sensitive() {
        let buf = invite_buf(Some(("Resource-Priority", "ESNET.0")));
        assert!(!buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn non_emergency_rph_value_is_not_emergency() {
        let buf = invite_buf(Some(("Resource-Priority", "dsn.flash")));
        assert!(!buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn rph_token_matches_as_substring_among_multiple_namespaces() {
        let buf = invite_buf(Some(("Resource-Priority", "dsn.flash, q735.0")));
        assert!(buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn token_on_a_different_line_than_rph_does_not_flag() {
        // A canonical token elsewhere in the message (here a Subject header)
        // must NOT count — the scan is confined to the Resource-Priority line.
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-x\r\n\
Subject: priority esnet.0 please\r\n\
Resource-Priority: dsn.flash\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_emergency_marker(raw.as_bytes()));
    }

    #[test]
    fn unterminated_rph_line_still_scans_to_end() {
        // No trailing CRLF after the value: the field scan runs to the end of
        // the buffer and still finds the token.
        let raw = b"INVITE sip:bob SIP/2.0\r\nResource-Priority: wps.0";
        assert!(buffer_has_emergency_marker(raw));
    }

    #[test]
    fn stamped_via_em_marker_flags_anywhere() {
        // `;em=1` is the Via custom param b2bua::stack_identity stamps; it sits
        // on the Via line, not in the Request-URI — must match anywhere.
        let raw = b"BYE sip:bob@127.0.0.1 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-9;em=1\r\n\
Content-Length: 0\r\n\r\n";
        assert!(buffer_has_emergency_marker(raw));
    }

    #[test]
    fn stamped_requri_emerg_marker_flags_anywhere() {
        // `;emerg=1` is the Request-URI param stamped on admitted emergency
        // calls; a later in-dialog request (no RPH header at all) carries it.
        let raw = b"ACK sip:b2bua@127.0.0.1:5060;emerg=1 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-3\r\n\
Content-Length: 0\r\n\r\n";
        assert!(buffer_has_emergency_marker(raw));
    }

    #[test]
    fn in_dialog_request_without_markers_or_rph_is_not_emergency() {
        let raw = b"OPTIONS sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-opt\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_emergency_marker(raw));
    }

    #[test]
    fn empty_buffer_is_not_emergency() {
        assert!(!buffer_has_emergency_marker(b""));
    }

    #[test]
    fn short_buffer_below_needle_length_is_not_emergency() {
        assert!(!buffer_has_emergency_marker(b"INV"));
    }

    // --- robust Resource-Priority detection ---------------------------------
    // A naive single-`indexOf` + CRLF-only scan sheds a genuine emergency whose
    // header is whitespace-padded or obs-folded, and mis-confines on bare-LF
    // datagrams. These pin the hardened contract.

    #[test]
    fn rph_whitespace_before_colon_still_flags() {
        // HCOLON allows LWS before the colon (RFC 3261 §7.3.1): a real emergency
        // INVITE so written must NOT be shed.
        let buf = invite_buf(Some(("Resource-Priority ", "esnet.0")));
        assert!(buffer_has_emergency_marker(&buf));
        let buf = invite_buf(Some(("Resource-Priority\t", "wps.0")));
        assert!(buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn rph_obs_fold_continuation_value_still_flags() {
        // The value sits on a folded continuation line (CRLF + leading SP). The
        // field scan must follow the fold and still find the canonical token.
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-fold\r\n\
Resource-Priority:\r\n esnet.0\r\n\
Content-Length: 0\r\n\r\n";
        assert!(buffer_has_emergency_marker(raw));
    }

    #[test]
    fn lf_only_token_on_other_line_does_not_spoof() {
        // Bare-LF datagram: the canonical token is on a Subject line, the RP
        // header carries a non-emergency value. Line-confinement must hold.
        let raw = b"INVITE sip:bob SIP/2.0\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-lf\n\
Subject: priority esnet.0 please\n\
Resource-Priority: dsn.flash\n\
Content-Length: 0\n\n";
        assert!(!buffer_has_emergency_marker(raw));
    }

    #[test]
    fn lf_only_emergency_rph_still_flags() {
        let raw = b"INVITE sip:bob SIP/2.0\n\
Resource-Priority: q735.0\n\
Content-Length: 0\n\n";
        assert!(buffer_has_emergency_marker(raw));
    }

    #[test]
    fn bare_name_without_colon_is_not_a_header() {
        // The canonical name appearing without a following colon (e.g. echoed in
        // a Subject) is not the header and must not be scanned as one.
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Subject: Resource-Priority esnet.0 mention\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_emergency_marker(raw));
    }
}
