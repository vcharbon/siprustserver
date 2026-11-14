//! What a datagram's body carries, read through the `sip-message` seam: every
//! part's media type, the byte span of its content and of each of its entity
//! header values within the datagram — one part for a single body, each MIME
//! part of a multipart one. The anonymiser understands some media types
//! ([`understood`]), and those the deployment states ([`BodyTypes`]); it
//! redacts every other part whole. From an understood part harvest reads its
//! entity headers as a message's, an SDP part's free-form fields and its
//! attributes the media stack does not define, a sipfrag's header fields,
//! the data of a control document and the digit runs of a digit-run body.

use std::ops::Range;

use sip_message::header::MediaType;
use sip_message::multipart::{decompose, outside_parts};
use sip_message::sdp_text::{attributes, text_fields};
use sip_message::{sipfrag, sniff, SipHeader, SipMessage, SipStr};

use super::body_types::{BodyReading, BodyTypes};
use super::Harvested;
use crate::rewrite::fields::MEDIA_ATTRIBUTES;

/// One body part: its media type (`type/subtype`, lowercase, parameters
/// left out), where its content sits in the datagram, and every entity
/// header of a MIME part with where its value sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyPart {
    pub media_type: String,
    pub start: usize,
    pub end: usize,
    pub headers: Vec<(String, Range<usize>)>,
}

/// The control documents an RFC defines, whose data harvest reads
/// ([`crate::xml`]): MSCML (RFC 5022) and emergency call control (RFC 8876).
const CONTROL: &[&str] =
    &["application/mediaservercontrol+xml", "application/emergencycalldata.control+xml"];

/// The media types every SIP stack carries, understood whatever the network:
/// SDP (RFC 4566), a sipfrag (RFC 3420), and DTMF over INFO —
/// `application/dtmf-relay` and `application/dtmf`, which no RFC registers
/// but SIP stacks send alike (`Signal=`, `Duration=`): a key and a duration,
/// never free text, so no profile has to state them.
const BUILT_IN: &[&str] =
    &["application/sdp", "message/sipfrag", "application/dtmf-relay", "application/dtmf"];

/// Whether the anonymiser reads `media_type` (`type/subtype`, lowercase)
/// itself, so no profile states it.
pub fn reads_itself(media_type: &str) -> bool {
    BUILT_IN.contains(&media_type) || CONTROL.contains(&media_type)
}

/// How a part of `media_type` (`type/subtype`, lowercase) is read beyond the
/// media types every SIP stack knows: a control document of [`CONTROL`], or
/// a type the deployment states.
fn reading(media_type: &str, types: &BodyTypes) -> Option<BodyReading> {
    if CONTROL.contains(&media_type) {
        return Some(BodyReading::Control);
    }
    types.reading(media_type)
}

/// Whether a part's media type (`type/subtype`, lowercase) is a control
/// document's, whose protocol attributes hold protocol text.
pub fn is_control(media_type: &str, types: &BodyTypes) -> bool {
    reading(media_type, types) == Some(BodyReading::Control)
}

/// Whether the anonymiser understands a body part: a type of [`BUILT_IN`], a
/// control document that reads as an XML document
/// ([`crate::xml::is_document`]), and a digit-run body written as text — a
/// binary one is encoded data it does not read.
pub fn understood(part: &BodyPart, raw: &[u8], types: &BodyTypes) -> bool {
    let content = &raw[part.start..part.end];
    let text =
        || content.iter().all(|b| (0x20..0x7f).contains(b) || matches!(b, b'\r' | b'\n' | b'\t'));
    if BUILT_IN.contains(&part.media_type.as_str()) {
        return true;
    }
    match reading(&part.media_type, types) {
        Some(BodyReading::Control) => control_document(content),
        Some(BodyReading::DigitRuns) => text(),
        None => false,
    }
}

/// Whether a control part's content reads as an XML document, whose every
/// byte is markup or data the reader returns.
pub fn control_document(content: &[u8]) -> bool {
    std::str::from_utf8(content).is_ok_and(crate::xml::is_document)
}

/// `type/subtype` of a media type, lowercase.
fn media_of(content_type: &str) -> String {
    content_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase()
}

/// Every body part of a parsed datagram, `raw` being its bytes.
pub(super) fn parts_of(msg: &SipMessage, raw: &[u8]) -> Vec<BodyPart> {
    let Some(body) = sniff::body(raw).filter(|b| !b.is_empty()) else { return Vec::new() };
    let base = raw.len() - body.len();
    let whole = |media_type: String| BodyPart {
        media_type,
        start: base,
        end: raw.len(),
        headers: Vec::new(),
    };
    let Some(media) = msg.header::<MediaType>().and_then(Result::ok) else {
        return vec![whole(String::new())];
    };
    match boundary_of(&media) {
        Some(boundary) => decompose(body, &boundary)
            .into_iter()
            .map(|p| BodyPart {
                media_type: media_of(&p.content_type),
                start: base + p.offset,
                end: base + p.offset + p.len,
                headers: p
                    .entity_spans
                    .into_iter()
                    .map(|(name, span)| (name, base + span.start..base + span.end))
                    .collect(),
            })
            .collect(),
        None => vec![whole(media_of(media.token()))],
    }
}

/// The boundary of a multipart media type.
fn boundary_of(media: &MediaType) -> Option<String> {
    media
        .is_multipart()
        .then(|| media.param("boundary").and_then(|b| b.as_str().map(str::to_string)))
        .flatten()
}

