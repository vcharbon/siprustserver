//! Mandatory-header extraction + the ADR-0007 strict-grammar gates
//! (`Wire` vs `Hydrate` mode). Port of `src/sip/parsers/extract-fields.ts`.
//!
//! Produces the typed [`CoreHeaders`] from the
//! [`HeaderIndex`] of one dispatch pass — this module locates nothing itself,
//! it validates and parses what the index holds. `Wire` runs every gate (the
//! security boundary on wire bytes);
//! `Hydrate` runs only the baseline presence/range/tag checks for already-
//! trusted internal construction.

use super::header_index::HeaderIndex;
use super::scanner::{is_token_char, strict_non_negative_decimal};
use super::structured_headers::{
    parse_contact, parse_cseq, parse_name_addr, parse_sip_uri_string, parse_via,
    top_level_comma_entries, validate_strict_host, validate_strict_sip_uri,
};
use crate::error::SipParseError;
use crate::header::{self, HostPort, NameAddr, Uri};
use crate::method::Method;
use crate::parser::SipParserLimits;
use crate::sip_str::SipStr;
use crate::types::{ContactSet, CoreHeaders, NonEmpty};

/// RFC 3261 §8.1.1.7 — top-Via branch MUST start with this magic cookie.
const VIA_BRANCH_MAGIC_COOKIE: &str = "z9hG4bK";

const INT_32_MAX: u64 = (1u64 << 31) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    Wire,
    Hydrate,
}

/// Request eager fields = the shared core + the parsed Request-URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestEager {
    pub common: CoreHeaders,
    pub request_uri: Uri,
}

/// RFC 3261 / RFC 3986 §3.2.3: SIP ports are 1..=65535.
fn is_valid_port(p: u64) -> bool {
    (1..=65535).contains(&p)
}

// ---------------------------------------------------------------------------
// Internal → public field mapping
// ---------------------------------------------------------------------------

/// A scanned name-addr as the typed address value. A URI the strict reader
/// rejects is kept whole (the parser's own gates decide admissibility), so no
/// reader ever loses what the peer sent.
fn to_name_addr(p: super::structured_headers::ParsedNameAddr) -> NameAddr {
    NameAddr::from_parts(p.display_name, Uri::parse_or_opaque(&p.uri), p.params)
}

fn to_contact(p: super::structured_headers::ParsedContact) -> header::Contact {
    header::Contact::new(NameAddr::from_parts(
        p.display_name,
        Uri::parse_or_opaque(&p.uri),
        p.params,
    ))
}

// ---------------------------------------------------------------------------
// Byte-scan strict gates. All structural bytes tested below are ASCII, so a
// UTF-8 lead/continuation byte can never alias one — byte scans visit exactly
// the positions the old `Vec<char>` walks did, without the per-call collect.
// ---------------------------------------------------------------------------

/// True iff the URI carries unescaped control bytes (0x00-0x1F except HTAB,
/// or 0x7F).
fn has_unescaped_ctl_bytes(uri: &str) -> bool {
    uri.bytes().any(|b| b != 0x09 && (b < 0x20 || b == 0x7f))
}

/// True iff `uri` contains `[` without a matching `]`.
fn has_unbalanced_square_brackets(uri: &str) -> bool {
    let mut open: i32 = 0;
    for c in uri.bytes() {
        if c == b'[' {
            open += 1;
        } else if c == b']' {
            if open == 0 {
                return true;
            }
            open -= 1;
        }
    }
    open != 0
}

/// True iff the host portion has 2+ colons outside `[...]` — IPv6 without the
/// required bracket delimiters (RFC 5118 §4.2).
fn has_unbracketed_ipv6(uri: &str) -> bool {
    let s = uri.as_bytes();
    let scheme_colon = match s.iter().position(|&c| c == b':') {
        Some(i) => i,
        None => return false,
    };
    let mut i = scheme_colon + 1;
    if let Some(at) = s[i..].iter().position(|&c| c == b'@') {
        i += at + 1;
    }
    let mut depth = 0i32;
    let mut colons = 0i32;
    while i < s.len() {
        let c = s[i];
        if c == b'[' {
            depth += 1;
        } else if c == b']' && depth > 0 {
            depth -= 1;
        } else if depth == 0 {
            if c == b':' {
                colons += 1;
                if colons >= 2 {
                    return true;
                }
            } else if c == b';' || c == b'?' || c == b'>' {
                break;
            }
        }
        i += 1;
    }
    false
}

