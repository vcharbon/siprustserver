//! Multipart body framing, both directions (RFC 2046 §5.1): [`compose`] frames
//! entity parts into one body plus the `Content-Type` that describes it, and
//! [`decompose`] walks a framed body's boundaries back into located parts. One
//! module owns the framing rules, so the two can never disagree.
//!
//! Two properties the callers depend on:
//!
//! - **A part's payload rides byte-exact.** The framing adds the delimiter's
//!   own CRLF and never a byte of the content, so a decomposed part recomposes
//!   to the same bytes.
//! - **The boundary is derived from the parts.** A body composed twice from the
//!   same parts frames identically, so a replay is reproducible; a delimiter
//!   that would appear inside a payload is extended until it cannot.
//! - **A part header line is written whole or not at all.** Its name is a
//!   `token` and its value holds no CR, LF or NUL (RFC 2045 §3), or the line
//!   is refused, never framed.

use serde::{Deserialize, Serialize};

use crate::header::{HeaderId, HeaderValue, MediaType};
use crate::header_table::HeaderTable;
use crate::sip_str::SipStr;
use crate::write_check::WriteRefusal;

/// One entity part: its media type, any further entity headers it states, and
/// its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultipartPart {
    /// The part's `Content-Type`, parameters included.
    pub content_type: String,
    /// Further entity headers, `(name, value)`, in the order they are emitted —
    /// `Content-ID`, `Content-Disposition`, `Content-Transfer-Encoding`.
    pub headers: Vec<(String, String)>,
    /// The part's content, exactly as it goes on the wire; base64 in a
    /// serialized form.
    #[serde(with = "payload_base64")]
    pub payload: Vec<u8>,
}

/// A part's bytes as standard base64 text in a serialized form.
mod payload_base64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD.decode(text).map_err(serde::de::Error::custom)
    }
}

impl MultipartPart {
    /// A part of `content_type` carrying `payload` and no further headers.
    pub fn new(content_type: impl Into<String>, payload: Vec<u8>) -> Self {
        MultipartPart { content_type: content_type.into(), headers: Vec::new(), payload }
    }

    /// The same part with one more entity header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// A composed multipart body: the bytes, and the `Content-Type` value that
/// frames them (the container type with the boundary this composition used).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composed {
    pub content_type: String,
    pub body: Vec<u8>,
}

/// Why a multipart body could not be composed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultipartError {
    /// The container type is not a `multipart/…` type.
    NotMultipart { content_type: String },
    /// A multipart body with no parts frames nothing (RFC 2046 §5.1.1 requires
    /// at least one encapsulation).
    NoParts,
    /// The caller stated a boundary that occurs inside a part's payload, so the
    /// delimiter would split a part rather than separate two.
    BoundaryInPayload { boundary: String },
    /// A part's `Content-Type` or header line cannot be written whole.
    Unwritable(WriteRefusal),
}

impl std::fmt::Display for MultipartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MultipartError::NotMultipart { content_type } => {
                write!(f, "{content_type:?} is not a multipart media type")
            }
            MultipartError::NoParts => write!(f, "a multipart body states no parts"),
            MultipartError::BoundaryInPayload { boundary } => {
                write!(f, "the stated boundary {boundary:?} occurs inside a part's payload")
            }
            MultipartError::Unwritable(refusal) => write!(f, "a part line: {refusal}"),
        }
    }
}

impl std::error::Error for MultipartError {}

/// Frame `parts` under `container_type` (`multipart/mixed`, `multipart/related`,
/// …).
///
/// A container type that already names a `boundary` uses it, and is refused
/// when a payload contains it; otherwise the boundary is derived from the parts
/// and extended until no payload holds it.
pub fn compose(container_type: &str, parts: &[MultipartPart]) -> Result<Composed, MultipartError> {
    let media = MediaType::parse(&SipStr::owned(container_type)).ok();
    if !media.as_ref().is_some_and(MediaType::is_multipart) {
        return Err(MultipartError::NotMultipart { content_type: container_type.to_string() });
    }
    if parts.is_empty() {
        return Err(MultipartError::NoParts);
    }
    if let Some(refusal) = parts.iter().find_map(part_refusal) {
        return Err(MultipartError::Unwritable(refusal));
    }
    let stated = media
        .as_ref()
        .and_then(|m| m.param("boundary"))
        .and_then(|v| v.as_str().map(str::to_string));
    let boundary = match &stated {
        Some(boundary) => {
            if parts.iter().any(|p| contains(&p.payload, boundary.as_bytes())) {
                return Err(MultipartError::BoundaryInPayload { boundary: boundary.clone() });
            }
            boundary.clone()
        }
        None => derive_boundary(parts),
    };
    let body = frame(&boundary, parts);
    let content_type = match stated {
        Some(_) => container_type.to_string(),
        None => format!("{container_type};boundary={boundary}"),
    };
    Ok(Composed { content_type, body })
}

