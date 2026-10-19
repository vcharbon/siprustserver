//! Mandatory-header extraction + the strict wire-grammar gates (`Wire` vs
//! `Hydrate` mode; those beyond the RFC grammar are listed in ADR-0001). Port of `src/sip/parsers/extract-fields.ts`.
//!
//! Produces the typed [`CoreHeaders`] from the
//! [`HeaderIndex`] of one dispatch pass — this module locates nothing itself,
//! it validates and parses what the index holds. `Wire` runs every gate (the
//! security boundary on wire bytes);
//! `Hydrate` runs only the baseline presence/range/tag checks for already-
//! trusted internal construction.

use super::contact_entries::{contact_entry_spans, read_contact_entry, ContactEntry};
use super::header_index::HeaderIndex;
use super::port_garbage::has_via_port_trailing_garbage;
use super::request_uri::request_uri_refusal;
use super::scanner::{is_token_char, strict_non_negative_decimal};
use super::structured_headers::{
    parse_cseq, parse_name_addr, parse_via, top_level_comma_entries, validate_strict_host,
    validate_strict_sip_uri,
};
use super::top_via;
use crate::error::SipParseError;
use crate::header::{self, HostPort, NameAddr, Uri};
use crate::method::Method;
use crate::parser::SipParserLimits;
use crate::sip_str::SipStr;
use crate::types::{ContactSet, CoreHeaders, NonEmpty};

const INT_32_MAX: u64 = (1u64 << 31) - 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    Wire,
    Hydrate,
}

/// How a message's Contact set is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactReading {
    /// The Contact names a dialog target or a binding (a request, a 1xx / 2xx):
    /// a misplaced wildcard or an unreadable entry refuses the message.
    Strict,
    /// The Contact names no dialog target (a final of 300 or more): a misplaced
    /// wildcard or an unreadable entry is dropped and the message kept, so the
    /// transaction it ends still completes.
    Lenient,
}

/// Request eager fields = the shared core + the parsed Request-URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestEager {
    pub common: CoreHeaders,
    pub request_uri: Uri,
}

/// RFC 3261 / RFC 3986 §3.2.3: SIP ports are 1..=65535.
pub(super) fn is_valid_port(p: u64) -> bool {
    (1..=65535).contains(&p)
}

// ---------------------------------------------------------------------------
// Internal → public field mapping
// ---------------------------------------------------------------------------

/// A scanned name-addr as the typed address value. A URI the strict reader
/// rejects is kept whole (the parser's own gates decide admissibility), so no
/// reader ever loses what the peer sent.
fn to_name_addr(p: super::structured_headers::ParsedNameAddr) -> NameAddr {
    NameAddr::from_parts(p.display_name, Uri::parse_or_verbatim(&p.uri), p.params)
}

fn to_contact(p: super::structured_headers::ParsedContact) -> header::Contact {
    header::Contact::new(NameAddr::from_parts(
        p.display_name,
        Uri::parse_or_verbatim(&p.uri),
        p.params,
    ))
}

/// True iff the name-addr's `<` opens an addr-spec no `>` closes (RFC 3261
/// §25.1 `name-addr = [ display-name ] LAQUOT addr-spec RAQUOT`): a parameter
/// written after it would read as part of the URI.
fn has_unclosed_angle(value: &str) -> bool {
    let s = value.as_bytes();
    s.iter().rposition(|&b| b == b'<').is_some_and(|lt| !s.iter().skip(lt).any(|&b| b == b'>'))
}

