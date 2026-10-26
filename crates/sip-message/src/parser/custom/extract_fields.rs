//! Mandatory-header extraction + the strict wire-grammar gates (`Wire` vs
//! `Hydrate` mode; those beyond the RFC grammar are listed in ADR-0001).
//!
//! Produces the typed [`CoreHeaders`] from the
//! [`HeaderIndex`] of one dispatch pass — this module locates nothing itself,
//! it validates and parses what the index holds. `Wire` runs every gate (the
//! security boundary on wire bytes);
//! `Hydrate` runs only the baseline presence/range/tag checks for already-
//! trusted internal construction.

use super::contact_entries::{
    contact_entry_spans, read_contact_entry, read_contact_entry_lenient, ContactEntry,
};
use super::dialog_contact::{can_establish_dialog, dialog_contact_refusal};
use super::dialog_ids::{count_tag_params, is_call_id, read_address, tag_refusal};
use super::header_index::HeaderIndex;
use super::request_uri::read_request_uri;
use super::routes::route_refusal;
use super::structured_headers::{parse_cseq, parse_via, top_level_comma_entries};
use super::top_via;
use crate::char_class::is_token_char;
use crate::error::SipParseError;
use crate::header::{self, HeaderValue, HostPort, Uri};
use crate::method::Method;
use crate::parser::SipParserLimits;
use crate::sip_str::SipStr;
use crate::types::{ContactSet, CoreHeaders, NonEmpty};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    Wire,
    Hydrate,
}

/// How a message's Contact set and route sets are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactReading {
    /// The Contact names a dialog target or a binding (a request, a 1xx / 2xx):
    /// a misplaced wildcard or an unreadable entry refuses the message, and so
    /// does a Route or Record-Route that does not read.
    Strict,
    /// The Contact names no dialog target (a final of 300 or more, §12.1.1): a
    /// misplaced wildcard or an unreadable entry is dropped and the message
    /// kept, so the transaction it ends still completes. Its route sets are
    /// not read at parse.
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
    let identity = extract_identity(idx, limits, mode)?;
    let wire = mode == ExtractMode::Wire;
    let contacts = read_contacts(idx, wire, contact_reading)?;
    if wire && contact_reading == ContactReading::Strict {
        route_refusal(idx)?;
    }
    let Identity { from, to, call_id, cseq, via } = identity;
    Ok(CoreHeaders::new(from, to, call_id, cseq, via, contacts))
}

/// The fields every response to a message echoes (RFC 3261 §8.2.6.2), typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Identity {
    pub from: header::From,
    pub to: header::To,
    pub call_id: header::CallId,
    pub cseq: header::CSeq,
    pub via: NonEmpty<header::Via>,
}