/// The body of `parts` framed under `boundary` (RFC 2046 §5.1.1): a delimiter
/// line per part, its `Content-Type` and further entity headers, a blank line,
/// the content, and the closing delimiter.
fn frame(boundary: &str, parts: &[MultipartPart]) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    for part in parts {
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(b"Content-Type: ");
        body.extend_from_slice(part.content_type.as_bytes());
        body.extend_from_slice(b"\r\n");
        for (name, value) in &part.headers {
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(b": ");
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&part.payload);
        // The CRLF before the next delimiter belongs to the delimiter, never to
        // the content: a payload ending in 0x0a survives byte-exact.
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--\r\n");
    body
}

/// The description a message body typed `content_type` and described by
/// `entity_headers` holds, as [`sdp_range`] reads it.
fn find_description(
    content_type: Option<&str>,
    entity_headers: &[(String, String)],
    body: &[u8],
) -> Option<description::Found> {
    let media = MediaType::parse(&SipStr::owned(content_type?)).ok()?;
    let disposition = entity_headers
        .iter()
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("Content-Disposition"))
        .map(|(_, value)| value.as_str());
    description::find(&media, disposition, body, 1)
}

/// Which of the [`entity_parts`] of a message body holds the session
/// description [`sdp_range`] locates, nested or not: the part [`attach`]
/// lifts. `None` where the body holds none.
pub fn description_entity(
    content_type: Option<&str>,
    entity_headers: &[(String, String)],
    body: &[u8],
) -> Option<usize> {
    find_description(content_type, entity_headers, body).map(|found| found.index)
}

/// The entity parts a message body carries (RFC 2045 §2.4): each part of a
/// `multipart/…` body with its entity headers in wire order; any other
/// non-empty body is one part typed `content_type`, its headers the message's
/// `entity_headers` that describe it (every `Content-` field but
/// `Content-Type` and `Content-Length`). No body, or no type, carries none.
pub fn entity_parts(
    content_type: Option<&str>,
    entity_headers: &[(String, String)],
    body: &[u8],
) -> Vec<MultipartPart> {
    let Some(text) = content_type.filter(|_| !body.is_empty()) else {
        return Vec::new();
    };
    let media = MediaType::parse(&SipStr::owned(text)).ok();
    let framing = media.as_ref().filter(|m| m.is_multipart()).and_then(|m| {
        let boundary = m.param("boundary")?.as_str()?.to_string();
        Some((boundary, default_part_type(m)))
    });
    if let Some((boundary, default)) = framing {
        return located(body, &boundary, default)
            .into_iter()
            .filter_map(|part| {
                Some(MultipartPart {
                    payload: body.get(part.offset..part.offset + part.len)?.to_vec(),
                    content_type: part.content_type,
                    headers: part.entity_headers,
                })
            })
            .collect();
    }
    let headers = describing_entity_headers(entity_headers);
    vec![MultipartPart { content_type: text.trim().to_string(), headers, payload: body.to_vec() }]
}

/// The message's entity headers that describe a bare body as one part: every
/// `Content-` field but `Content-Type` and `Content-Length`.
fn describing_entity_headers(entity_headers: &[(String, String)]) -> Vec<(String, String)> {
    entity_headers
        .iter()
        .filter(|(name, _)| {
            let name = name.trim();
            is_entity_header(name)
                && !name.eq_ignore_ascii_case("Content-Type")
                && !name.eq_ignore_ascii_case("Content-Length")
                && !name.eq_ignore_ascii_case("MIME-Version")
        })
        .cloned()
        .collect()
}

/// Whether `name` is a MIME entity header (RFC 2045 §9: `MIME-Version` and every
/// `Content-` field) — a statement about the body the message carries, which
/// rides only with that body.
pub fn is_entity_header(name: &str) -> bool {
    const CONTENT: &[u8] = b"content-";
    let bytes = name.trim().as_bytes();
    name.trim().eq_ignore_ascii_case("MIME-Version")
        || (bytes.len() > CONTENT.len()
            && bytes.get(..CONTENT.len()).is_some_and(|head| head.eq_ignore_ascii_case(CONTENT)))
}

/// Whether `boundary` has RFC 2046 §5.1.1's form: 1 to 70 `bchars`, the last
/// no space.
pub(crate) fn is_boundary(boundary: &str) -> bool {
    let bcharsnospace = |b: u8| b.is_ascii_alphanumeric() || b"'()+_,-./:=?".contains(&b);
    let bytes = boundary.as_bytes();
    (1..=70).contains(&bytes.len())
        && bytes.iter().all(|&b| b == b' ' || bcharsnospace(b))
        && bytes.last().is_some_and(|&b| bcharsnospace(b))
}

