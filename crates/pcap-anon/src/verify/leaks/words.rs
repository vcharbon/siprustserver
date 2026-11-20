//! The free text of a datagram that names people and organisations, read
//! with the leak check's own eyes: the values of the product, organisation,
//! subject and visited-network headers, the parameter values of
//! `P-Access-Network-Info` behind its access type and the quoted strings of a
//! vendor header — in the message head, a sipfrag body and a MIME part's
//! entity headers alike — the value of every SDP attribute the media stack
//! does not define, the data of a control document, the words of letters of
//! a digit-run part its row's vocabulary does not list, the whole content of
//! a part the anonymiser does not understand, and every byte of a multipart
//! body outside its parts and its delimiter lines; a URI among them is
//! the other classes' to read. Protocol text — a reason phrase, a Reason's or
//! a Warning's text, a control-protocol attribute's value — is no free text:
//! the leak check blanks it before reading (`verify::without_protocol_text`).

use sip_message::header::HeaderName;
use sip_message::sdp_text::attributes;
use sip_message::{sipfrag, SipHeader, SipStr};

use crate::identities::bodies::{is_control, understood};
use crate::identities::body_types::BodyReading;
use crate::identities::BodyTypes;
use crate::reading::Reading;

use crate::rewrite::fields::MEDIA_ATTRIBUTES;

/// Headers whose values are protocol vocabulary, never free text.
const VOCABULARY_HEADERS: &[&str] = &[
    "recv-info",
    "info-package",
    "feature-caps",
    "accept-contact",
    "reject-contact",
    "request-disposition",
    "p-asserted-service",
    "p-preferred-service",
    "session-id",
    "security-client",
    "security-server",
    "security-verify",
    "identity",
];

/// Every piece of free text of `raw`, read under `reading`, the network's
/// body `types` among those understood.
pub(super) fn free_text(raw: &[u8], reading: &Reading, types: &BodyTypes) -> Vec<String> {
    let Ok(msg) = reading.parse(raw) else { return Vec::new() };
    let mut out = Vec::new();
    headers(msg.headers(), &mut out);
    for part in crate::identities::body_parts(raw) {
        let entity: Vec<SipHeader> = part
            .headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("content-type"))
            .map(|(name, span)| SipHeader {
                name: SipStr::owned(name),
                value: SipStr::owned(&String::from_utf8_lossy(&raw[span.clone()])),
            })
            .collect();
        headers(&entity, &mut out);
        let content = &raw[part.start..part.end];
        if understood(&part, raw, types) {
            part_text(&part.media_type, content, types, &mut out);
        } else {
            out.push(String::from_utf8_lossy(content).into_owned());
        }
    }
    let outside = crate::identities::outside_parts(raw);
    out.extend(outside.into_iter().map(|span| String::from_utf8_lossy(&raw[span]).into_owned()));
    out
}

/// The free text of one understood body part of `media` (`type/subtype`,
/// lowercase); a digit-run part's is each run of letters, its digit runs
/// being the number pass's.
fn part_text(media: &str, content: &[u8], types: &BodyTypes, out: &mut Vec<String>) {
    if media == "application/sdp" {
        for attr in attributes(content) {
            if !MEDIA_ATTRIBUTES.contains(&attr.name.to_ascii_lowercase().as_str()) {
                out.extend(attr.value.filter(|v| !is_uri(v)));
            }
        }
    } else if media == "message/sipfrag" {
        headers(&sipfrag::header_fields(content), out);
    } else if is_control(media, types) {
        let data = crate::xml::data(&String::from_utf8_lossy(content));
        let data = data.into_iter().filter(|d| !d.is_protocol()).map(|d| d.value);
        out.extend(data.filter(|v| !is_uri(v.trim())));
    } else if types.reading(media) == Some(BodyReading::DigitRuns) {
        let words = content.split(|b| !b.is_ascii_alphabetic()).filter(|w| !w.is_empty());
        let words = words.map(|w| String::from_utf8_lossy(w).into_owned());
        out.extend(words.filter(|w| !types.lists(media, w)));
    }
}

/// The free text of a header block.
fn headers(block: &[SipHeader], out: &mut Vec<String>) {
    for h in block {
        let value = h.value.as_str();
        match HeaderName::of(&h.name) {
            HeaderName::Server
            | HeaderName::UserAgent
            | HeaderName::Organization
            | HeaderName::Subject => out.push(value.to_string()),
            HeaderName::PAccessNetworkInfo => out.extend(
                value
                    .split(';')
                    .skip(1)
                    .filter_map(|p| p.split_once('=').map(|(_, v)| v.to_string())),
            ),
            HeaderName::Other(name) => {
                let name = name.as_str().to_ascii_lowercase();
                if name == "p-visited-network-id" {
                    out.push(value.to_string());
                } else if !VOCABULARY_HEADERS.contains(&name.as_str()) {
                    let quoted = crate::identities::quoted_strings(value);
                    out.extend(quoted.into_iter().filter(|q| !is_uri(q)).map(str::to_string));
                }
            }
            _ => {}
        }
    }
}

/// Whether a datum is a URI, whose hosts and digits the other classes read.
fn is_uri(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.contains("://")
        || ["sip:", "sips:", "tel:", "cid:", "mid:", "urn:"].iter().any(|s| lower.starts_with(s))
}

/// The words of a piece of text: runs of ASCII letters and digits and
/// non-ASCII bytes.
pub(super) fn of(text: &str) -> Vec<Vec<u8>> {
    text.as_bytes()
        .split(|b| !(b.is_ascii_alphanumeric() || *b >= 0x80))
        .filter(|w| !w.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}