/// True iff `uri` has port digits followed by an alphabetic byte at the
/// host:port position (SIP-ALG confusion), context-aware about userinfo.
fn has_uri_port_trailing_garbage(uri: &str) -> bool {
    let s = uri.as_bytes();
    let first = match s.iter().position(|&c| c == b'@') {
        Some(a) => a,
        None => match s.iter().position(|&c| c == b':') {
            Some(c) => c,
            None => return false,
        },
    };
    let mut i = first + 1; // past `@` or scheme colon
    if i < s.len() && s[i] == b'[' {
        match s[i + 1..].iter().position(|&c| c == b']') {
            None => return false,
            Some(close) => i = i + 1 + close + 1,
        }
    } else {
        while i < s.len() && s[i] != b':' && s[i] != b';' && s[i] != b'?' && s[i] != b'>' {
            i += 1;
        }
    }
    if i >= s.len() || s[i] != b':' {
        return false;
    }
    i += 1;
    let port_start = i;
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    if i == port_start || i >= s.len() {
        return false;
    }
    s[i].is_ascii_alphabetic()
}

/// True iff `via` carries a port followed by alphabetic trailing garbage,
/// quote-aware.
fn has_via_port_trailing_garbage(via: &str) -> bool {
    let s = via.as_bytes();
    if s.is_empty() {
        return false;
    }
    let mut in_quote = false;
    let mut i = 0usize;
    while i + 1 < s.len() {
        let c = s[i];
        if in_quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_quote = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_quote = true;
            i += 1;
            continue;
        }
        if c != b':' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let port_start = j;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
        }
        if j == port_start || j >= s.len() {
            i += 1;
            continue;
        }
        if s[j].is_ascii_alphabetic() {
            return true;
        }
        i += 1;
    }
    false
}

/// Count `;tag=` occurrences outside quoted-strings (RFC 3261 permits one).
fn count_tag_params(value: &str) -> usize {
    let s = value.as_bytes();
    let mut count = 0usize;
    let mut in_quote = false;
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        if in_quote {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_quote = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_quote = true;
            i += 1;
            continue;
        }
        if c != b';' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < s.len() && (s[j] == b' ' || s[j] == b'\t') {
            j += 1;
        }
        if j + 3 > s.len() {
            i += 1;
            continue;
        }
        let t = s[j];
        let a = s[j + 1];
        let g = s[j + 2];
        if (t != b't' && t != b'T') || (a != b'a' && a != b'A') || (g != b'g' && g != b'G') {
            i += 1;
            continue;
        }
        let mut k = j + 3;
        while k < s.len() && (s[k] == b' ' || s[k] == b'\t') {
            k += 1;
        }
        if k >= s.len() || s[k] != b'=' {
            i += 1;
            continue;
        }
        count += 1;
        i += 1;
    }
    count
}