/// A boundary no payload contains, derived from the parts so the same parts
/// always frame the same way. RFC 2046 §5.1.1's `bchars` set is respected: the
/// derivation emits hex digits and `-` only.
fn derive_boundary(parts: &[MultipartPart]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.content_type.as_bytes().iter().chain(part.payload.iter()) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    let mut boundary = format!("boundary-{hash:016x}");
    while parts.iter().any(|p| contains(&p.payload, boundary.as_bytes())) {
        boundary.push('-');
    }
    boundary
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

/// One part LOCATED in a framed body: its entity headers read, its content
/// addressed as `offset`/`len` into the body itself — never copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedPart {
    /// The part's `Content-Type`, parameters included — `text/plain` where the
    /// block states none (RFC 2045 §5.2), `message/rfc822` in a
    /// `multipart/digest` (RFC 2046 §5.1.5).
    pub content_type: String,
    /// The part's `Content-ID`, where it states one.
    pub content_id: Option<String>,
    /// Every further entity header, `(name, value)`, in wire order and with the
    /// part's own spelling — what a byte-exact replay has to put back
    /// (RFC 2045 §3).
    pub headers: Vec<(String, String)>,
    /// Every entity header but `Content-Type`, `Content-ID` included, in wire
    /// order and with the part's own spelling.
    pub entity_headers: Vec<(String, String)>,
    /// Every entity header, `Content-Type` included, as its name and the range
    /// of its value in the body [`decompose`] walked, folds included, in wire
    /// order.
    pub entity_spans: Vec<(String, std::ops::Range<usize>)>,
    /// Where the part's content starts in the body [`decompose`] walked.
    pub offset: usize,
    /// The content's length in bytes.
    pub len: usize,
}

/// Locate each MIME part of `body` framed under `boundary` (RFC 2046 §5.1):
/// the delimiter lines found, the preamble and the epilogue dropped, then each
/// part's head read and the offsets of its content. A part stating no type is
/// `text/plain`, a `multipart/digest`'s aside ([`entity_parts`] reads those).
pub fn decompose(body: &[u8], boundary: &str) -> Vec<LocatedPart> {
    located(body, boundary, "text/plain")
}

/// The type of a part of `container` that states none (RFC 2045 §5.2, RFC
/// 2046 §5.1.5).
fn default_part_type(container: &MediaType) -> &'static str {
    if container.subtype().eq_ignore_ascii_case("digest") {
        "message/rfc822"
    } else {
        "text/plain"
    }
}

/// [`decompose`], a part stating no type typed `default`.
fn located(body: &[u8], boundary: &str, default: &str) -> Vec<LocatedPart> {
    framed_parts(body, boundary)
        .into_iter()
        .map(|part| LocatedPart {
            content_type: part.head.get("content-type").unwrap_or(default).to_string(),
            content_id: part.head.get("content-id").map(str::to_string),
            headers: part.head.beyond(&["content-type", "content-id"]),
            entity_headers: part.head.beyond(&["content-type"]),
            entity_spans: part.head.spans(),
            offset: part.offset,
            len: part.len,
        })
        .collect()
}

/// The session description a body typed `content_type` carries, as the range
/// of `body` holding it, `None` where it holds none:
///
/// - the whole body under `application/sdp`, when `disposition` (the body's
///   `Content-Disposition` value, `None` when none is stated) is absent or
///   `session`, SDP's default (RFC 3261 §20.11, RFC 5621 §8.2);
/// - under `multipart/…`, a part read by the same rule with its own head,
///   nested multiparts searched four levels down, the message's own included:
///   the first found in a `mixed` (or any other) body, the last in an
///   `alternative` one (RFC 2046 §5.1.4, RFC 5621 §6.1). The disposition of a
///   multipart body or part never hides what it frames.
pub fn sdp_range(
    content_type: &MediaType,
    disposition: Option<&str>,
    body: &[u8],
) -> Option<std::ops::Range<usize>> {
    description::find(content_type, disposition, body, 1).map(|found| found.range)
}

/// Where the body of the message `headers` head holds its session
/// description, by its `Content-Type` and `Content-Disposition`
/// ([`sdp_range`]); `None` where the type does not read or names none.
pub(crate) fn message_sdp_range(
    headers: &HeaderTable,
    body: &[u8],
) -> Option<std::ops::Range<usize>> {
    if body.is_empty() {
        return None;
    }
    let line = headers.of_id(HeaderId::ContentType).next()?;
    let content_type = MediaType::parse(&line.value).ok()?;
    let disposition = headers.of_id(HeaderId::ContentDisposition).next();
    sdp_range(&content_type, disposition.map(|d| d.value.as_str()), body)
}

mod attach;
pub use attach::{attach, Attached};

mod description;
mod entity;

mod framing;
use framing::framed_parts;
pub(crate) use framing::frames_cleanly;

mod part_lines;
use part_lines::part_refusal;

#[cfg(test)]
mod tests;