/// Count `;tag=` occurrences outside quoted-strings (RFC 3261 permits one).
fn count_tag_params(value: &str) -> usize {
    let s = value.as_bytes();
    let at = |k: usize| s.get(k).copied();
    let skip_ws = |mut k: usize| {
        while matches!(at(k), Some(b' ' | b'\t')) {
            k += 1;
        }
        k
    };
    let mut count = 0usize;
    let mut in_quote = false;
    let mut i = 0usize;
    while let Some(c) = at(i) {
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
        let j = skip_ws(i + 1);
        let is_tag = s.get(j..j + 3).is_some_and(|name| name.eq_ignore_ascii_case(b"tag"));
        if !is_tag || at(skip_ws(j + 3)) != Some(b'=') {
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
    let at = |i: usize| s.get(i).copied();
    let skip_ws = |mut i: usize| {
        while matches!(at(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        i
    };
    let read_token_run = |mut i: usize| {
        while at(i).is_some_and(is_token_char) {
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
    if at(i) != Some(b'/') {
        return Some("missing `/` after Via protocol-name".to_string());
    }
    i = skip_ws(i + 1);
    let ver_start = i;
    i = read_token_run(i);
    if i == ver_start {
        return Some("empty Via protocol-version".to_string());
    }
    i = skip_ws(i);
    if at(i) != Some(b'/') {
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
    contact_reading: ContactReading,
) -> Result<CoreHeaders, SipParseError> {
    let wire = mode == ExtractMode::Wire;

    // From/To/Call-ID/CSeq appear exactly once; only Via may repeat.
    if idx.from.count > 1 {
        return Err(SipParseError::new(
            "Multiple From headers (RFC 3261 §8.1.1 — exactly one required)",
        ));
    }
    if idx.to.count > 1 {
        return Err(SipParseError::new(
            "Multiple To headers (RFC 3261 §8.1.1 — exactly one required)",
        ));
    }
    if idx.call_id.count > 1 {
        return Err(SipParseError::new(
            "Multiple Call-ID headers (RFC 3261 §8.1.1 — exactly one required)",
        ));
    }
    if idx.cseq.count > 1 {
        return Err(SipParseError::new(
            "Multiple CSeq headers (RFC 3261 §8.1.1 — exactly one required)",
        ));
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
            return Err(SipParseError::new(format!(
                "Strict From URI: {reason} (\"{}\")",
                from_parsed.uri
            )));
        }
        if has_unclosed_angle(from_val) {
            return Err(SipParseError::new("Strict From: unterminated \"<...>\""));
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
            return Err(SipParseError::new(format!(
                "Strict To URI: {reason} (\"{}\")",
                to_parsed.uri
            )));
        }
        if has_unclosed_angle(to_val) {
            return Err(SipParseError::new("Strict To: unterminated \"<...>\""));
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
        match cseq_raw.split_once([' ', '\t']) {
            None => {
                return Err(SipParseError::new(format!(
                    "CSeq missing method token: \"{cseq_val}\""
                )));
            }
            Some((cseq_digits, cseq_method)) => {
                if strict_non_negative_decimal(cseq_digits, INT_32_MAX).is_none() {
                    return Err(SipParseError::new(format!(
                        "CSeq seq malformed (paranoid digit check): \"{cseq_digits}\""
                    )));
                }
                if cseq_method.trim().is_empty() {
                    return Err(SipParseError::new(format!(
                        "CSeq missing method token: \"{cseq_val}\""
                    )));
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
            return Err(SipParseError::new(format!(
                "Trailing non-digit after Via port: \"{segment}\""
            )));
        }
        if wire {
            if let Some(reason) = check_sent_protocol(segment) {
                return Err(SipParseError::new(format!("{reason}: \"{segment}\"")));
            }
        }
    }
    let mut vias: Vec<header::Via> = Vec::with_capacity(idx.via.len());
    for (value, raw) in
        idx.via.iter().flat_map(|v| top_level_comma_entries(v.as_str()).map(move |s| (*v, s)))
    {
        let v = parse_via(&value.reslice(raw));
        let top = vias.is_empty();
        if let Some(p) = v.port.filter(|&p| top_via::port_out_of_range(p, top)) {
            return Err(SipParseError::new(format!("Via port out of range: {p}")));
        }
        if v.branch.as_deref() == Some("") {
            return Err(SipParseError::new("Empty Via branch parameter"));
        }
        if wire {
            if let Some(refusal) = top.then(|| top_via::cookie_refusal(&v)).flatten() {
                return Err(refusal);
            }
            if let Some(reason) = validate_strict_host(&v.host) {
                return Err(SipParseError::new(format!(
                    "Strict Via sent-by host: {reason} (\"{}\")",
                    v.host
                )));
            }
            if let Some(colon_count) = sent_by_extra_colons(raw) {
                return Err(SipParseError::new(format!(
                    "Via sent-by has {colon_count} colons (must be ≤ 1): \"{raw}\""
                )));
            }
            if let Some(refusal) = top.then(|| top_via::transport_refusal(&v, limits)).flatten() {
                return Err(refusal);
            }
        }
        let port = v
            .port
            .map(u16::try_from)
            .transpose()
            .map_err(|_| SipParseError::new(format!("Via port out of range: {:?}", v.port)))?;
        vias.push(header::Via::from_parts(
            v.protocol,
            v.version,
            v.transport,
            HostPort::new(v.host, port),
            v.params,
        ));
    }

    let contacts = read_contacts(idx, wire, contact_reading)?;

    let mut hops = vias.into_iter();
    let top = hops.next().ok_or_else(|| SipParseError::new("Missing mandatory Via header"))?;

    Ok(CoreHeaders::new(
        header::From::new(to_name_addr(from_parsed)),
        header::To::new(to_name_addr(to_parsed)),
        header::CallId::new(call_id),
        // The lenient read saturates a sequence number past `u32`; the wire
        // read refused it above.
        header::CSeq::new(
            u32::try_from(cseq_parsed.seq).unwrap_or(u32::MAX),
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

/// The message's Contact set, folded over comma-lists and repeated lines.
/// `Contact: *` must stand alone, which is settled over every entry before any
/// is parsed; `contact_reading` says whether a misplaced wildcard or an
/// unreadable entry refuses the message or is dropped.
fn read_contacts(
    idx: &HeaderIndex,
    wire: bool,
    contact_reading: ContactReading,
) -> Result<ContactSet, SipParseError> {
    let spans = || idx.contact.iter().flat_map(|v| contact_entry_spans(v));
    let strict = contact_reading == ContactReading::Strict;
    let contact_wildcard = spans().any(|e| e.as_str() == "*");
    if strict && contact_wildcard && spans().any(|e| e.as_str() != "*") {
        return Err(SipParseError::new(
            "Contact: * wildcard must be the only value (RFC 3261 §10.2.2)",
        ));
    }
    let mut contact_list: Vec<header::Contact> = Vec::new();
    for span in spans() {
        match read_contact_entry(&span) {
            ContactEntry::Wildcard => {}
            ContactEntry::Readable(contact) => contact_list.push(to_contact(contact)),
            // The strict URI gate holds on wire bytes only.
            ContactEntry::Unreadable { contact, .. } if !wire => {
                contact_list.push(to_contact(contact));
            }
            ContactEntry::Unreadable { .. } if !strict => {}
            ContactEntry::Unreadable { contact, reason } => {
                return Err(SipParseError::new(format!(
                    "Strict Contact URI: {reason} (\"{}\")",
                    contact.uri
                )));
            }
        }
    }
    // A lenient read keeps the readable entries beside a misplaced wildcard.
    Ok(if contact_wildcard && contact_list.is_empty() {
        ContactSet::Wildcard
    } else {
        ContactSet::Contacts(contact_list)
    })
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
    let common = extract_common_fields(idx, limits, mode, ContactReading::Strict)?;
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
            let cseq_method = match trimmed.split_once([' ', '\t']) {
                Some((_, method)) => method.trim(),
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
            return Err(SipParseError::new(format!(
                "Strict Request-URI: {reason} (\"{request_uri}\")"
            )));
        }
    }
    request_uri_refusal(request_uri)?;
    Ok(RequestEager { common, request_uri: Uri::parse_or_verbatim(request_uri) })
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
    // A final of 300 or more names no dialog target (RFC 3261 §12.1.1 governs
    // the 1xx / 2xx that establish one): its Contact set never refuses it.
    let reading = if status >= 300 { ContactReading::Lenient } else { ContactReading::Strict };
    let common = extract_common_fields(idx, limits, mode, reading)?;
    if status > 100 && common.to().tag().is_none() {
        return Err(SipParseError::new(format!(
            "Non-100 response (status={status}) missing mandatory To-tag"
        )));
    }
    if mode == ExtractMode::Wire {
        let cseq_method = common.cseq().method().as_str().to_string();
        if reading == ContactReading::Strict
            && (cseq_method == "INVITE" || cseq_method == "SUBSCRIBE" || cseq_method == "REFER")
        {
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
