//! Lenient raw-datagram scanners: extract one field from an unparsed SIP
//! datagram without a full parse. For hot paths (demux, dedup, metrics
//! labelling) that must stay cheap and tolerate malformed input by returning
//! `None`/empty rather than erroring.
//!
//! This module is the ONLY sanctioned home for raw SIP header extraction —
//! never re-implement these scanners in another crate. If a scanner you need
//! is missing, add it here. For anything richer than single-field extraction,
//! use the real parser ([`crate::parser`]). Strict, allocation-free
//! pre-parse *classifiers* (overload brake, dispatcher fast-path) live in
//! [`crate::preparse`] and its sibling modules.

use std::borrow::Cow;

fn as_str(raw: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(raw)
}

/// The (trimmed) request/status line, empty if the datagram has none.
pub fn first_line(raw: &[u8]) -> String {
    as_str(raw).lines().next().unwrap_or("").trim().to_string()
}

/// Value of header `name` (case-insensitive), scanning the header block only.
pub fn header_value(raw: &[u8], name: &str) -> Option<String> {
    let s = as_str(raw);
    for line in s.lines() {
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((h, v)) = line.split_once(':') {
            if h.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

/// The Call-ID (full or compact `i` form).
pub fn call_id(raw: &[u8]) -> Option<String> {
    header_value(raw, "call-id").or_else(|| header_value(raw, "i"))
}

/// The `tag` parameter of the To header (full or compact `t` form), or the
/// empty string when absent (e.g. a tagless 100 Trying).
pub fn to_tag(raw: &[u8]) -> String {
    let Some(v) = header_value(raw, "to").or_else(|| header_value(raw, "t")) else {
        return String::new();
    };
    let lower = v.to_ascii_lowercase();
    let Some(pos) = lower.find("tag=") else { return String::new() };
    let rest = &v[pos + "tag=".len()..];
    let end = rest.find([';', ',', ' ', '\t', '>']).unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

/// Whether `raw` is a SIP response (status line) vs a request.
pub fn is_response(raw: &[u8]) -> bool {
    first_line(raw).starts_with("SIP/2.0")
}

/// The response status code (`200` from `SIP/2.0 200 OK`), or `None` for a request.
pub fn resp_status(raw: &[u8]) -> Option<u16> {
    let line = first_line(raw);
    if !line.starts_with("SIP/2.0") {
        return None;
    }
    line.split_whitespace().nth(1).and_then(|s| s.parse().ok())
}

/// The request method (`INVITE` from the request line), or `None` for a response.
pub fn req_method(raw: &[u8]) -> Option<String> {
    let line = first_line(raw);
    if line.starts_with("SIP/2.0") {
        return None;
    }
    line.split_whitespace().next().map(str::to_string)
}

/// The Request-URI (the second token of the request line), or `None` for a
/// response. The token is returned exactly as it appears — a demux tier that
/// keys on the URI reads it with the URI value type.
pub fn request_uri(raw: &[u8]) -> Option<String> {
    let line = first_line(raw);
    if line.starts_with("SIP/2.0") {
        return None;
    }
    line.split_whitespace().nth(1).map(str::to_string)
}

/// The CSeq sequence number (the `<num>` of `CSeq: <num> <METHOD>`), or `None`
/// if absent/unparseable. Works for requests and responses.
pub fn cseq_number(raw: &[u8]) -> Option<u32> {
    cseq_line(raw)?.split_whitespace().next()?.parse().ok()
}

/// The CSeq line rendered for a log/sample (`CSeq: <num> <METHOD>`), empty if
/// absent.
pub fn cseq_value(raw: &[u8]) -> String {
    match cseq_line(raw) {
        Some(v) => format!("CSeq: {v}"),
        None => String::new(),
    }
}

/// The CSeq method mapped to a BOUNDED static label — safe as a low-cardinality
/// metrics label AND usable for method comparison (every RFC 3261/3262/3515
/// method maps to itself). `"none"` when absent, `"other"` for an unknown
/// method.
pub fn cseq_method_label(raw: &[u8]) -> &'static str {
    let Some(l) = cseq_line(raw) else { return "none" };
    let m = l.split_whitespace().nth(1).unwrap_or("");
    match m.to_ascii_uppercase().as_str() {
        "INVITE" => "INVITE",
        "ACK" => "ACK",
        "BYE" => "BYE",
        "CANCEL" => "CANCEL",
        "OPTIONS" => "OPTIONS",
        "REFER" => "REFER",
        "NOTIFY" => "NOTIFY",
        "PRACK" => "PRACK",
        "UPDATE" => "UPDATE",
        "INFO" => "INFO",
        "SUBSCRIBE" => "SUBSCRIBE",
        "MESSAGE" => "MESSAGE",
        "" => "none",
        _ => "other",
    }
}

/// The trimmed CSeq header value (`<num> <METHOD>`), or `None`.
fn cseq_line(raw: &[u8]) -> Option<String> {
    let s = as_str(raw);
    for line in s.lines() {
        let l = line.trim();
        if l.len() >= 5 && l[..5].eq_ignore_ascii_case("cseq:") {
            return Some(l[5..].trim().to_string());
        }
    }
    None
}

/// Whether a `Require` header lists the `100rel` option-tag (comma-folded,
/// case-insensitive) — the reliable-provisional marker (RFC 3262 §3). `Require`
/// has no compact form.
pub fn require_has_100rel(raw: &[u8]) -> bool {
    header_value(raw, "require")
        .is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("100rel")))
}

/// The `RSeq` value of a reliable provisional (RFC 3262 §3), or `None`.
pub fn rseq_of(raw: &[u8]) -> Option<u64> {
    header_value(raw, "rseq")?.trim().parse().ok()
}

/// The RAck response-num (its FIRST token = the acknowledged 1xx's RSeq,
/// RFC 3262 §7.2) of a PRACK, or `None`.
pub fn rack_rseq(raw: &[u8]) -> Option<u64> {
    header_value(raw, "rack")?.split_whitespace().next()?.parse().ok()
}

/// Selective intake-shed classifier: whether a raw datagram is a NEW-DIALOG,
/// NON-EMERGENCY INVITE — the only class a depth-watermarked pre-ingress hook
/// may drop under overload. Called per-datagram at the socket pump, zero
/// allocation; `false` = admit.
///
/// `true` requires ALL of: the canonical `INVITE ` request line
/// ([`crate::preparse::is_invite_request_buffer`]), a To header (full or
/// compact `t` form, like [`to_tag`]) WITHOUT a `tag=` parameter (new
/// dialog), and no emergency Resource-Priority — any `Resource-Priority`
/// header (case-insensitive name, comma-separated r-values) carrying an
/// emergency r-value classifies exactly as [`crate::emergency`]'s
/// parsed-side check. Everything else — in-dialog requests, responses,
/// other methods, emergency INVITEs, truncated/garbage datagrams — is
/// admitted (garbage is cheap to admit; the parse layer discards it).
pub fn is_sheddable_new_invite(raw: &[u8]) -> bool {
    if !crate::preparse::is_invite_request_buffer(raw) {
        return false;
    }
    let mut lines = raw.split(|&b| b == b'\n');
    lines.next(); // request line
    let mut tagless_to_seen = false;
    for line in lines {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break; // end of headers
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        let name = line[..colon].trim_ascii();
        let value = &line[colon + 1..];
        if name.eq_ignore_ascii_case(b"to") || name.eq_ignore_ascii_case(b"t") {
            if contains_ignore_ascii_case(value, b"tag=") {
                return false; // in-dialog
            }
            tagless_to_seen = true;
        } else if name.eq_ignore_ascii_case(b"resource-priority")
            && value.split(|&b| b == b',').any(|rv| {
                crate::emergency::EMERGENCY_RPH_TOKENS
                    .iter()
                    .any(|tok| rv.trim_ascii().eq_ignore_ascii_case(tok.as_bytes()))
            })
        {
            return false; // emergency
        }
    }
    tagless_to_seen
}

/// ASCII-case-insensitive substring search, allocation-free. `needle` must be
/// non-empty.
fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

/// The `branch` parameter of the TOP-most Via header (RFC 3261 §17 transaction
/// key), or `None` if absent. Only the first Via matters — on a request we sent
/// it is OUR Via, echoed by the UAS onto the matching response.
pub fn via_branch(raw: &[u8]) -> Option<String> {
    for line in as_str(raw).lines() {
        if line.is_empty() {
            break; // end of headers
        }
        let Some((h, v)) = line.split_once(':') else { continue };
        let h = h.trim();
        if h.eq_ignore_ascii_case("via") || h.eq_ignore_ascii_case("v") {
            let pos = v.find("branch=")?;
            let rest = &v[pos + "branch=".len()..];
            let end = rest.find([';', ',', ' ', '\t']).unwrap_or(rest.len());
            let b = rest[..end].trim();
            return (!b.is_empty()).then(|| b.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_tag_extracts_the_to_parameter() {
        assert_eq!(
            to_tag(b"SIP/2.0 180 X\r\nTo: <sip:b@h>;tag=abc\r\n\r\n"),
            "abc"
        );
        assert_eq!(
            to_tag(b"SIP/2.0 100 Trying\r\nTo: <sip:b@h>\r\n\r\n"),
            "",
            "a tagless To yields empty"
        );
        assert_eq!(
            to_tag(b"SIP/2.0 200 OK\r\nt: <sip:b@h>;tag=Z9\r\n\r\n"),
            "Z9",
            "the compact To form is parsed"
        );
    }

    #[test]
    fn cseq_scanners_share_one_line_walk() {
        let raw = b"BYE sip:x SIP/2.0\r\nCSeq:  7   BYE\r\n\r\n";
        assert_eq!(cseq_number(raw), Some(7));
        assert_eq!(cseq_value(raw), "CSeq: 7   BYE");
        assert_eq!(cseq_method_label(raw), "BYE");
        assert_eq!(cseq_method_label(b"OPTIONS sip:x SIP/2.0\r\n\r\n"), "none");
        assert_eq!(
            cseq_method_label(b"X sip:x SIP/2.0\r\nCSeq: 1 WEIRD\r\n\r\n"),
            "other"
        );
    }

    #[test]
    fn request_uri_reads_the_second_request_line_token() {
        assert_eq!(
            request_uri(b"INVITE sip:bob@10.0.0.1:5070 SIP/2.0\r\n\r\n").as_deref(),
            Some("sip:bob@10.0.0.1:5070")
        );
        assert_eq!(
            request_uri(b"SIP/2.0 200 OK\r\n\r\n"),
            None,
            "a response has no Request-URI"
        );
    }

    /// Assemble a request with the given request-line method token and extra
    /// header lines (fixture only — the classifier under test never allocates).
    fn req(method: &str, headers: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@10.0.0.2:5070 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-shed\r\n\
From: <sip:alice@example.com>;tag=a1\r\n\
{headers}Call-ID: shed@10.0.0.1\r\n\
CSeq: 1 {method}\r\n\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn new_invite_is_sheddable() {
        assert!(is_sheddable_new_invite(&req("INVITE", "To: <sip:bob@example.com>\r\n")));
    }

    #[test]
    fn to_tag_marks_in_dialog_and_admits() {
        assert!(!is_sheddable_new_invite(&req(
            "INVITE",
            "To: <sip:bob@example.com>;tag=b2\r\n"
        )));
    }

    #[test]
    fn to_header_name_casing_and_compact_form_are_recognised() {
        for tagless in ["TO: <sip:bob@h>\r\n", "to: <sip:bob@h>\r\n", "t: <sip:bob@h>\r\n"] {
            assert!(is_sheddable_new_invite(&req("INVITE", tagless)), "{tagless:?}");
        }
        for tagged in ["TO: <sip:bob@h>;TAG=B2\r\n", "t: <sip:bob@h>;tag=b2\r\n"] {
            assert!(!is_sheddable_new_invite(&req("INVITE", tagged)), "{tagged:?}");
        }
    }

    #[test]
    fn non_invite_methods_are_admitted() {
        for m in ["ACK", "BYE", "CANCEL", "OPTIONS", "REGISTER"] {
            assert!(
                !is_sheddable_new_invite(&req(m, "To: <sip:bob@example.com>\r\n")),
                "{m} must be admitted"
            );
        }
    }

    #[test]
    fn responses_are_admitted() {
        assert!(!is_sheddable_new_invite(
            b"SIP/2.0 200 OK\r\nTo: <sip:bob@h>\r\nCSeq: 1 INVITE\r\n\r\n"
        ));
        assert!(!is_sheddable_new_invite(b"SIP/2.0 180 Ringing\r\nTo: <sip:bob@h>\r\n\r\n"));
    }

    #[test]
    fn emergency_invites_are_admitted() {
        // Each canonical r-value, mixed case, and a comma-separated list.
        for rph in ["esnet.0", "wps.0", "q735.0", "ESNET.0", "Wps.0", "dsn.flash, q735.0"] {
            let raw = req(
                "INVITE",
                &format!("To: <sip:bob@h>\r\nResource-Priority: {rph}\r\n"),
            );
            assert!(!is_sheddable_new_invite(&raw), "{rph:?} must be admitted");
        }
        // Case-insensitive header name.
        assert!(!is_sheddable_new_invite(&req(
            "INVITE",
            "To: <sip:bob@h>\r\nRESOURCE-PRIORITY: esnet.0\r\n"
        )));
        // Any of multiple Resource-Priority headers flags.
        assert!(!is_sheddable_new_invite(&req(
            "INVITE",
            "To: <sip:bob@h>\r\nResource-Priority: dsn.flash\r\nResource-Priority: wps.0\r\n"
        )));
    }

    #[test]
    fn non_emergency_resource_priority_stays_sheddable() {
        // r-values compare whole (comma-split, trimmed) — mirrors
        // `crate::emergency`: `dsn.flash` and the embedded `esnet.01` are not
        // emergency, so the new INVITE remains sheddable.
        for rph in ["dsn.flash", "esnet.01"] {
            let raw = req(
                "INVITE",
                &format!("To: <sip:bob@h>\r\nResource-Priority: {rph}\r\n"),
            );
            assert!(is_sheddable_new_invite(&raw), "{rph:?} is not emergency");
        }
    }

    #[test]
    fn truncated_and_garbage_datagrams_are_admitted() {
        for raw in [
            &b""[..],
            &b"INVITE"[..],
            &b"INVITE sip:bob@h SIP/2.0\r\nVia: SIP/2.0/UDP x\r\n\r\n"[..], // no To at all
            &b"invite sip:bob@h SIP/2.0\r\nTo: <sip:bob@h>\r\n\r\n"[..],    // method case-sensitive
            &b"\x00\x01\x02 garbage \xff\xfe"[..],
        ] {
            assert!(!is_sheddable_new_invite(raw), "{raw:?} must be admitted");
        }
    }

    #[test]
    fn via_branch_takes_topmost_via_only() {
        let raw = b"INVITE sip:x SIP/2.0\r\nVia: SIP/2.0/UDP a;branch=z9-top\r\nVia: SIP/2.0/UDP b;branch=z9-bot\r\n\r\n";
        assert_eq!(via_branch(raw).as_deref(), Some("z9-top"));
        assert_eq!(via_branch(b"ACK sip:x SIP/2.0\r\n\r\n"), None);
    }
}
