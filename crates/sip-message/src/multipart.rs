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
    needle.len() <= haystack.len() && haystack.windows(needle.len()).any(|window| window == needle)
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

/// Every entity header of a part's header block except the ones already held in
/// their own field, in wire order and with the part's own spelling.
fn part_headers_beyond(head: &[u8], own_fields: &[&str]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in head.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        let name = String::from_utf8_lossy(&line[..colon]).trim().to_string();
        if name.is_empty() || own_fields.iter().any(|own| name.eq_ignore_ascii_case(own)) {
            continue;
        }
        out.push((name, String::from_utf8_lossy(&line[colon + 1..]).trim().to_string()));
    }
    out
}

/// One MIME entity header of a part's header block, case-insensitively.
fn part_header(head: &[u8], name_lower: &str) -> Option<String> {
    for line in head.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        let name = String::from_utf8_lossy(&line[..colon]);
        if name.trim().eq_ignore_ascii_case(name_lower) {
            return Some(String::from_utf8_lossy(&line[colon + 1..]).trim().to_string());
        }
    }
    None
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
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(raw: &str) -> MediaType {
        MediaType::parse(&SipStr::owned(raw)).unwrap()
    }

    /// RFC 5621 §3.1: a description framed inside a multipart body is the
    /// message's offer or answer, wherever the part sits; a body typed
    /// `application/sdp` is the description whole.
    #[test]
    fn the_sdp_range_reads_a_bare_body_and_a_framed_part_alike() {
        let bare = sdp().payload;
        assert_eq!(sdp_range(&media("application/sdp"), &bare), Some(0..bare.len()));
        assert_eq!(sdp_range(&media("application/sdp"), b""), None);

        let composed = compose("multipart/mixed", &[indata(), sdp()]).unwrap();
        let range = sdp_range(&media(&composed.content_type), &composed.body)
            .expect("the framed description is located");
        assert_eq!(&composed.body[range], sdp().payload.as_slice());

        let none = compose("multipart/mixed", &[indata()]).unwrap();
        assert_eq!(sdp_range(&media(&none.content_type), &none.body), None);
        let empty = compose(
            "multipart/mixed",
            &[MultipartPart::new("application/sdp", Vec::new()), indata()],
        )
        .unwrap();
        assert_eq!(sdp_range(&media(&empty.content_type), &empty.body), None, "an empty part");
        assert_eq!(sdp_range(&media("multipart/mixed"), &composed.body), None, "no boundary");
        assert_eq!(sdp_range(&media("text/plain"), b"v=0\r\n"), None);
    }

    fn sdp() -> MultipartPart {
        MultipartPart::new("application/sdp", b"v=0\r\nc=IN IP4 1.2.3.4\r\n".to_vec())
    }

    fn indata() -> MultipartPart {
        MultipartPart::new("application/vnd.example.indata", vec![0x77, 0x15, 0x47, 0x00, 0x0a])
            .with_header("Content-ID", "<indata@example.invalid>")
            .with_header("Content-Transfer-Encoding", "binary")
            .with_header("Content-Disposition", "signal;handling=optional")
    }

    /// The framing a capture shows (RFC 2046 §5.1): a delimiter line per part,
    /// the part's entity headers, a blank line, the content, and a closing
    /// delimiter.
    #[test]
    fn parts_frame_into_the_shape_a_capture_carries() {
        let composed = compose("multipart/mixed", &[sdp(), indata()]).unwrap();
        let boundary = composed
            .content_type
            .split("boundary=")
            .nth(1)
            .expect("the composed type names its boundary")
            .to_string();
        let text = String::from_utf8_lossy(&composed.body).into_owned();
        assert!(text.starts_with(&format!("--{boundary}\r\nContent-Type: application/sdp\r\n\r\n")));
        // The entity headers ride in the order the caller stated them, under
        // the part's own Content-Type (RFC 2045 §3).
        assert!(text.contains(
            "Content-Type: application/vnd.example.indata\r\n\
             Content-ID: <indata@example.invalid>\r\n\
             Content-Transfer-Encoding: binary\r\n\
             Content-Disposition: signal;handling=optional\r\n\r\n"
        ));
        assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    }

    /// The property the replay depends on: a payload goes out byte for byte,
    /// trailing newline included, because the delimiter brings its own CRLF.
    #[test]
    fn a_part_payload_rides_byte_exact_including_a_trailing_newline() {
        let composed = compose("multipart/mixed", &[sdp(), indata()]).unwrap();
        let boundary = composed.content_type.split("boundary=").nth(1).unwrap().as_bytes().to_vec();
        // Split the way an extractor does and read each part's content back.
        let delim = [b"--".as_slice(), &boundary].concat();
        let text = composed.body.clone();
        let mut contents: Vec<Vec<u8>> = Vec::new();
        let mut at = 0usize;
        while let Some(i) = find(&text[at..], &delim) {
            let block_start = at + i + delim.len();
            if text[block_start..].starts_with(b"--") {
                break;
            }
            let rest = &text[block_start + 2..];
            let head = find(rest, b"\r\n\r\n").expect("part headers end");
            let content_start = block_start + 2 + head + 4;
            let next = find(&text[content_start..], &delim).expect("next delimiter");
            contents.push(text[content_start..content_start + next - 2].to_vec());
            at = content_start + next;
        }
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0], sdp().payload, "the SDP part is byte-exact");
        assert_eq!(contents[1], indata().payload, "the binary part is byte-exact");
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Same parts, same framing: a replayed body does not churn between runs.
    #[test]
    fn the_derived_boundary_is_a_function_of_the_parts() {
        let once = compose("multipart/mixed", &[sdp(), indata()]).unwrap();
        let twice = compose("multipart/mixed", &[sdp(), indata()]).unwrap();
        assert_eq!(once, twice);
        let other = compose("multipart/mixed", &[sdp()]).unwrap();
        assert_ne!(once.content_type, other.content_type);
    }

    /// A delimiter that would appear inside a payload does not separate parts,
    /// it splits one — so the derivation walks past it.
    #[test]
    fn a_derived_boundary_never_occurs_inside_a_payload() {
        let seed = compose("multipart/mixed", &[sdp()]).unwrap();
        let boundary = seed.content_type.split("boundary=").nth(1).unwrap().to_string();
        let hostile = MultipartPart::new(
            "application/sdp",
            format!("v=0\r\na=x:{boundary}\r\n").into_bytes(),
        );
        let composed = compose("multipart/mixed", std::slice::from_ref(&hostile)).unwrap();
        let used = composed.content_type.split("boundary=").nth(1).unwrap();
        assert!(!contains(&hostile.payload, used.as_bytes()));
    }

    /// A caller that states its own boundary keeps it — and is refused when a
    /// payload holds it, rather than framing a body nothing can split.
    #[test]
    fn a_stated_boundary_is_honoured_and_a_colliding_one_is_refused() {
        let composed = compose("multipart/mixed;boundary=unique-boundary-1", &[sdp()]).unwrap();
        assert_eq!(composed.content_type, "multipart/mixed;boundary=unique-boundary-1");
        assert!(composed.body.starts_with(b"--unique-boundary-1\r\n"));
        let hostile = MultipartPart::new("text/plain", b"x unique-boundary-1 x".to_vec());
        assert_eq!(
            compose("multipart/mixed;boundary=unique-boundary-1", &[hostile]),
            Err(MultipartError::BoundaryInPayload { boundary: "unique-boundary-1".into() })
        );
    }

    /// The BINARY-IDENTITY proof for MIME framing: what [`decompose`] locates,
    /// [`compose`] puts back. A captured body in the corpus's shape — an offer,
    /// a location body whose content type states a `charset` and whose block
    /// carries a `Content-ID`, and a binary payload ending in 0x0a — decomposes
    /// and recomposes to the SAME BYTES. The container's boundary is the ONE
    /// substitution (a replay regenerates it); the equality then leaves nothing
    /// else free to move.
    #[test]
    fn a_captured_multipart_body_recomposes_byte_identically_bar_its_boundary() {
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(b"--unique-boundary-1\r\nContent-Type: application/sdp\r\n\r\n");
        body.extend_from_slice(b"v=0\r\nc=IN IP4 10.0.0.1\r\nm=audio 16804 RTP/AVP 8");
        body.extend_from_slice(
            b"\r\n--unique-boundary-1\r\n\
              Content-Type: application/pidf+xml;charset=utf-8\r\n\
              Content-ID: <geoloc@example.invalid>\r\n\
              Content-Disposition: render;handling=optional\r\n\r\n",
        );
        body.extend_from_slice(b"<presence/>");
        body.extend_from_slice(
            b"\r\n--unique-boundary-1\r\n\
              Content-Type: application/vnd.example.indata\r\n\
              Content-Transfer-Encoding: binary\r\n\
              Content-Disposition: signal;handling=optional\r\n\r\n",
        );
        body.extend_from_slice(&[0x77, 0x15, 0x47, 0x00, 0x83, 0x11, 0x89, 0x07, 0x0a]);
        body.extend_from_slice(b"\r\n--unique-boundary-1--\r\n");

        let located = decompose(&body, "unique-boundary-1");
        assert_eq!(located.len(), 3);
        // Recompose exactly as a replay does: the located content, the part's
        // own content type with its parameters, its id, then its other entity
        // headers in the order the walk read them.
        let parts: Vec<MultipartPart> = located
            .iter()
            .map(|p| {
                let payload = body[p.offset..p.offset + p.len].to_vec();
                let mut part = MultipartPart::new(p.content_type.clone(), payload);
                if let Some(id) = &p.content_id {
                    part = part.with_header("Content-ID", id.clone());
                }
                for (name, value) in &p.headers {
                    part = part.with_header(name.clone(), value.clone());
                }
                part
            })
            .collect();
        let composed = compose("multipart/mixed", &parts).expect("the parts frame");
        let derived = composed
            .content_type
            .strip_prefix("multipart/mixed;boundary=")
            .expect("the container states its own type and the boundary it derived");

        let recomposed = replace_bytes(&composed.body, derived.as_bytes(), b"unique-boundary-1");
        assert_eq!(
            String::from_utf8_lossy(&recomposed),
            String::from_utf8_lossy(&body),
            "the recomposed body differs from the captured one beyond its boundary"
        );
        assert_eq!(recomposed, body, "the difference is not a UTF-8 artefact");
    }

    /// `haystack` with every occurrence of `from` replaced by `to`.
    fn replace_bytes(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(haystack.len());
        let mut at = 0usize;
        while at < haystack.len() {
            match find(&haystack[at..], from) {
                Some(i) => {
                    out.extend_from_slice(&haystack[at..at + i]);
                    out.extend_from_slice(to);
                    at += i + from.len();
                }
                None => {
                    out.extend_from_slice(&haystack[at..]);
                    break;
                }
            }
        }
        out
    }

    #[test]
    fn a_non_multipart_container_and_an_empty_part_list_are_refused() {
        assert_eq!(
            compose("application/sdp", &[sdp()]),
            Err(MultipartError::NotMultipart { content_type: "application/sdp".into() })
        );
        assert_eq!(compose("multipart/mixed", &[]), Err(MultipartError::NoParts));
    }

    fn data() -> MultipartPart {
        MultipartPart::new("application/vnd.example.data", vec![0x77, 0x15, 0x00, 0x0a])
            .with_header("Content-ID", "<data@example.invalid>")
            .with_header("Content-Transfer-Encoding", "binary")
    }

    fn location() -> MultipartPart {
        MultipartPart::new("application/pidf+xml", b"<presence/>".to_vec())
            .with_header("Content-ID", "<loc@example.invalid>")
    }

    /// RFC 5621 §3: a bare description beside parts frames as
    /// `multipart/mixed`, description first and byte-exact, the parts after it
    /// with their own entity headers, `MIME-Version` on the message.
    #[test]
    fn a_bare_description_and_parts_frame_as_multipart_mixed() {
        let attached = attach(Some("application/sdp"), &[], &sdp().payload, &[data()]);
        let ct = attached.content_type.as_deref().expect("a body");
        assert!(media(ct).is("multipart/mixed"), "{ct}");
        assert_eq!(attached.headers, [("MIME-Version".to_string(), "1.0".to_string())]);
        let boundary =
            media(ct).param("boundary").and_then(ParamValue::as_str).unwrap().to_string();
        let parts = decompose(&attached.body, &boundary);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].content_type, "application/sdp");
        assert_eq!(&attached.body[parts[0].offset..][..parts[0].len], sdp().payload.as_slice());
        assert_eq!(parts[1].content_type, "application/vnd.example.data");
        assert_eq!(parts[1].content_id.as_deref(), Some("<data@example.invalid>"));
        assert_eq!(&attached.body[parts[1].offset..][..parts[1].len], data().payload.as_slice());
    }

    /// The parts replace every non-description part the source frames; the
    /// framed description keeps its own entity headers.
    #[test]
    fn the_parts_replace_the_sources_other_parts() {
        let source = compose(
            "multipart/mixed",
            &[sdp().with_header("Content-Disposition", "session"), location()],
        )
        .unwrap();
        let attached = attach(Some(&source.content_type), &[], &source.body, &[data()]);
        let ct = attached.content_type.unwrap();
        let boundary =
            media(&ct).param("boundary").and_then(ParamValue::as_str).unwrap().to_string();
        let parts = decompose(&attached.body, &boundary);
        let types: Vec<&str> = parts.iter().map(|p| p.content_type.as_str()).collect();
        assert_eq!(types, ["application/sdp", "application/vnd.example.data"]);
        assert_eq!(parts[0].headers, [("Content-Disposition".to_string(), "session".to_string())]);
    }

    /// Without a description one part is the body, its entity headers and
    /// `MIME-Version` the message's; several frame as `multipart/mixed`; none
    /// leave no body.
    #[test]
    fn without_a_description_one_part_is_the_body() {
        let one = attach(None, &[], b"", &[location()]);
        assert_eq!(one.content_type.as_deref(), Some("application/pidf+xml"));
        assert_eq!(
            one.headers,
            [
                ("MIME-Version".to_string(), "1.0".to_string()),
                ("Content-ID".to_string(), "<loc@example.invalid>".to_string())
            ]
        );
        assert_eq!(one.body, location().payload);

        let several = attach(Some("text/plain"), &[], b"note", &[location(), data()]);
        assert!(media(several.content_type.as_deref().unwrap()).is("multipart/mixed"));
        assert_eq!(several.headers, [("MIME-Version".to_string(), "1.0".to_string())]);

        let none = attach(None, &[], b"", &[]);
        assert_eq!(none, Attached { content_type: None, headers: Vec::new(), body: Vec::new() });
        let bare = attach(Some("application/sdp"), &[], &sdp().payload, &[]);
        assert_eq!(bare.content_type.as_deref(), Some("application/sdp"));
        assert!(bare.headers.is_empty(), "a description alone is no MIME entity of ours");
        assert_eq!(bare.body, sdp().payload);
    }

    /// RFC 2045 §9: `MIME-Version` and the `Content-` fields describe the body.
    #[test]
    fn the_entity_headers_are_mime_version_and_the_content_fields() {
        for name in ["MIME-Version", "mime-version", "Content-ID", "content-disposition"] {
            assert!(is_entity_header(name), "{name}");
        }
        for name in ["Contact", "Content-", "Via", "X-Content-Id"] {
            assert!(!is_entity_header(name), "{name}");
        }
    }

    /// A part's own `Content-Type` header types it; its `MIME-Version` and
    /// its structural `Content-Length` never ride, lifted or framed.
    #[test]
    fn a_parts_message_level_headers_are_filtered() {
        let stated = MultipartPart::new("application/octet-stream", b"hello".to_vec())
            .with_header("Content-Type", "application/vnd.example.data")
            .with_header("MIME-Version", "1.0")
            .with_header("Content-Length", "5")
            .with_header("Content-ID", "<d@example.invalid>");
        let one = attach(None, &[], b"", &[stated.clone()]);
        assert_eq!(one.content_type.as_deref(), Some("application/vnd.example.data"));
        assert_eq!(
            one.headers,
            [
                ("MIME-Version".to_string(), "1.0".to_string()),
                ("Content-ID".to_string(), "<d@example.invalid>".to_string())
            ]
        );

        let framed = attach(Some("application/sdp"), &[], &sdp().payload, &[stated]);
        let ct = media(framed.content_type.as_deref().unwrap());
        let boundary = ct.param("boundary").and_then(ParamValue::as_str).unwrap().to_string();
        let parts = decompose(&framed.body, &boundary);
        assert_eq!(parts[1].content_type, "application/vnd.example.data");
        assert_eq!(
            parts[1].entity_headers,
            [("Content-ID".to_string(), "<d@example.invalid>".to_string())]
        );
    }

    /// The description keeps its entity headers: a framed one in wire order, a
    /// bare one those the message stated for it.
    #[test]
    fn the_description_keeps_its_entity_headers_in_order() {
        let source = compose(
            "multipart/mixed",
            &[
                sdp()
                    .with_header("Content-Disposition", "session")
                    .with_header("Content-ID", "<s@example.invalid>"),
                location(),
            ],
        )
        .unwrap();
        let attached = attach(Some(&source.content_type), &[], &source.body, &[data()]);
        let boundary = media(attached.content_type.as_deref().unwrap())
            .param("boundary")
            .and_then(ParamValue::as_str)
            .unwrap()
            .to_string();
        let parts = decompose(&attached.body, &boundary);
        assert_eq!(
            parts[0].entity_headers,
            [
                ("Content-Disposition".to_string(), "session".to_string()),
                ("Content-ID".to_string(), "<s@example.invalid>".to_string())
            ]
        );

        let message = [
            ("Content-Disposition".to_string(), "session".to_string()),
            ("MIME-Version".to_string(), "1.0".to_string()),
            ("Content-Length".to_string(), "40".to_string()),
        ];
        let bare = attach(Some("application/sdp"), &message, &sdp().payload, &[data()]);
        let boundary = media(bare.content_type.as_deref().unwrap())
            .param("boundary")
            .and_then(ParamValue::as_str)
            .unwrap()
            .to_string();
        let parts = decompose(&bare.body, &boundary);
        assert_eq!(
            parts[0].entity_headers,
            [("Content-Disposition".to_string(), "session".to_string())]
        );
    }

    /// A body's entity parts: each part of a multipart body, or the bare body
    /// as one part described by the message's entity headers.
    #[test]
    fn a_bodys_entity_parts() {
        let message = [
            ("Content-ID".to_string(), "<x@example.invalid>".to_string()),
            ("Geolocation".to_string(), "<cid:x@example.invalid>".to_string()),
        ];
        let bare = entity_parts(Some("application/pidf+xml"), &message, b"<presence/>");
        assert_eq!(
            bare,
            [MultipartPart::new("application/pidf+xml", b"<presence/>".to_vec())
                .with_header("Content-ID", "<x@example.invalid>")],
            "a non-entity header stays the message's"
        );
        let framed = compose("multipart/mixed", &[sdp(), data()]).unwrap();
        let parts = entity_parts(Some(&framed.content_type), &[], &framed.body);
        assert_eq!(parts, [sdp(), data()]);
        assert!(entity_parts(Some("application/sdp"), &[], b"").is_empty());
        assert!(entity_parts(None, &[], b"v=0").is_empty());
    }

    /// A part serializes its bytes as base64 text and reads them back.
    #[test]
    fn a_part_serializes_its_bytes_as_base64() {
        let json = serde_json::to_value(data()).unwrap();
        assert_eq!(json["payload"], "dxUACg==");
        assert_eq!(serde_json::from_value::<MultipartPart>(json).unwrap(), data());
    }
}
