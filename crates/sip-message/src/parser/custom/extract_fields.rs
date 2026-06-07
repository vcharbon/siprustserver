//! Mandatory-header extraction + the ADR-0007 strict-grammar gates
//! (`Wire` vs `Hydrate` mode). Port of `src/sip/parsers/extract-fields.ts`.
//!
//! Produces the public eager-field model ([`crate::types`]) from the parsed
//! header list. `Wire` runs every gate (the security boundary on wire bytes);
//! `Hydrate` runs only the baseline presence/range/tag checks for already-
//! trusted internal construction.

use super::scanner::{is_token_char, strict_non_negative_decimal};
use super::structured_headers::{
    parse_contact, parse_cseq, parse_name_addr, parse_sip_uri_string, parse_via,
    split_top_level_commas, validate_strict_host, validate_strict_sip_uri,
};
use crate::error::SipParseError;
use crate::method::Method;
use crate::parser::SipParserLimits;
use crate::types::{Contact, ContactSet, CSeq, NameAddr, RequestUri, Via};

/// RFC 3261 §8.1.1.7 — top-Via branch MUST start with this magic cookie.
const VIA_BRANCH_MAGIC_COOKIE: &str = "z9hG4bK";

const INT_32_MAX: u64 = (1u64 << 31) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    Wire,
    Hydrate,
}

/// Common eager fields shared by requests and responses. `vias` is guaranteed
/// non-empty (extraction fails otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonEager {
    pub from: NameAddr,
    pub to: NameAddr,
    pub call_id: String,
    pub cseq: CSeq,
    pub vias: Vec<Via>,
    pub contact: Option<Contact>,
    pub contacts: ContactSet,
}

/// Request eager fields = common + parsed Request-URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestEager {
    pub common: CommonEager,
    pub request_uri: RequestUri,
}

// ---------------------------------------------------------------------------
// Header lookup helpers
// ---------------------------------------------------------------------------

fn get_header_value<'a>(headers: &'a [crate::types::SipHeader], name: &str) -> Option<&'a str> {
    let lower = name.to_lowercase();
    headers
        .iter()
        .find(|h| h.name.to_lowercase() == lower)
        .map(|h| h.value.as_str())
}

fn get_header_values<'a>(headers: &'a [crate::types::SipHeader], name: &str) -> Vec<&'a str> {
    let lower = name.to_lowercase();
    headers
        .iter()
        .filter(|h| h.name.to_lowercase() == lower)
        .map(|h| h.value.as_str())
        .collect()
}

/// RFC 3261 / RFC 3986 §3.2.3: SIP ports are 1..=65535.
fn is_valid_port(p: u64) -> bool {
    (1..=65535).contains(&p)
}

fn is_token_char_c(c: char) -> bool {
    (c as u32) < 0x80 && is_token_char(c as u8)
}

// ---------------------------------------------------------------------------
// Internal → public field mapping
// ---------------------------------------------------------------------------

fn to_name_addr(p: super::structured_headers::ParsedNameAddr) -> NameAddr {
    NameAddr { display_name: p.display_name, uri: p.uri, tag: p.tag, params: p.params }
}

fn to_contact(p: super::structured_headers::ParsedContact) -> Contact {
    Contact { display_name: p.display_name, uri: p.uri, params: p.params }
}

// ---------------------------------------------------------------------------
// Byte-scan strict gates (operate on &[char] for index fidelity with the TS)
// ---------------------------------------------------------------------------

/// True iff the URI carries unescaped control bytes (0x00-0x1F except HTAB,
/// or 0x7F).
fn has_unescaped_ctl_bytes(uri: &str) -> bool {
    for c in uri.chars() {
        let u = c as u32;
        if u == 0x09 {
            continue;
        }
        if u < 0x20 || u == 0x7f {
            return true;
        }
    }
    false
}