/// The byte ranges of a parsed datagram's multipart body, `raw` being its
/// bytes, that no part and no delimiter line covers: the preamble, the
/// epilogue, the whole body when its boundary frames none of it
/// ([`sip_message::multipart::outside_parts`]). Nothing for a single body.
pub(super) fn outside_parts_of(msg: &SipMessage, raw: &[u8]) -> Vec<Range<usize>> {
    let Some(body) = sniff::body(raw).filter(|b| !b.is_empty()) else { return Vec::new() };
    let base = raw.len() - body.len();
    let boundary = msg.header::<MediaType>().and_then(Result::ok).and_then(|m| boundary_of(&m));
    boundary.map_or_else(Vec::new, |boundary| {
        outside_parts(body, &boundary).into_iter().map(|r| base + r.start..base + r.end).collect()
    })
}

/// Harvest the names and numbers the understood parts of `msg` carry.
pub(super) fn harvest(msg: &SipMessage, raw: &[u8], types: &BodyTypes, out: &mut Harvested) {
    for part in parts_of(msg, raw).into_iter().filter(|p| understood(p, raw, types)) {
        let headers: Vec<SipHeader> = part
            .headers
            .iter()
            .map(|(name, span)| SipHeader {
                name: SipStr::owned(name),
                value: SipStr::owned(&String::from_utf8_lossy(&raw[span.clone()])),
            })
            .collect();
        super::harvest_headers(&headers, out);
        let bytes = &raw[part.start..part.end];
        match part.media_type.as_str() {
            "application/sdp" => sdp(bytes, out),
            // A status line's reason phrase is protocol text, never read.
            "message/sipfrag" => super::harvest_headers(&sipfrag::header_fields(bytes), out),
            media => match reading(media, types) {
                Some(BodyReading::DigitRuns) => out.numbers.extend(digit_runs(bytes)),
                Some(BodyReading::Control) => control(bytes, out),
                None => {}
            },
        }
    }
}

/// The free-form fields of an SDP part — the origin username, the session
/// name and information as text, the URI's host, an email's local part and
/// domain, a phone field as text — and the value of every attribute the
/// media stack does not define ([`MEDIA_ATTRIBUTES`]).
fn sdp(bytes: &[u8], out: &mut Harvested) {
    for field in text_fields(bytes) {
        let value = field.value.trim();
        if value.is_empty() || value == "-" {
            continue;
        }
        match field.key {
            'u' => {
                out.names
                    .hosts
                    .extend(sip_message::header::network_host(value).map(str::to_string));
                out.names.texts.push(value.to_string());
            }
            'e' => {
                let address =
                    value.split([' ', '(', '<', '>']).find(|w| w.contains('@')).unwrap_or(value);
                if let Some((local, domain)) = address.rsplit_once('@') {
                    out.names.texts.push(local.to_string());
                    out.names.hosts.push(domain.to_string());
                }
                out.names.texts.push(value.to_string());
            }
            _ => out.names.texts.push(value.to_string()),
        }
    }
    for attr in attributes(bytes) {
        let media = MEDIA_ATTRIBUTES.contains(&attr.name.to_ascii_lowercase().as_str());
        if let Some(value) = attr.value.filter(|_| !media) {
            datum(value.trim(), None, out);
        }
    }
}

/// The data of a control document ([`crate::xml`]); a control-protocol
/// attribute's value is protocol text, never read.
fn control(bytes: &[u8], out: &mut Harvested) {
    for datum_found in crate::xml::data(&String::from_utf8_lossy(bytes)) {
        if datum_found.is_protocol() {
            continue;
        }
        let delimited = datum_found.delimited();
        datum(datum_found.value.trim(), Some(&delimited), out);
    }
}

/// One datum of a body: a URI's hosts and numbers, a message id's domain and
/// local part, or free text — faked as written between its delimiters when
/// it has them, so the rewrite touches only that datum.
fn datum(value: &str, delimited: Option<&str>, out: &mut Harvested) {
    // A URN names a protocol thing (an XML namespace, a service).
    let urn = value.get(..4).is_some_and(|v| v.eq_ignore_ascii_case("urn:"));
    if value.is_empty() || urn || crate::fake::names::vocabulary(value) {
        return;
    }
    let uri = value.contains("://")
        || ["sip:", "sips:", "tel:", "cid:", "mid:"]
            .iter()
            .any(|s| value.get(..s.len()).is_some_and(|v| v.eq_ignore_ascii_case(s)));
    if uri {
        super::scan_uris(&SipStr::owned(value), out);
        super::names::network_hosts(value, &mut out.names);
        super::names::message_ids(value, &mut out.names);
        return;
    }
    if let Some((local, domain)) = value.rsplit_once('@').filter(|(_, d)| d.contains('.')) {
        out.names.hosts.push(domain.to_string());
        if local.bytes().any(|b| b.is_ascii_alphabetic()) {
            out.names.texts.push(local.to_string());
        }
        return;
    }
    if value.bytes().any(|b| b.is_ascii_alphabetic() || b >= 0x80) {
        out.names.texts.push(delimited.unwrap_or(value).to_string());
    }
}

/// Every run of four digits or more in `bytes`.
fn digit_runs(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| !b.is_ascii_digit())
        .filter(|run| run.len() >= 4)
        .map(|run| String::from_utf8_lossy(run).into_owned())
        .collect()
}
