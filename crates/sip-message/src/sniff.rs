//! Lenient raw-datagram scanners: extract one field from an unparsed SIP
//! datagram without a full parse. For hot paths (demux, dedup, metrics
//! labelling) that must stay cheap and tolerate malformed input by returning
//! `None`/empty rather than erroring.
//!
//! This module is the ONLY sanctioned home for raw SIP header extraction —
//! never re-implement these scanners in another crate. If a scanner you need
//! is missing, add it here. For anything richer than single-field extraction,
//! use the real parser ([`crate::parser`]).

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
    fn via_branch_takes_topmost_via_only() {
        let raw = b"INVITE sip:x SIP/2.0\r\nVia: SIP/2.0/UDP a;branch=z9-top\r\nVia: SIP/2.0/UDP b;branch=z9-bot\r\n\r\n";
        assert_eq!(via_branch(raw).as_deref(), Some("z9-top"));
        assert_eq!(via_branch(b"ACK sip:x SIP/2.0\r\n\r\n"), None);
    }
}
