//! MessageHelpers — SIP header accessors + structured-header readers. Port of
//! the **pure** half of `src/sip/MessageHelpers.ts`: it reads and rewrites
//! existing headers rather than constructing new ones (construction lives in
//! [`crate::generators`]).
//!
//! One byte-level dispatcher helper is ported ahead of slice 2 because the
//! Tier-1 overload brake's emergency bypass depends on it (see the byte-helpers
//! section at the bottom):
//!   - [`buffer_has_emergency_marker`] — the cheap pre-parse signal the Tier-1
//!     UDP brake uses to NEVER 503 an emergency packet.
//!
//! Deferred to slice 2 (network/dispatch), NOT ported here:
//!   - the identifier generators (`newTag`/`newBranch`/`newCallId`/`currentRng`)
//!     — they read a fiber-local seeded RNG (Effect `Random`); the Rust port
//!     will inject an RNG seam at the network layer.
//!   - the remaining byte-level overload/dispatcher helpers
//!     (`buildStatelessReject503Buffer`, `isInviteRequestBuffer`,
//!     `bufferHasToTag`, `jitteredRetryAfter`) — Tier-1 UDP / dispatcher
//!     concerns, ported with their consumers.

use std::collections::BTreeMap;

use crate::parser::custom::structured_headers::{
    parse_contact, parse_name_addr, parse_sip_uri_string, parse_via,
};
use crate::types::{ParamValue, SipHeader, SipRequest};

/// First header value matching `name` (case-insensitive).
/// Canonical top-level comma splitter (quote-, escape- AND angle-bracket-
/// aware) — the ONE implementation for header folding everywhere. Three
/// near-copies used to exist (parser x2, proxy) and two had drifted: one
/// honoured backslash escapes but split inside <...>, the other the reverse,
/// so the same value could split differently at the proxy hop than in the
/// parser that produced it.
pub use crate::parser::custom::structured_headers::split_top_level_commas;

pub fn get_header<'a>(headers: &'a [SipHeader], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

/// All header values matching `name` (case-insensitive), in wire order.
pub fn get_headers<'a>(headers: &'a [SipHeader], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
        .collect()
}

/// Set or replace a header (first occurrence). Returns a new header list.
pub fn set_header(headers: &[SipHeader], name: &str, value: &str) -> Vec<SipHeader> {
    let mut result = headers.to_vec();
    match result.iter_mut().find(|h| h.name.eq_ignore_ascii_case(name)) {
        Some(h) => {
            h.name = name.to_string();
            h.value = value.to_string();
        }
        None => result.push(SipHeader { name: name.to_string(), value: value.to_string() }),
    }
    result
}

/// Remove all headers matching `name` (case-insensitive).
pub fn remove_header(headers: &[SipHeader], name: &str) -> Vec<SipHeader> {
    headers.iter().filter(|h| !h.name.eq_ignore_ascii_case(name)).cloned().collect()
}

/// Extract the `tag` parameter from a From/To header value. Quote-aware.
pub fn extract_tag(header_value: &str) -> Option<String> {
    parse_name_addr(header_value).tag
}

/// Strip the `tag` parameter from a From/To header value, reconstructing the
/// remaining name-addr + params. Quote-aware.
pub fn strip_tag(header_value: &str) -> String {
    let parsed = parse_name_addr(header_value);
    if parsed.tag.is_none() {
        return header_value.to_string();
    }

    let mut result = String::new();
    if let Some(dn) = &parsed.display_name {
        result.push_str(&format!("\"{dn}\" "));
    }
    result.push_str(&format!("<{}>", parsed.uri));
    for (k, v) in &parsed.params {
        if k == "tag" {
            continue;
        }
        match v {
            ParamValue::Flag => result.push_str(&format!(";{k}")),
            ParamValue::Value(val) => result.push_str(&format!(";{k}={val}")),
        }
    }
    result
}

/// Extract the URI from a From/To header value (name-addr). Quote-aware.
pub fn extract_name_addr_uri(header_value: &str) -> String {
    parse_name_addr(header_value).uri
}

/// Extract the URI from a Contact header value. Quote-aware.
pub fn extract_contact_uri(contact_value: &str) -> String {
    parse_contact(contact_value).uri
}

/// Parsed SIP URI fields (port default 5060).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSipUri {
    pub scheme: String,
    pub user: Option<String>,
    pub host: String,
    pub port: u64,
    pub params: BTreeMap<String, String>,
}

