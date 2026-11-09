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
use super::dialog_ids::{read_address, tag_count, tag_refusal, uri_repeat_refusal};
use super::header_index::HeaderIndex;
use super::request_uri::read_request_uri;
use super::routes::read_routes;
use super::structured_headers::{parse_cseq, top_level_comma_entries};
use super::top_via;
use crate::error::SipParseError;
use crate::header::is_call_id;
use crate::header::{self, HeaderName, HeaderValue, Reading, Uri};
use crate::method::Method;
use crate::parser::{Exemption, SipParserLimits};
use crate::sip_str::SipStr;
use smallvec::SmallVec;

use crate::types::{ContactSet, CoreHeaders, NonEmpty, ReadRoutes};

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
    let contacts = read_contacts(idx, limits, wire, contact_reading)?;
    let routes = if wire && contact_reading == ContactReading::Strict {
        read_routes(idx, &limits.exemptions)?
    } else {
        ReadRoutes::default()
    };
    let Identity { from, to, call_id, cseq, via } = identity;
    Ok(CoreHeaders::new(from, to, call_id, cseq, via, contacts, routes))
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

    let exemptions = &limits.exemptions;
    let from_val =
        idx.from.value.ok_or_else(|| SipParseError::new("Missing mandatory From header"))?;
    let from = read_address(from_val, HeaderName::From, wire, exemptions)?;
    if from.params().value("tag") == Some("") {
        return Err(SipParseError::new("Empty From tag parameter"));
    }
    if limits.value_gates && tag_count(&from) > 1 {
        return Err(SipParseError::new("Duplicate From tag parameter"));
    }

    let to_val = idx.to.value.ok_or_else(|| SipParseError::new("Missing mandatory To header"))?;
    let to = read_address(to_val, HeaderName::To, wire, exemptions)?;
    if to.params().value("tag") == Some("") {
        return Err(SipParseError::new("Empty To tag parameter"));
    }
    if limits.value_gates && tag_count(&to) > 1 {
        return Err(SipParseError::new("Duplicate To tag parameter"));
    }
    if wire {
        tag_refusal(&from, "From")?;
        tag_refusal(&to, "To")?;
    }

    let call_id = match idx.call_id.value {
        Some(v) if !v.is_empty() => v.clone(),
        _ => return Err(SipParseError::new("Missing mandatory Call-ID header")),
    };
    if wire && !is_call_id(&call_id) {
        return Err(SipParseError::new(format!(
            "Call-ID is not word [\"@\" word] (RFC 3261 §25.1){}: {call_id:?}",
            crate::char_class::utf8_note(&call_id)
        )));
    }

    let cseq_val =
        idx.cseq.value.ok_or_else(|| SipParseError::new("Missing mandatory CSeq header"))?;
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
    // The top hop and the lower ones, each list at its exact size; each entry
    // stays a span of its line.
    let reading = if wire { Reading::Strict } else { Reading::Lenient };
    let mut head: Option<header::Via> = None;
    let mut hops: Vec<header::Via> = Vec::with_capacity(idx.via.len().saturating_sub(1));
    for line in &idx.via {
        if line.is_empty() {
            return Err(SipParseError::new("Empty Via header (RFC 3261 §25.1)"));
        }
        for entry in top_level_comma_entries(line.as_str()) {
            let via = header::Via::read(&line.reslice(entry), reading)
                .map_err(|e| SipParseError::new(format!("Strict {}", e.reason)))?;
            let top = head.is_none();
            if let Some(p) = via.port().filter(|&p| top_via::port_out_of_range(p, top)) {
                return Err(SipParseError::new(format!("Via port out of range: {p}")));
            }
            if via.branch() == Some("") {
                return Err(SipParseError::new("Empty Via branch parameter"));
            }
            if wire && top {
                top_via::cookie_refusal(&via).map_or(Ok(()), Err)?;
                top_via::transport_refusal(&via, limits).map_or(Ok(()), Err)?;
            }
            match head {
                None => head = Some(via),
                Some(_) => hops.push(via),
            }
        }
    }

    let top = head.ok_or_else(|| SipParseError::new("Missing mandatory Via header"))?;

    Ok(Identity {
        from: header::From::new(from),
        to: header::To::new(to),
        call_id: header::CallId::new(call_id),
        cseq,
        via: NonEmpty::from_parts(top, hops),
    })
}

