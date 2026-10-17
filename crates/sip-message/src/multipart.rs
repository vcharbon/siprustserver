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

use serde::{Deserialize, Serialize};

use crate::header::{HeaderClass, HeaderName, HeaderValue, MediaType, ParamValue};
use crate::sip_str::SipStr;

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

/// A message body built by [`attach`]: the bytes, the `Content-Type` that
/// describes them (`None` for no body), and the further entity headers the
/// message itself states for them, `(name, value)` in emission order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attached {
    pub content_type: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The body carrying the session description of `body` (typed `content_type`,
/// described by the message's `entity_headers`) beside `parts`, which replace
/// every other part `body` carries (RFC 5621 §3).
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
/// types it; its `MIME-Version` and any structural header never ride.
pub fn attach(
    content_type: Option<&str>,
    entity_headers: &[(String, String)],
    body: &[u8],
    parts: &[MultipartPart],
) -> Attached {
    let mut all: Vec<MultipartPart> = Vec::with_capacity(parts.len() + 1);
    all.extend(
        entity_parts(content_type, entity_headers, body)
            .into_iter()
            .find(|part| !part.payload.is_empty() && is_sdp(&part.content_type)),
    );
    all.extend(parts.iter().map(normalized));
    match all.as_slice() {
        [] => Attached { content_type: None, headers: Vec::new(), body: Vec::new() },
        [one] => Attached {
            content_type: Some(one.content_type.clone()),
            headers: if parts.is_empty() { Vec::new() } else { vec![mime_version()] }
                .into_iter()
                .chain(one.headers.iter().cloned())
                .collect(),
            body: one.payload.clone(),
        },
        several => {
            let boundary = derive_boundary(several);
            Attached {
                content_type: Some(format!("multipart/mixed;boundary={boundary}")),
                headers: vec![mime_version()],
                body: frame(&boundary, several),
            }
        }
    }
}

fn mime_version() -> (String, String) {
    ("MIME-Version".to_string(), "1.0".to_string())
}

fn is_sdp(content_type: &str) -> bool {
    MediaType::parse(&SipStr::owned(content_type)).is_ok_and(|ct| ct.is_sdp())
}

/// `part` as it is framed or lifted: a `Content-Type` among its headers types
/// it, and `MIME-Version` (the message's own) and structural headers such as
/// `Content-Length` (RFC 3261 §20.14, the message's framing) are dropped.
fn normalized(part: &MultipartPart) -> MultipartPart {
    let mut out = MultipartPart::new(part.content_type.clone(), part.payload.clone());
    for (name, value) in &part.headers {
        let name_trimmed = name.trim();
        if name_trimmed.eq_ignore_ascii_case("Content-Type") {
            out.content_type = value.clone();
        } else if !name_trimmed.eq_ignore_ascii_case("MIME-Version")
            && HeaderName::class_of(name_trimmed) != HeaderClass::Structural
        {
            out.headers.push((name.clone(), value.clone()));
        }
    }
    out
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
    let boundary = media
        .as_ref()
        .filter(|m| m.is_multipart())
        .and_then(|m| m.param("boundary"))
        .and_then(|b| b.as_str().map(str::to_string));
    if let Some(boundary) = boundary {
        return decompose(body, &boundary)
            .into_iter()
            .map(|part| MultipartPart {
                payload: body[part.offset..part.offset + part.len].to_vec(),
                content_type: part.content_type,
                headers: part.entity_headers,
            })
            .collect();
    }
    let headers = entity_headers
        .iter()
        .filter(|(name, _)| {
            let name = name.trim();
            is_entity_header(name)
                && !name.eq_ignore_ascii_case("Content-Type")
                && !name.eq_ignore_ascii_case("Content-Length")
                && !name.eq_ignore_ascii_case("MIME-Version")
        })
        .cloned()
        .collect();
    vec![MultipartPart { content_type: text.trim().to_string(), headers, payload: body.to_vec() }]
}

/// Whether `name` is a MIME entity header (RFC 2045 §9: `MIME-Version` and every
/// `Content-` field) — a statement about the body the message carries, which
/// rides only with that body.
pub fn is_entity_header(name: &str) -> bool {
    const CONTENT: &[u8] = b"content-";
    let bytes = name.trim().as_bytes();
    name.trim().eq_ignore_ascii_case("MIME-Version")
        || (bytes.len() > CONTENT.len() && bytes[..CONTENT.len()].eq_ignore_ascii_case(CONTENT))
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
    /// block states none (RFC 2045 §5.2).
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
    /// of its value in the body [`decompose`] walked, in wire order.
    pub entity_spans: Vec<(String, std::ops::Range<usize>)>,
    /// Where the part's content starts in the body [`decompose`] walked.
    pub offset: usize,
    /// The content's length in bytes.
    pub len: usize,
}

/// Locate each MIME part of `body` framed under `boundary` (RFC 2046 §5.1):
/// split on `--boundary`, drop the preamble and the closing delimiter, then
/// read each block's entity headers and the offsets of its content.
pub fn decompose(body: &[u8], boundary: &str) -> Vec<LocatedPart> {
    let delim = format!("--{boundary}");
    let mut out = Vec::new();
    for (start, block) in segments(body, delim.as_bytes()).into_iter().skip(1) {
        // A closing delimiter's remainder begins with `--`.
        if block.starts_with(b"--") {
            continue;
        }
        // The CRLF that ended the boundary line belongs to the delimiter.
        let lead = if block.starts_with(b"\r\n") {
            2
        } else if block.starts_with(b"\n") {
            1
        } else {
            0
        };
        let block = &block[lead..];
        if block.is_empty() {
            continue;
        }
        let (head_len, gap) = match find(block, b"\r\n\r\n") {
            Some(i) => (i, 4),
            None => match find(block, b"\n\n") {
                Some(i) => (i, 2),
                None => (0, 0),
            },
        };
        let content = &block[head_len + gap..];
        // Exactly the ONE trailing CRLF that separates the content from the
        // next delimiter — never greedily, so a binary part ending in 0x0a
        // survives byte-exact.
        let trailing = if content.ends_with(b"\r\n") {
            2
        } else if content.ends_with(b"\n") {
            1
        } else {
            0
        };
        out.push(LocatedPart {
            content_type: part_header(&block[..head_len], "content-type")
                .unwrap_or_else(|| "text/plain".to_string()),
            content_id: part_header(&block[..head_len], "content-id"),
            headers: part_headers_beyond(&block[..head_len], &["content-type", "content-id"]),
            entity_headers: part_headers_beyond(&block[..head_len], &["content-type"]),
            entity_spans: part_header_spans(&block[..head_len], start + lead),
            offset: start + lead + head_len + gap,
            len: content.len() - trailing,
        });
    }
    out
}

/// The session description a body carries under `content_type`, as the range
/// of `body` holding it: the whole body under `application/sdp`, the first
/// `application/sdp` part of a `multipart/…` body (RFC 5621 §3.1 — a body that
/// frames a description carries it as an offer or answer like a bare one),
/// none where the type names neither or the body is empty.
pub fn sdp_range(content_type: &MediaType, body: &[u8]) -> Option<std::ops::Range<usize>> {
    if body.is_empty() {
        return None;
    }
    if content_type.is_sdp() {
        return Some(0..body.len());
    }
    if !content_type.is_multipart() {
        return None;
    }
    let boundary = content_type.param("boundary").and_then(ParamValue::as_str)?;
    decompose(body, boundary)
        .into_iter()
        .find(|part| {
            part.len > 0
                && MediaType::parse(&SipStr::owned(&part.content_type)).is_ok_and(|ct| ct.is_sdp())
        })
        .map(|part| part.offset..part.offset + part.len)
}

/// `(offset, slice)` for every stretch of `hay` between occurrences of `sep`,
/// the separators dropped. The first entry is whatever preceded the first
/// separator, so a caller wanting the parts skips it.
fn segments<'a>(hay: &'a [u8], sep: &[u8]) -> Vec<(usize, &'a [u8])> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(rel) = find(&hay[at..], sep) {
        out.push((at, &hay[at..at + rel]));
        at += rel + sep.len();
    }
    out.push((at, &hay[at..]));
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    memchr::memmem::find(hay, needle)
}

mod entity;

use entity::{part_header, part_header_spans, part_headers_beyond};

#[cfg(test)]
mod tests;