/// Parse a SIP URI (angle brackets stripped if present). Zero-regex.
pub fn parse_sip_uri(uri: &str) -> Option<ParsedSipUri> {
    let mut cleaned = uri;
    if let Some(lt) = uri.find('<') {
        cleaned = match uri[lt + 1..].find('>') {
            Some(rel) => &uri[lt + 1..lt + 1 + rel],
            None => &uri[lt + 1..],
        };
    }
    let parsed = parse_sip_uri_string(cleaned)?;
    Some(ParsedSipUri {
        scheme: parsed.scheme,
        user: parsed.user,
        host: parsed.host,
        port: parsed.port.unwrap_or(5060),
        params: parsed.params,
    })
}

/// Extract host:port from a SIP URI string.
pub fn extract_host_port(uri: &str) -> Option<(String, u64)> {
    let parsed = parse_sip_uri(uri)?;
    Some((parsed.host, parsed.port))
}

/// Parse URI parameters from a SIP URI (e.g. `sip:b2bua@host;callRef=abc;leg=a`).
pub fn parse_uri_params(uri: &str) -> BTreeMap<String, String> {
    parse_sip_uri(uri).map(|p| p.params).unwrap_or_default()
}

/// The B2BUA's custom Via parameters: `branch`, `cr`, `lg`. Zero-regex.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViaParams {
    pub branch: Option<String>,
    pub cr: Option<String>,
    pub lg: Option<String>,
}

pub fn parse_via_params(via_value: &str) -> ViaParams {
    let parsed = parse_via(via_value);
    let pick = |key: &str| match parsed.params.get(key) {
        Some(ParamValue::Value(v)) => Some(v.clone()),
        _ => None,
    };
    ViaParams { branch: parsed.branch, cr: pick("cr"), lg: pick("lg") }
}

/// Percent-encode all but RFC 3986 unreserved characters — the encoding the
/// B2BUA stamps on its Via `cr`/`lg` and Contact `callRef`/`leg` params (values
/// contain `|`/`@`/`:`, unsafe in a SIP param). The single home for this codec so
/// the encoder and its inverse [`decode_param`] cannot drift across crates (the
/// B2BUA stamps; sip-txn and the router read).
pub fn encode_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    out
}