/// From, To, Call-ID, CSeq and every Via entry of `idx`, each held to the
/// rules of `mode`.
pub(super) fn extract_identity(
    idx: &HeaderIndex,
    limits: &SipParserLimits,
    mode: ExtractMode,
) -> Result<Identity, SipParseError> {
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
    let from = read_address(from_val, "From", wire)?;
    if from.params().value("tag") == Some("") {
        return Err(SipParseError::new("Empty From tag parameter"));
    }
    if count_tag_params(from_val) > 1 {
        return Err(SipParseError::new("Duplicate From tag parameter"));
    }

    let to_val = idx.to.first.ok_or_else(|| SipParseError::new("Missing mandatory To header"))?;
    let to = read_address(to_val, "To", wire)?;
    if to.params().value("tag") == Some("") {
        return Err(SipParseError::new("Empty To tag parameter"));
    }
    if count_tag_params(to_val) > 1 {
        return Err(SipParseError::new("Duplicate To tag parameter"));
    }
    if wire {
        tag_refusal(&from, "From")?;
        tag_refusal(&to, "To")?;
    }

    let call_id = match idx.call_id.first {
        Some(v) if !v.is_empty() => v.clone(),
        _ => return Err(SipParseError::new("Missing mandatory Call-ID header")),
    };
    if wire && !is_call_id(&call_id) {
        return Err(SipParseError::new(format!(
            "Call-ID is not word [\"@\" word] (RFC 3261 §25.1): \"{call_id}\""
        )));
    }

    let cseq_val =
        idx.cseq.first.ok_or_else(|| SipParseError::new("Missing mandatory CSeq header"))?;
    let cseq = if wire {
        header::CSeq::parse(cseq_val)?
    } else {
        // The lenient read saturates a sequence number past `u32`.
        let lenient = parse_cseq(cseq_val);
        header::CSeq::new(
            u32::try_from(lenient.seq).unwrap_or(u32::MAX),
            Method::from_wire(&lenient.method),
        )
    };

    // Via — at least one required; both comma-list and repeated-line encodings
    // fold into one ordered list. Each segment stays a span of its header
    // value, so folding a comma-list into the ordered Via list copies nothing.
    if idx.via.is_empty() {
        return Err(SipParseError::new("Missing mandatory Via header"));
    }
    let via_segments = || idx.via.iter().flat_map(|v| top_level_comma_entries(v.as_str()));
    if wire {
        for segment in via_segments() {
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
        // A lenient reading holds a sent-by the reader refuses whole, as it
        // holds a URI it refuses opaque.
        let (host, port) = match &v.sent_by {
            Ok((host, port)) => (Some(host.clone()), *port),
            Err(reason) if wire => {
                return Err(SipParseError::new(format!("Strict Via sent-by: {reason}: \"{raw}\"")));
            }
            Err(_) => (None, None),
        };
        if let Some(p) = port.filter(|&p| top_via::port_out_of_range(p, top)) {
            return Err(SipParseError::new(format!("Via port out of range: {p}")));
        }
        if v.branch.as_deref() == Some("") {
            return Err(SipParseError::new("Empty Via branch parameter"));
        }
        if wire {
            if let Some(refusal) = top.then(|| top_via::cookie_refusal(&v)).flatten() {
                return Err(refusal);
            }
            if v.branch.as_ref().is_some_and(|b| !b.bytes().all(is_token_char)) {
                return Err(SipParseError::new(format!(
                    "Via branch is not a token: \"{}\"",
                    v.branch.as_deref().unwrap_or_default()
                )));
            }
            if let Some(refusal) = top.then(|| top_via::transport_refusal(&v, limits)).flatten() {
                return Err(refusal);
            }
        }
        let port = port
            .map(u16::try_from)
            .transpose()
            .map_err(|_| SipParseError::new(format!("Via port out of range: {port:?}")))?;
        let sent_by = match host {
            Some(host) => HostPort::new(host, port),
            None => HostPort::whole(v.sent_by_text),
        };
        vias.push(header::Via::from_parts(v.protocol, v.version, v.transport, sent_by, v.params));
    }

    let mut hops = vias.into_iter();
    let top = hops.next().ok_or_else(|| SipParseError::new("Missing mandatory Via header"))?;

    Ok(Identity {
        from: header::From::new(from),
        to: header::To::new(to),
        call_id: header::CallId::new(call_id),
        cseq,
        via: NonEmpty::from_parts(top, hops.collect()),
    })
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
            ContactEntry::Readable(contact) => contact_list.push(*contact),
            // The grammar holds on wire bytes only.
            ContactEntry::Unreadable { .. } if !wire => {
                contact_list.push(read_contact_entry_lenient(&span));
            }
            ContactEntry::Unreadable { .. } if !strict => {}
            ContactEntry::Unreadable { reason } => {
                return Err(SipParseError::new(format!("Strict Contact: {reason} (\"{span}\")")));
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
        if let Some(method) = method.filter(|m| can_establish_dialog(m)) {
            if let Some(refusal) = dialog_contact_refusal(common.contacts(), method) {
                return Err(refusal);
            }
        }
        if let Some(method) = method {
            if matches!(method, "INVITE" | "SUBSCRIBE" | "REFER") {
                match common.contacts() {
                    ContactSet::Wildcard => {
                        return Err(SipParseError::new(format!(
                            "Contact: * wildcard is not valid in {method} (RFC 3261 §10.2.2)"
                        )));
                    }
                    ContactSet::Contacts(cs) if cs.len() > 1 => {
                        return Err(SipParseError::new(format!(
                            "{method} must not contain multiple Contact headers (RFC 3261 §8.1.1.8 — found {})",
                            cs.len()
                        )));
                    }
                    _ => {}
                }
            }
        }
    }

    // CSeq method must match the request method, spelling for spelling
    // (RFC 3261 §7.1, §8.1.1.5).
    if let Some(method) = method {
        if let Some(cseq_val) = idx.cseq.first {
            let trimmed = cseq_val.trim();
            let cseq_method = match trimmed.split_once([' ', '\t']) {
                Some((_, method)) => method.trim(),
                None => "",
            };
            if !cseq_method.is_empty()
                && Method::from_wire(cseq_method) != Method::from_wire(method)
            {
                return Err(SipParseError::new(format!(
                    "CSeq method \"{cseq_method}\" does not match request method \"{method}\""
                )));
            }
        }
    }

    // The Request-URI is read in every mode: a URI that does not read is no
    // request's target, whoever built the request.
    let request_uri = read_request_uri(request_uri)?;
    Ok(RequestEager { common, request_uri })
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
        if reading == ContactReading::Strict && can_establish_dialog(&cseq_method) {
            let what = format!("A {status} response to {cseq_method}");
            if let Some(refusal) = dialog_contact_refusal(common.contacts(), &what) {
                return Err(refusal);
            }
        }
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