/// True iff `uri` contains `[` without a matching `]`.
fn has_unbalanced_square_brackets(uri: &str) -> bool {
    let mut open: i32 = 0;
    for c in uri.chars() {
        if c == '[' {
            open += 1;
        } else if c == ']' {
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
    let s: Vec<char> = uri.chars().collect();
    let scheme_colon = match s.iter().position(|&c| c == ':') {
        Some(i) => i as i64,
        None => return false,
    };
    let mut i = (scheme_colon + 1) as usize;
    if let Some(at) = (i..s.len()).find(|&j| s[j] == '@') {
        i = at + 1;
    }
    let mut depth = 0i32;
    let mut colons = 0i32;
    while i < s.len() {
        let c = s[i];
        if c == '[' {
            depth += 1;
        } else if c == ']' && depth > 0 {
            depth -= 1;
        } else if depth == 0 {
            if c == ':' {
                colons += 1;
                if colons >= 2 {
                    return true;
                }
            } else if c == ';' || c == '?' || c == '>' {
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
    let s: Vec<char> = uri.chars().collect();
    let first = match s.iter().position(|&c| c == '@') {
        Some(a) => a,
        None => match s.iter().position(|&c| c == ':') {
            Some(c) => c,
            None => return false,
        },
    };
    let mut i = first + 1; // past `@` or scheme colon
    if i < s.len() && s[i] == '[' {
        match (i + 1..s.len()).find(|&j| s[j] == ']') {
            None => return false,
            Some(close) => i = close + 1,
        }
    } else {
        while i < s.len() && s[i] != ':' && s[i] != ';' && s[i] != '?' && s[i] != '>' {
            i += 1;
        }
    }
    if i >= s.len() || s[i] != ':' {
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
    let s: Vec<char> = via.chars().collect();
    if s.is_empty() {
        return false;
    }
    let mut in_quote = false;
    let mut i = 0usize;
    while i + 1 < s.len() {
        let c = s[i];
        if in_quote {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_quote = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_quote = true;
            i += 1;
            continue;
        }
        if c != ':' {
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
    let s: Vec<char> = value.chars().collect();
    let mut count = 0usize;
    let mut in_quote = false;
    let mut i = 0usize;
    while i < s.len() {
        let c = s[i];
        if in_quote {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_quote = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_quote = true;
            i += 1;
            continue;
        }
        if c != ';' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < s.len() && (s[j] == ' ' || s[j] == '\t') {
            j += 1;
        }
        if j + 3 > s.len() {
            i += 1;
            continue;
        }
        let t = s[j];
        let a = s[j + 1];
        let g = s[j + 2];
        if (t != 't' && t != 'T') || (a != 'a' && a != 'A') || (g != 'g' && g != 'G') {
            i += 1;
            continue;
        }
        let mut k = j + 3;
        while k < s.len() && (s[k] == ' ' || s[k] == '\t') {
            k += 1;
        }
        if k >= s.len() || s[k] != '=' {
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
    let s: Vec<char> = via.chars().collect();
    let skip_ws = |mut i: usize| {
        while i < s.len() && (s[i] == ' ' || s[i] == '\t') {
            i += 1;
        }
        i
    };
    let read_token_run = |mut i: usize| {
        while i < s.len() && is_token_char_c(s[i]) {
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
    if i >= s.len() || s[i] != '/' {
        return Some("missing `/` after Via protocol-name".to_string());
    }
    i = skip_ws(i + 1);
    let ver_start = i;
    i = read_token_run(i);
    if i == ver_start {
        return Some("empty Via protocol-version".to_string());
    }
    i = skip_ws(i);
    if i >= s.len() || s[i] != '/' {
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
    headers: &[crate::types::SipHeader],
    limits: &SipParserLimits,
    mode: ExtractMode,
) -> Result<CommonEager, SipParseError> {
    let wire = mode == ExtractMode::Wire;

    // From/To/Call-ID/CSeq appear exactly once; only Via may repeat.
    {
        let (mut n_from, mut n_to, mut n_call_id, mut n_cseq) = (0, 0, 0, 0);
        for hdr in headers {
            match hdr.name.to_lowercase().as_str() {
                "from" => n_from += 1,
                "to" => n_to += 1,
                "call-id" => n_call_id += 1,
                "cseq" => n_cseq += 1,
                _ => {}
            }
        }
        if n_from > 1 {
            return Err(SipParseError::new("Multiple From headers (RFC 3261 §8.1.1 — exactly one required)"));
        }
        if n_to > 1 {
            return Err(SipParseError::new("Multiple To headers (RFC 3261 §8.1.1 — exactly one required)"));
        }
        if n_call_id > 1 {
            return Err(SipParseError::new("Multiple Call-ID headers (RFC 3261 §8.1.1 — exactly one required)"));
        }
        if n_cseq > 1 {
            return Err(SipParseError::new("Multiple CSeq headers (RFC 3261 §8.1.1 — exactly one required)"));
        }
    }

    let from_val = get_header_value(headers, "From")
        .ok_or_else(|| SipParseError::new("Missing mandatory From header"))?;
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

    let to_val = get_header_value(headers, "To")
        .ok_or_else(|| SipParseError::new("Missing mandatory To header"))?;
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

    let call_id = match get_header_value(headers, "Call-ID") {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => return Err(SipParseError::new("Missing mandatory Call-ID header")),
    };

    let cseq_val = get_header_value(headers, "CSeq")
        .ok_or_else(|| SipParseError::new("Missing mandatory CSeq header"))?;
    let cseq_parsed = parse_cseq(cseq_val);
    if wire {
        let cseq_raw = cseq_val.trim();
        let cseq_chars: Vec<char> = cseq_raw.chars().collect();
        let space_idx = cseq_chars.iter().position(|&c| c == ' ' || c == '\t');
        match space_idx {
            None => {
                return Err(SipParseError::new(format!("CSeq missing method token: \"{cseq_val}\"")));
            }
            Some(idx) => {
                let cseq_digits: String = cseq_chars[..idx].iter().collect();
                if strict_non_negative_decimal(&cseq_digits, INT_32_MAX).is_none() {
                    return Err(SipParseError::new(format!(
                        "CSeq seq malformed (paranoid digit check): \"{cseq_digits}\""
                    )));
                }
                let method_token: String = cseq_chars[idx + 1..].iter().collect();
                if method_token.trim().is_empty() {
                    return Err(SipParseError::new(format!("CSeq missing method token: \"{cseq_val}\"")));
                }
            }
        }
    }

    // Via — at least one required; both comma-list and repeated-line encodings
    // fold into one ordered list.
    let via_values = get_header_values(headers, "Via");
    if via_values.is_empty() {
        return Err(SipParseError::new("Missing mandatory Via header"));
    }
    for v in &via_values {
        for segment in split_top_level_commas(v) {
            if has_via_port_trailing_garbage(&segment) {
                return Err(SipParseError::new(format!("Trailing non-digit after Via port: \"{segment}\"")));
            }
            if wire {
                if let Some(reason) = check_sent_protocol(&segment) {
                    return Err(SipParseError::new(format!("{reason}: \"{segment}\"")));
                }
            }
        }
    }
    let via_segments: Vec<String> =
        via_values.iter().flat_map(|v| split_top_level_commas(v)).collect();
    let vias_parsed: Vec<_> = via_segments.iter().map(|seg| parse_via(seg)).collect();

    for (idx, v) in vias_parsed.iter().enumerate() {
        let raw = &via_segments[idx];
        if let Some(p) = v.port {
            if !is_valid_port(p) {
                return Err(SipParseError::new(format!("Via port out of range: {p}")));
            }
        }
        if v.branch.as_deref() == Some("") {
            return Err(SipParseError::new("Empty Via branch parameter"));
        }
        if !wire {
            continue;
        }
        // Top-Via magic cookie (RFC 3261 §8.1.1.7) — topmost Via only.
        if idx == 0 {
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
        // Multiple-colon sent-by (outside `[...]`, before `;`).
        {
            let mut in_bracket = false;
            let mut colon_count = 0i32;
            for c in raw.chars() {
                if c == '[' {
                    in_bracket = true;
                    continue;
                }
                if c == ']' {
                    in_bracket = false;
                    continue;
                }
                if c == ';' && !in_bracket {
                    break;
                }
                if !in_bracket && c == ':' {
                    colon_count += 1;
                }
            }
            if colon_count > 1 {
                return Err(SipParseError::new(format!(
                    "Via sent-by has {colon_count} colons (must be ≤ 1): \"{raw}\""
                )));
            }
        }
        if !limits.allowed_transports.contains(&v.transport.to_uppercase()) {
            return Err(SipParseError::new(format!("Via transport \"{}\" not in allowed set", v.transport)));
        }
    }
    let vias: Vec<Via> = vias_parsed
        .into_iter()
        .map(|v| Via {
            transport: v.transport,
            host: v.host,
            port: v.port.map(|p| p as u16),
            branch: v.branch,
            params: v.params,
        })
        .collect();

    // Contact — parse + validate every value; fold comma-list and repeated
    // lines; `Contact: *` must stand alone.
    let mut contact_wildcard = false;
    let mut contact_entries: Vec<String> = Vec::new();
    for v in get_header_values(headers, "Contact") {
        for seg in split_top_level_commas(v) {
            let trimmed = seg.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed == "*" {
                contact_wildcard = true;
                continue;
            }
            contact_entries.push(seg);
        }
    }
    if contact_wildcard && !contact_entries.is_empty() {
        return Err(SipParseError::new("Contact: * wildcard must be the only value (RFC 3261 §10.2.2)"));
    }
    let mut contact_list: Vec<Contact> = Vec::new();
    for entry in &contact_entries {
        let parsed = parse_contact(entry);
        if wire {
            if let Some(reason) = validate_strict_sip_uri(&parsed.uri) {
                return Err(SipParseError::new(format!("Strict Contact URI: {reason} (\"{}\")", parsed.uri)));
            }
        }
        contact_list.push(to_contact(parsed));
    }
    let contacts = if contact_wildcard {
        ContactSet::Wildcard
    } else {
        ContactSet::Contacts(contact_list.clone())
    };
    let contact = if contact_wildcard { None } else { contact_list.first().cloned() };

    Ok(CommonEager {
        from: to_name_addr(from_parsed),
        to: to_name_addr(to_parsed),
        call_id,
        cseq: CSeq {
            seq: cseq_parsed.seq.min(u32::MAX as u64) as u32,
            method: Method::from_wire(&cseq_parsed.method),
        },
        vias,
        contact,
        contacts,
    })
}

// ---------------------------------------------------------------------------
// extractRequestFields
// ---------------------------------------------------------------------------

pub fn extract_request_fields(
    headers: &[crate::types::SipHeader],
    request_uri: &str,
    limits: &SipParserLimits,
    method: Option<&str>,
    mode: ExtractMode,
) -> Result<RequestEager, SipParseError> {
    let common = extract_common_fields(headers, limits, mode)?;
    let wire = mode == ExtractMode::Wire;

    // Contact cardinality on dialog-creating requests.
    if wire {
        if let Some(method) = method {
            let m = method.to_uppercase();
            if m == "INVITE" || m == "SUBSCRIBE" || m == "REFER" {
                match &common.contacts {
                    ContactSet::Wildcard => {
                        return Err(SipParseError::new(format!(
                            "Contact: * wildcard is not valid in {m} (RFC 3261 §10.2.2)"
                        )));
                    }
                    ContactSet::Contacts(cs) if cs.len() > 1 => {
                        return Err(SipParseError::new(format!(
                            "{m} must not contain multiple Contact headers (RFC 3261 §8.1.1.8 — found {})",
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
        if let Some(cseq_val) = get_header_value(headers, "CSeq") {
            let cseq_method = {
                let trimmed: Vec<char> = cseq_val.trim().chars().collect();
                let mut found = String::new();
                for i in 0..trimmed.len() {
                    if trimmed[i] == ' ' || trimmed[i] == '\t' {
                        found = trimmed[i + 1..].iter().collect::<String>().trim().to_string();
                        break;
                    }
                }
                found
            };
            if !cseq_method.is_empty() && cseq_method.to_uppercase() != method.to_uppercase() {
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
    let request_uri_field = RequestUri {
        scheme: request_uri_parsed.scheme,
        user: request_uri_parsed.user,
        host: request_uri_parsed.host,
        port: request_uri_parsed.port.map(|p| p as u16),
        params: request_uri_parsed.params,
    };

    Ok(RequestEager { common, request_uri: request_uri_field })
}

// ---------------------------------------------------------------------------
// extractResponseFields
// ---------------------------------------------------------------------------

pub fn extract_response_fields(
    headers: &[crate::types::SipHeader],
    status: u16,
    limits: &SipParserLimits,
    mode: ExtractMode,
) -> Result<CommonEager, SipParseError> {
    let common = extract_common_fields(headers, limits, mode)?;
    if status > 100 && common.to.tag.is_none() {
        return Err(SipParseError::new(format!(
            "Non-100 response (status={status}) missing mandatory To-tag"
        )));
    }
    if mode == ExtractMode::Wire {
        let cseq_method = common.cseq.method.to_string();
        let is_redirect = status == 485 || (300..400).contains(&status);
        if !is_redirect && (cseq_method == "INVITE" || cseq_method == "SUBSCRIBE" || cseq_method == "REFER") {
            match &common.contacts {
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