/// Confirm the Via sent-protocol grammar: three non-empty `1*tchar` tokens
/// separated by `/` with LWS permitted around the `/`. `None` if valid.
fn check_sent_protocol(via: &str) -> Option<String> {
    let s = via.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < s.len() && (s[i] == b' ' || s[i] == b'\t') {
            i += 1;
        }
        i
    };
    let read_token_run = |mut i: usize| {
        while i < s.len() && is_token_char(s[i]) {
            i += 1;
        }
        i
    };
    let mut i = skip_ws(0);
    let name_start = i;
    i = read_token_run(i);
    if i == name_start {
        return Some("empty Via protocol-name".to_string());
    }
    i = skip_ws(i);
    if i >= s.len() || s[i] != b'/' {
        return Some("missing `/` after Via protocol-name".to_string());
    }
    i = skip_ws(i + 1);
    let ver_start = i;
    i = read_token_run(i);
    if i == ver_start {
        return Some("empty Via protocol-version".to_string());
    }
    i = skip_ws(i);
    if i >= s.len() || s[i] != b'/' {
        return Some("missing `/` after Via protocol-version".to_string());
    }
    i = skip_ws(i + 1);
    let trans_start = i;
    i = read_token_run(i);
    if i == trans_start {
        return Some("empty Via transport".to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// extractCommonFields
// ---------------------------------------------------------------------------

pub fn extract_common_fields(
    idx: &HeaderIndex,
    limits: &SipParserLimits,
    mode: ExtractMode,
) -> Result<CoreHeaders, SipParseError> {
    let wire = mode == ExtractMode::Wire;

    // From/To/Call-ID/CSeq appear exactly once; only Via may repeat.
    if idx.from.count > 1 {
        return Err(SipParseError::new("Multiple From headers (RFC 3261 §8.1.1 — exactly one required)"));
    }
    if idx.to.count > 1 {
        return Err(SipParseError::new("Multiple To headers (RFC 3261 §8.1.1 — exactly one required)"));
    }
    if idx.call_id.count > 1 {
        return Err(SipParseError::new("Multiple Call-ID headers (RFC 3261 §8.1.1 — exactly one required)"));
    }
    if idx.cseq.count > 1 {
        return Err(SipParseError::new("Multiple CSeq headers (RFC 3261 §8.1.1 — exactly one required)"));
    }

    let from_val =
        idx.from.first.ok_or_else(|| SipParseError::new("Missing mandatory From header"))?;
    let from_parsed = parse_name_addr(from_val);
    if from_parsed.tag.as_deref() == Some("") {
        return Err(SipParseError::new("Empty From tag parameter"));
    }
    if count_tag_params(from_val) > 1 {
        return Err(SipParseError::new("Duplicate From tag parameter"));
    }
    if wire {
        if let Some(reason) = validate_strict_sip_uri(&from_parsed.uri) {
            return Err(SipParseError::new(format!("Strict From URI: {reason} (\"{}\")", from_parsed.uri)));
        }
    }

    let to_val = idx.to.first.ok_or_else(|| SipParseError::new("Missing mandatory To header"))?;
    let to_parsed = parse_name_addr(to_val);
    if to_parsed.tag.as_deref() == Some("") {
        return Err(SipParseError::new("Empty To tag parameter"));
    }
    if count_tag_params(to_val) > 1 {
        return Err(SipParseError::new("Duplicate To tag parameter"));
    }
    if wire {
        if let Some(reason) = validate_strict_sip_uri(&to_parsed.uri) {
            return Err(SipParseError::new(format!("Strict To URI: {reason} (\"{}\")", to_parsed.uri)));
        }
    }

    let call_id = match idx.call_id.first {
        Some(v) if !v.is_empty() => v.clone(),
        _ => return Err(SipParseError::new("Missing mandatory Call-ID header")),
    };

    let cseq_val =
        idx.cseq.first.ok_or_else(|| SipParseError::new("Missing mandatory CSeq header"))?;
    let cseq_parsed = parse_cseq(cseq_val);
    if wire {
        let cseq_raw = cseq_val.trim();
        // First SP/HTAB is ASCII → a valid slice boundary.
        let space_idx = cseq_raw.bytes().position(|c| c == b' ' || c == b'\t');
        match space_idx {
            None => {
                return Err(SipParseError::new(format!("CSeq missing method token: \"{cseq_val}\"")));
            }
            Some(idx) => {
                let cseq_digits = &cseq_raw[..idx];
                if strict_non_negative_decimal(cseq_digits, INT_32_MAX).is_none() {
                    return Err(SipParseError::new(format!(
                        "CSeq seq malformed (paranoid digit check): \"{cseq_digits}\""
                    )));
                }
                if cseq_raw[idx + 1..].trim().is_empty() {
                    return Err(SipParseError::new(format!("CSeq missing method token: \"{cseq_val}\"")));
                }
            }
        }
    }

    // Via — at least one required; both comma-list and repeated-line encodings
    // fold into one ordered list. Each segment stays a span of its header
    // value, so folding a comma-list into the ordered Via list copies nothing.
    if idx.via.is_empty() {
        return Err(SipParseError::new("Missing mandatory Via header"));
    }
    let via_segments = || idx.via.iter().flat_map(|v| top_level_comma_entries(v.as_str()));
    for segment in via_segments() {
        if has_via_port_trailing_garbage(segment) {
            return Err(SipParseError::new(format!("Trailing non-digit after Via port: \"{segment}\"")));
        }
        if wire {
            if let Some(reason) = check_sent_protocol(segment) {
                return Err(SipParseError::new(format!("{reason}: \"{segment}\"")));
            }
        }
    }
    let mut vias: Vec<header::Via> = Vec::with_capacity(idx.via.len());
    for (value, raw) in idx.via.iter().flat_map(|v| top_level_comma_entries(v.as_str()).map(move |s| (*v, s))) {
        let v = parse_via(&value.reslice(raw));
        if let Some(p) = v.port {
            if !is_valid_port(p) {
                return Err(SipParseError::new(format!("Via port out of range: {p}")));
            }
        }
        if v.branch.as_deref() == Some("") {
            return Err(SipParseError::new("Empty Via branch parameter"));
        }
        if wire {
            // Top-Via magic cookie (RFC 3261 §8.1.1.7) — topmost Via only.
            if vias.is_empty() {
                match &v.branch {
                    Some(b) if b.starts_with(VIA_BRANCH_MAGIC_COOKIE) => {}
                    other => {
                        let shown = match other {
                            None => "<no branch>".to_string(),
                            Some(b) => format!("\"{b}\""),
                        };
                        return Err(SipParseError::new(format!(
                            "Top Via branch missing magic cookie \"{VIA_BRANCH_MAGIC_COOKIE}\": {shown}"
                        )));
                    }
                }
            }
            if let Some(reason) = validate_strict_host(&v.host) {
                return Err(SipParseError::new(format!("Strict Via sent-by host: {reason} (\"{}\")", v.host)));
            }
            if let Some(colon_count) = sent_by_extra_colons(raw) {
                return Err(SipParseError::new(format!(
                    "Via sent-by has {colon_count} colons (must be ≤ 1): \"{raw}\""
                )));
            }
            // The allowlist is documented case-insensitive; probe without minting
            // an uppercased String per Via (set is ≤6 entries — linear is fine).
            if !limits.allowed_transports.iter().any(|t| t.eq_ignore_ascii_case(&v.transport)) {
                return Err(SipParseError::new(format!("Via transport \"{}\" not in allowed set", v.transport)));
            }
        }
        vias.push(header::Via::from_parts(
            v.protocol,
            v.version,
            v.transport,
            HostPort::new(v.host, v.port.map(|p| p as u16)),
            v.params,
        ));
    }

    // Contact — fold comma-list and repeated lines. `Contact: *` must stand
    // alone, which is settled over every entry before any is parsed.
    let contact_segments = || {
        idx.contact
            .iter()
            .flat_map(|v| top_level_comma_entries(v.as_str()).map(move |s| (*v, s)))
            .filter(|(_, seg)| !seg.is_empty())
    };
    let contact_wildcard = contact_segments().any(|(_, seg)| seg == "*");
    if contact_wildcard && contact_segments().any(|(_, seg)| seg != "*") {
        return Err(SipParseError::new("Contact: * wildcard must be the only value (RFC 3261 §10.2.2)"));
    }
    let mut contact_list: Vec<header::Contact> = Vec::new();
    for (value, seg) in contact_segments().filter(|(_, seg)| *seg != "*") {
        let parsed = parse_contact(&value.reslice(seg));
        if wire {
            if let Some(reason) = validate_strict_sip_uri(&parsed.uri) {
                return Err(SipParseError::new(format!("Strict Contact URI: {reason} (\"{}\")", parsed.uri)));
            }
        }
        contact_list.push(to_contact(parsed));
    }
    let contacts =
        if contact_wildcard { ContactSet::Wildcard } else { ContactSet::Contacts(contact_list) };

    let mut hops = vias.into_iter();
    let top = hops.next().ok_or_else(|| SipParseError::new("Missing mandatory Via header"))?;

    Ok(CoreHeaders::new(
        header::From::new(to_name_addr(from_parsed)),
        header::To::new(to_name_addr(to_parsed)),
        header::CallId::new(call_id),
        header::CSeq::new(
            cseq_parsed.seq.min(u32::MAX as u64) as u32,
            Method::from_wire(&cseq_parsed.method),
        ),
        NonEmpty::from_parts(top, hops.collect()),
        contacts,
    ))
}

/// The sent-by colon count when a Via segment carries more than one colon
/// outside `[...]` and before the first `;` — IPv6 without brackets, or a
/// second port. `None` when the segment is well-formed.
fn sent_by_extra_colons(raw: &str) -> Option<i32> {
    let mut in_bracket = false;
    let mut colon_count = 0i32;
    for c in raw.bytes() {
        if c == b'[' {
            in_bracket = true;
            continue;
        }
        if c == b']' {
            in_bracket = false;
            continue;
        }
        if c == b';' && !in_bracket {
            break;
        }
        if !in_bracket && c == b':' {
            colon_count += 1;
        }
    }
    (colon_count > 1).then_some(colon_count)
}

// ---------------------------------------------------------------------------
// extractRequestFields
// ---------------------------------------------------------------------------

pub fn extract_request_fields(
    idx: &HeaderIndex,
    request_uri: &SipStr,
    limits: &SipParserLimits,
    method: Option<&str>,
    mode: ExtractMode,
) -> Result<RequestEager, SipParseError> {
    let common = extract_common_fields(idx, limits, mode)?;
    let wire = mode == ExtractMode::Wire;

    // Contact cardinality on dialog-creating requests.
    if wire {
        if let Some(method) = method {
            let dialog_creating = method.eq_ignore_ascii_case("INVITE")
                || method.eq_ignore_ascii_case("SUBSCRIBE")
                || method.eq_ignore_ascii_case("REFER");
            if dialog_creating {
                match common.contacts() {
                    ContactSet::Wildcard => {
                        return Err(SipParseError::new(format!(
                            "Contact: * wildcard is not valid in {} (RFC 3261 §10.2.2)",
                            method.to_ascii_uppercase()
                        )));
                    }
                    ContactSet::Contacts(cs) if cs.len() > 1 => {
                        return Err(SipParseError::new(format!(
                            "{} must not contain multiple Contact headers (RFC 3261 §8.1.1.8 — found {})",
                            method.to_ascii_uppercase(),
                            cs.len()
                        )));
                    }
                    _ => {}
                }
            }
        }
    }

    // CSeq method must match the request method.
    if let Some(method) = method {
        if let Some(cseq_val) = idx.cseq.first {
            let trimmed = cseq_val.trim();
            // First SP/HTAB is ASCII → a valid slice boundary.
            let cseq_method = match trimmed.bytes().position(|c| c == b' ' || c == b'\t') {
                Some(idx) => trimmed[idx + 1..].trim(),
                None => "",
            };
            if !cseq_method.is_empty() && !cseq_method.eq_ignore_ascii_case(method) {
                return Err(SipParseError::new(format!(
                    "CSeq method \"{cseq_method}\" does not match request method \"{method}\""
                )));
            }
        }
    }

    // Strict Request-URI gates.
    if wire {
        if let Some(reason) = validate_strict_sip_uri(request_uri) {
            return Err(SipParseError::new(format!("Strict Request-URI: {reason} (\"{request_uri}\")")));
        }
    }
    if has_unescaped_ctl_bytes(request_uri) {
        return Err(SipParseError::new(format!("Control byte in Request-URI: \"{request_uri}\"")));
    }
    if has_unbalanced_square_brackets(request_uri) {
        return Err(SipParseError::new(format!("Unbalanced IPv6 brackets in Request-URI: \"{request_uri}\"")));
    }
    if has_uri_port_trailing_garbage(request_uri) {
        return Err(SipParseError::new(format!("Trailing non-digit after Request-URI port: \"{request_uri}\"")));
    }
    if has_unbracketed_ipv6(request_uri) {
        return Err(SipParseError::new(format!("Unbracketed IPv6 in Request-URI: \"{request_uri}\"")));
    }
    let request_uri_parsed = parse_sip_uri_string(request_uri)
        .ok_or_else(|| SipParseError::new(format!("Malformed Request-URI: \"{request_uri}\"")))?;
    if request_uri_parsed.host.is_empty() {
        return Err(SipParseError::new(format!("Empty host in Request-URI: \"{request_uri}\"")));
    }
    if let Some(p) = request_uri_parsed.port {
        if !is_valid_port(p) {
            return Err(SipParseError::new(format!("Request-URI port out of range: {p}")));
        }
    }
    Ok(RequestEager { common, request_uri: Uri::parse_or_opaque(request_uri) })
}

// ---------------------------------------------------------------------------
// extractResponseFields
// ---------------------------------------------------------------------------

pub fn extract_response_fields(
    idx: &HeaderIndex,
    status: u16,
    limits: &SipParserLimits,
    mode: ExtractMode,
) -> Result<CoreHeaders, SipParseError> {
    let common = extract_common_fields(idx, limits, mode)?;
    if status > 100 && common.to().tag().is_none() {
        return Err(SipParseError::new(format!(
            "Non-100 response (status={status}) missing mandatory To-tag"
        )));
    }
    if mode == ExtractMode::Wire {
        let cseq_method = common.cseq().method().as_str().to_string();
        let is_redirect = status == 485 || (300..400).contains(&status);
        if !is_redirect && (cseq_method == "INVITE" || cseq_method == "SUBSCRIBE" || cseq_method == "REFER") {
            match common.contacts() {
                ContactSet::Wildcard => {
                    return Err(SipParseError::new(format!(
                        "Contact: * wildcard is not valid in a {status} response to {cseq_method} (RFC 3261 §10.2.2)"
                    )));
                }
                ContactSet::Contacts(cs) if cs.len() > 1 => {
                    return Err(SipParseError::new(format!(
                        "{status} response to {cseq_method} must not contain multiple Contact headers (RFC 3261 §12.1.1 — found {})",
                        cs.len()
                    )));
                }
                _ => {}
            }
        }
    }
    Ok(common)
}
