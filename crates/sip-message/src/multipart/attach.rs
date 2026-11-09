//! A message body rebuilt around its session description (RFC 5621 §3): the
//! description [`sdp_range`](super::sdp_range) locates, lifted with its own
//! head, beside the parts a caller states.

use super::part_lines::writable;
use super::{derive_boundary, describing_entity_headers, find_description, frame, MultipartPart};
use crate::header::{HeaderClass, HeaderName};
use crate::whitespace;
use crate::write_check::{Position, WriteRefusal};

/// A message body built by [`attach`]: the bytes, the `Content-Type` that
/// describes them (`None` for no body), the further entity headers the
/// message itself states for them, `(name, value)` in emission order, and
/// what of the parts was left out because it cannot be written whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub content_type: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub refused: Vec<WriteRefusal>,
}

/// The body carrying the session description of `body` (typed `content_type`,
/// described by the message's `entity_headers`, located as [`sdp_range`](super::sdp_range)
/// locates it) beside `parts`, which replace every other part `body` carries
/// (RFC 5621 §3).
///
/// - a description and parts: `multipart/mixed`, the description first;
/// - no description, one part: that part is the body, its entity headers the
///   message's;
/// - no description, several parts: `multipart/mixed` of them;
/// - a description alone: that description, bare, its entity headers the
///   message's; nothing at all: no body.
///
/// A body carrying a part states `MIME-Version: 1.0` on the message (RFC 2045
/// §4). The description keeps its own entity headers in wire order, a bare
/// one those the message stated for it. A part's own `Content-Type` header
/// types it; its `MIME-Version` and any structural header never ride, the
/// description's included. A part header line that cannot be written whole,
/// or that was not UTF-8 as the description's head wrote it, is left out, and
/// so is a part whose `Content-Type` cannot be written; each is named in
/// [`Attached::refused`].
pub fn attach(
    content_type: Option<&str>,
    entity_headers: &[(String, String)],
    body: &[u8],
    parts: &[MultipartPart],
) -> Attached {
    let mut refused = Vec::new();
    let mut all: Vec<MultipartPart> = Vec::with_capacity(parts.len() + 1);
    all.extend(description_part(content_type, entity_headers, body, &mut refused));
    all.extend(parts.iter().filter_map(|part| writable(normalized(part), &mut refused)));
    let mime = if parts.is_empty() { Vec::new() } else { vec![mime_version()] };
    match all.as_slice() {
        [] => Attached { content_type: None, headers: Vec::new(), body: Vec::new(), refused },
        [one] => Attached {
            content_type: Some(one.content_type.clone()),
            headers: mime.into_iter().chain(one.headers.iter().cloned()).collect(),
            body: one.payload.clone(),
            refused,
        },
        several => {
            let boundary = derive_boundary(several);
            Attached {
                content_type: Some(format!("multipart/mixed;boundary={boundary}")),
                headers: vec![mime_version()],
                body: frame(&boundary, several),
                refused,
            }
        }
    }
}

fn mime_version() -> (String, String) {
    ("MIME-Version".to_string(), "1.0".to_string())
}

/// The session description `body` carries ([`sdp_range`](super::sdp_range)) as one part: a
/// framed one with its own head, a bare one typed `content_type` and described
/// by the message's `entity_headers`. Its `MIME-Version` and structural headers
/// never ride; a head line that is not UTF-8 as written is left out and pushed
/// onto `refused`.
fn description_part(
    content_type: Option<&str>,
    entity_headers: &[(String, String)],
    body: &[u8],
    refused: &mut Vec<WriteRefusal>,
) -> Option<MultipartPart> {
    let text = content_type?;
    let found = find_description(content_type, entity_headers, body)?;
    let payload = body.get(found.range)?.to_vec();
    let Some(head) = found.part else {
        let headers = describing_entity_headers(entity_headers);
        return Some(MultipartPart {
            content_type: whitespace::trim(text).to_string(),
            headers,
            payload,
        });
    };
    let mut part = MultipartPart::new(head.get("content-type").unwrap_or_default(), payload);
    for (name, value, utf8) in head.written() {
        if !utf8 {
            let reason = format!("{name}: not UTF-8 as written");
            refused.push(WriteRefusal { position: Position::HeaderValue, reason });
        } else if !name.eq_ignore_ascii_case("Content-Type") && rides_with_a_part(name) {
            part.headers.push((name.to_string(), value.to_string()));
        }
    }
    Some(part)
}

/// Whether a part's header `name` rides with it: `MIME-Version` (the
/// message's own) and structural headers such as `Content-Length` (RFC 3261
/// §20.14, the message's framing) never do.
fn rides_with_a_part(name: &str) -> bool {
    !name.eq_ignore_ascii_case("MIME-Version")
        && HeaderName::class_of(name) != HeaderClass::Structural
}

/// `part` as it is framed or lifted: a `Content-Type` among its headers types
/// it, and `MIME-Version` (the message's own) and structural headers such as
/// `Content-Length` (RFC 3261 §20.14, the message's framing) are dropped.
fn normalized(part: &MultipartPart) -> MultipartPart {
    let mut out = MultipartPart::new(part.content_type.clone(), part.payload.clone());
    for (name, value) in &part.headers {
        let name_trimmed = whitespace::trim(name);
        if name_trimmed.eq_ignore_ascii_case("Content-Type") {
            out.content_type = value.clone();
        } else if rides_with_a_part(name_trimmed) {
            out.headers.push((name.clone(), value.clone()));
        }
    }
    out
}