/// The message's Contact set, folded over comma-lists and repeated lines.
/// `Contact: *` must stand alone, which is settled over every entry before any
/// is parsed; `contact_reading` says whether a misplaced wildcard or an
/// unreadable entry refuses the message or is dropped, on wire and stored bytes
/// alike. A strict set read from stored bytes keeps an unreadable entry, read
/// leniently. An empty line or entry
/// is unreadable (RFC 3261 §25.1), and on wire bytes so is a URI with a
/// parameter written twice that `limits` does not exempt.
fn read_contacts(
    idx: &HeaderIndex,
    limits: &SipParserLimits,
    wire: bool,
    contact_reading: ContactReading,
) -> Result<ContactSet, SipParseError> {
    // Each line is split into its entries once, for the wildcard rule and the
    // read alike.
    let mut spans: SmallVec<[SipStr; 2]> = SmallVec::new();
    for line in &idx.contact {
        let before = spans.len();
        spans.extend(contact_entry_spans(line));
        if spans.len() == before {
            spans.push((*line).clone());
        }
    }
    let strict = contact_reading == ContactReading::Strict;
    let contact_wildcard = spans.iter().any(|e| e.as_str() == "*");
    if strict && contact_wildcard && spans.iter().any(|e| e.as_str() != "*") {
        return Err(SipParseError::new(
            "Contact: * wildcard must be the only value (RFC 3261 §10.2.2)",
        ));
    }
    let mut contact_list: Vec<header::Contact> = Vec::with_capacity(spans.len());
    for span in &spans {
        let entry = match read_contact_entry(span) {
            ContactEntry::Readable(contact) if wire => {
                match uri_repeat_refusal(contact.uri(), &HeaderName::Contact, &limits.exemptions) {
                    Ok(()) => ContactEntry::Readable(contact),
                    Err(e) => ContactEntry::Unreadable { reason: e.reason },
                }
            }
            entry => entry,
        };
        match entry {
            ContactEntry::Wildcard => {}
            ContactEntry::Readable(contact) => contact_list.push(*contact),
            // A lenient set drops the entry on wire and stored bytes alike, so
            // both readers see one set.
            ContactEntry::Unreadable { .. } if !strict => {}
            // The grammar holds on wire bytes only.
            ContactEntry::Unreadable { .. } if !wire => {
                contact_list.push(read_contact_entry_lenient(span));
            }
            ContactEntry::Unreadable { reason } => {
                return Err(SipParseError::new(format!(
                    "Strict Contact: {reason} ({:?})",
                    span.as_str()
                )));
            }
        }
    }
    // The list lives as long as the message: no room for a dropped entry.
    if contact_list.len() < contact_list.capacity() {
        contact_list.shrink_to_fit();
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
    // The Request-URI is read in every mode, ahead of what the request states
    // of itself: a URI that does not read is no request's target, whoever
    // built the request.
    let request_uri = read_request_uri(request_uri)?;
    if let Some(param) = request_uri.repeated_params().next().filter(|_| wire) {
        return Err(SipParseError::new(format!(
            "Malformed Request-URI: URI parameter {param:?} written twice (RFC 3261 §19.1.1)"
        )));
    }

    // §8.1.1: a request states its originator's tag and its hop count.
    if wire && common.from().tag().is_none() {
        return Err(SipParseError::new("Missing From tag (RFC 3261 §8.1.1.3)"));
    }
    if wire
        && idx.max_forwards.value.is_none()
        && !limits.exemptions.holds(&Exemption::MaxForwardsAbsent)
    {
        return Err(SipParseError::new("Missing mandatory Max-Forwards header (RFC 3261 §8.1.1)"));
    }
    if let Some(refer_to) = idx.refer_to.value.filter(|_| wire) {
        if let Ok(target) = header::ReferTo::parse(refer_to) {
            uri_repeat_refusal(target.uri(), &HeaderName::ReferTo, &limits.exemptions)?;
        }
    }

    // Contact cardinality on dialog-creating requests. A new request's missing
    // Contact is refused where its dialog would be established, not here: one
    // the element rejects first (a spent Max-Forwards, no route) needs none.
    if wire {
        if let Some(method) = method.filter(|m| can_establish_dialog(m)) {
            if common.contacts().is_empty() && common.to().tag().is_some() {
                return Err(SipParseError::new(format!(
                    "{method} has no Contact (RFC 3261 §8.1.1.8)"
                )));
            }
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
        if let Some(cseq_val) = idx.cseq.value {
            let trimmed = crate::whitespace::trim(cseq_val);
            let cseq_method = match trimmed.split_once([' ', '\t']) {
                Some((_, method)) => crate::whitespace::trim(method),
                None => "",
            };
            if !cseq_method.is_empty()
                && Method::from_wire(cseq_method) != Method::from_wire(method)
            {
                return Err(SipParseError::new(format!(
                    "CSeq method {cseq_method:?} does not match request method {method:?}"
                )));
            }
        }
    }

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
    if limits.value_gates && status > 100 && common.to().tag().is_none() {
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
        // §12.1.1, RFC 6665 §4.1.2.1, RFC 3515 §2.4.4: a 2xx that creates a
        // dialog names its remote target; one to a NOTIFY creates none.
        if (200..300).contains(&status)
            && matches!(cseq_method.as_str(), "INVITE" | "SUBSCRIBE" | "REFER")
            && common.contacts().is_empty()
        {
            return Err(SipParseError::new(format!(
                "A {status} response to {cseq_method} has no Contact (RFC 3261 §12.1.1)"
            )));
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

#[cfg(test)]
#[path = "extract_fields_tests.rs"]
mod tests;