/// Inverse of [`encode_param`]; invalid/truncated escapes pass through verbatim.
pub fn decode_param(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        _ => (b'a' + (v - 10)) as char,
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Whether a request carries an emergency Resource-Priority header
/// (esnet.0 / wps.0 / q735.0). Case-insensitive name, case-sensitive value.
pub fn is_emergency_request(req: &SipRequest) -> bool {
    match get_header(&req.headers, "resource-priority") {
        None => false,
        Some(value) => {
            value.contains("esnet.0") || value.contains("wps.0") || value.contains("q735.0")
        }
    }
}

// ---------------------------------------------------------------------------
// Cheap byte-level classifier helpers (overload protection)
// ---------------------------------------------------------------------------
//
// These run on the RAW datagram *before* the SIP parser, in the Tier-1 UDP
// brake's hot path: they decide whether to stateless-503 an INVITE while the
// ingress queue is saturated, so they MUST be allocation-free byte scans, not
// parses. Port of the byte-classifier block in `src/sip/MessageHelpers.ts`.

/// The canonical emergency Resource-Priority namespace.value tokens (per
/// docs/overload-protection.md). Matched case-SENSITIVELY — the upstream
/// contract requires canonical casing, mirroring [`is_emergency_request`].
const EMERGENCY_RPH_TOKENS: [&[u8]; 3] = [b"esnet.0", b"wps.0", b"q735.0"];

/// Cheap byte scan: does the raw datagram carry an emergency signal?
///
/// Two signals, checked in TS order (cheapest first):
///   1. the dispatcher-side markers `;emerg=1` (Request-URI) or `;em=1` (Via
///      custom param) that the B2BUA stamps once an emergency call is admitted
///      (see `b2bua::stack_identity`), which every subsequent in-dialog packet
///      then carries — a plain substring match anywhere in the buffer; then
///   2. an **initial** INVITE's `Resource-Priority:` header (case-sensitive
///      canonical name) whose value, within that same header line, contains one
///      of the canonical emergency tokens [`EMERGENCY_RPH_TOKENS`].
///
/// This is the exact signal the Tier-1 brake consults to NEVER 503 an emergency
/// packet. It operates on bytes (no UTF-8/parse cost) and never allocates.
pub fn buffer_has_emergency_marker(raw: &[u8]) -> bool {
    // Cheap path: dispatcher-side markers stamped on admitted calls.
    if find_subslice(raw, b";emerg=1").is_some() {
        return true;
    }
    if find_subslice(raw, b";em=1").is_some() {
        return true;
    }

    // Initial INVITE: Resource-Priority header (case-sensitive canonical name).
    let rp_idx = match find_subslice(raw, b"Resource-Priority:") {
        Some(idx) => idx,
        None => return false,
    };
    // Confine the token scan to that header's own line: from the match to the
    // next CRLF (or end of buffer if the line is unterminated), so a token in a
    // later header / the body cannot spoof an emergency.
    let line_end = find_subslice(&raw[rp_idx..], b"\r\n").map(|rel| rp_idx + rel);
    let slice = match line_end {
        Some(end) => &raw[rp_idx..end],
        None => &raw[rp_idx..],
    };
    EMERGENCY_RPH_TOKENS.iter().any(|tok| find_subslice(slice, tok).is_some())
}

/// First index of `needle` within `haystack` (byte substring search), or
/// `None`. The Rust equivalent of `Buffer.indexOf` used by the byte
/// classifiers. Empty `needle` matches at 0 (as `indexOf("")` does), matching
/// the TS contract; the callers here never pass an empty needle.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod emergency_tests {
    //! Port of the documented `isEmergencyRequest` contract from
    //! `src/sip/MessageHelpers.ts` (L379–387). The TS function has no dedicated
    //! unit test in the source suite — it is exercised end-to-end by the proxy
    //! admission (`selectForNewDialog-overload.test.ts`) and the byte-level UDP
    //! brake (`UdpTransport-brake.test.ts`, which tests the *separate*
    //! `bufferHasEmergencyMarker` helper, deferred per this module's doc). These
    //! tests pin the contract the TS implementation encodes: case-INsensitive
    //! header lookup, case-SENSITIVE value match (canonical casing required by
    //! the upstream contract, per docs/overload-protection.md), substring match
    //! against the three canonical RPH tokens, `false` when absent.

    use super::is_emergency_request;
    use crate::parser::SipParser;
    use crate::parser::custom::CustomParser;
    use crate::types::SipMessage;

    /// Parse a minimal INVITE carrying `rph` as the `Resource-Priority` header
    /// value. `rph == None` omits the header entirely.
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
        // Lower-cased header name still matched (getHeader is case-insensitive).
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
        // A well-formed but non-emergency RPH namespace.value.
        let req = invite_with_rph(Some("Resource-Priority"), Some("dsn.flash"));
        assert!(!is_emergency_request(&req));
    }

    #[test]
    fn token_matches_as_substring_among_multiple() {
        // The TS uses indexOf !== -1, so a canonical token embedded in a
        // multi-namespace RPH value still flags emergency.
        let req = invite_with_rph(Some("Resource-Priority"), Some("dsn.flash, q735.0"));
        assert!(is_emergency_request(&req));
    }
}

#[cfg(test)]
mod buffer_emergency_tests {
    //! Port of the `bufferHasEmergencyMarker` contract from
    //! `src/sip/MessageHelpers.ts` (L421–438). Like its parsed sibling
    //! `isEmergencyRequest`, the TS function has **no dedicated unit test** in
    //! the source suite — it is exercised only end-to-end by the byte-level UDP
    //! brake (`tests/sip/UdpTransport-brake.test.ts`), which cannot be ported
    //! until the `UdpTransport` facade lands in slice 2 (network/dispatch).
    //!
    //! These tests therefore pin the byte-scan contract the TS implementation
    //! encodes directly, including the three cases the brake integration relies
    //! on (an `esnet.0` RPH INVITE bypasses the brake; a plain INVITE does not):
    //!   - the stamped in-dialog markers `;emerg=1` (Request-URI) / `;em=1`
    //!     (Via) match **anywhere** in the datagram — the cheap path checked
    //!     first;
    //!   - the **initial** INVITE path matches a case-SENSITIVE
    //!     `Resource-Priority:` header name whose **same-line** value carries a
    //!     canonical token (`esnet.0` / `wps.0` / `q735.0`);
    //!   - a token on a *different* line than the `Resource-Priority:` header
    //!     does NOT count (line-confined scan);
    //!   - `false` when no signal is present.
    //!
    //! The marker strings asserted here are exactly what `b2bua::stack_identity`
    //! stamps: `em=1` as a Via custom param and `emerg=1` as a Request-URI
    //! param, which serialise to `;em=1` / `;emerg=1` on the wire.

    use super::buffer_has_emergency_marker;

    /// Build a minimal INVITE datagram. `rph` is the optional
    /// `Resource-Priority` header *value*; `name` lets a test vary the header
    /// casing to pin case-sensitivity.
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
        // Mirrors `buildInviteBuffer(_, { emergency: true })` (esnet.0) plus the
        // other two canonical tokens.
        for tok in ["esnet.0", "wps.0", "q735.0"] {
            let buf = invite_buf(Some(("Resource-Priority", tok)));
            assert!(buffer_has_emergency_marker(&buf), "{tok} should flag emergency");
        }
    }

    #[test]
    fn plain_invite_is_not_emergency() {
        // The non-emergency `buildInviteBuffer` shape — must NOT bypass the brake.
        assert!(!buffer_has_emergency_marker(&invite_buf(None)));
    }

    #[test]
    fn rph_header_name_is_case_sensitive() {
        // TS scans for the literal "Resource-Priority:" — a lower-cased header
        // name is not matched (canonical-casing contract).
        let buf = invite_buf(Some(("resource-priority", "esnet.0")));
        assert!(!buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn rph_token_match_is_case_sensitive() {
        // Canonical token casing required: an upper-cased token does not flag.
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
        // indexOf semantics: a canonical token embedded in a multi-namespace
        // value still flags.
        let buf = invite_buf(Some(("Resource-Priority", "dsn.flash, q735.0")));
        assert!(buffer_has_emergency_marker(&buf));
    }

    #[test]
    fn token_on_a_different_line_than_rph_does_not_flag() {
        // A canonical token elsewhere in the message (here a Subject header)
        // must NOT count — the scan is confined to the Resource-Priority line.
        // The RP header itself carries a non-emergency value.
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-x\r\n\
Subject: priority esnet.0 please\r\n\
Resource-Priority: dsn.flash\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_emergency_marker(raw.as_bytes()));
    }

    #[test]
    fn unterminated_rph_line_still_scans_to_end() {
        // No trailing CRLF after the value (lineEnd === -1 branch in TS):
        // subarray(rpIdx) scans to the end and still finds the token.
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
    fn em_marker_is_checked_before_rph_so_non_invite_in_dialog_passes() {
        // An in-dialog OPTIONS with neither RPH nor markers is not emergency
        // (sanity: the cheap path returns false, then no RPH header → false).
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
        // Guards the `needle.len() > haystack.len()` early-out in find_subslice.
        assert!(!buffer_has_emergency_marker(b"INV"));
    }
}

#[cfg(test)]
mod find_subslice_tests {
    use super::find_subslice;

    #[test]
    fn finds_at_start_middle_and_absent() {
        assert_eq!(find_subslice(b"abcdef", b"abc"), Some(0));
        assert_eq!(find_subslice(b"abcdef", b"cd"), Some(2));
        assert_eq!(find_subslice(b"abcdef", b"xyz"), None);
    }

    #[test]
    fn empty_needle_matches_at_zero_like_indexof() {
        assert_eq!(find_subslice(b"abc", b""), Some(0));
        assert_eq!(find_subslice(b"", b""), Some(0));
    }

    #[test]
    fn needle_longer_than_haystack_is_none() {
        assert_eq!(find_subslice(b"ab", b"abc"), None);
    }
}

#[cfg(test)]
mod codec_tests {
    use super::{decode_param, encode_param};

    #[test]
    fn param_round_trips_through_unsafe_chars() {
        let raw = "w0|alice@example.com:5060|ab12cd34";
        let enc = encode_param(raw);
        assert!(!enc.contains('|') && !enc.contains('@') && !enc.contains(':'));
        assert_eq!(decode_param(&enc), raw, "encode∘decode is identity");
    }

    #[test]
    fn decode_passes_truncated_escapes_verbatim() {
        assert_eq!(decode_param("ab%"), "ab%");
        assert_eq!(decode_param("ab%4"), "ab%4");
        assert_eq!(decode_param("%zz"), "%zz");
    }
}
