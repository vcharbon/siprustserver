//! Custom SIP parser — zero-regex, state-machine based. It holds wire bytes to
//! the RFC 3261 §25.1 grammar through one reader per grammar, beside the gates
//! and bounds ADR-0001 states and the exemptions a deployment names.
//!
//! Pipeline:
//!   scanner -> start_line -> headers -> extract_fields (the typed readers)
//!
//! Observationally pure: internal helpers may use early returns as control
//! flow, but every escape is translated into `Err(SipParseError)` at this
//! entry point. No panic ever crosses the boundary.

use bytes::Bytes;

use crate::draft::{RequestLine, StatusLine, SIP_VERSION};
use crate::error::SipParseError;
use crate::header_table::HeaderTable;
use crate::parser::{Framing, SipParser, SipParserLimits};
use crate::sip_str::{SharedText, SipStr};
use crate::types::{MessageCore, SipMessage, SipRequest, SipResponse};

pub(crate) mod compact_forms;
pub mod contact_entries;
mod dialog_contact;
mod dialog_ids;
pub mod extract_fields;
pub mod header_index;
pub mod headers;
mod name_forms;
pub mod optional_headers;
pub mod partial;
pub(crate) mod request_uri;
mod routes;
pub mod scanner;
pub mod start_line;
pub mod structured_headers;
mod top_via;
mod vouched;

use extract_fields::{extract_request_fields, extract_response_fields, ExtractMode, RequestEager};
use header_index::HeaderIndex;
use scanner::Scanner;
use start_line::{parse_start_line, StartLine};

use crate::types::SipHeader;

/// Build a trusted [`SipRequest`] from already-structured components (method,
/// Request-URI, header list, body). Used by `generators` to assemble outbound
/// messages.
/// Runs the eager field extraction in [`ExtractMode::Hydrate`] (lenient: no
/// strict wire-grammar gates), so well-formed stack-built input always hydrates.
pub fn hydrate_request(
    method: &str,
    uri: &str,
    headers: Vec<SipHeader>,
    body: impl Into<Bytes>,
) -> Result<SipRequest, SipParseError> {
    let uri = SipStr::owned(uri);
    let headers = HeaderTable::new(headers);
    let idx = HeaderIndex::build(&headers);
    let eager = extract_request_fields(
        &idx,
        &uri,
        &SipParserLimits::DEFAULT,
        Some(method),
        ExtractMode::Hydrate,
    )?;
    let optional = optional_headers::extract_optional_indexed(&idx);
    let body: Bytes = body.into();
    let start = RequestLine {
        method: crate::method::Method::from_wire(method),
        uri: eager.request_uri,
        version: SipStr::from_static(SIP_VERSION),
    };
    // A message always carries its datagram: `image()` is what the transport
    // sends, so a hydrated request renders it once here.
    let image = Bytes::from(crate::serializer::render(headers.lines(), &body, |out| {
        use std::io::Write;
        let _ = write!(out, "{} {} {}", start.method, start.uri.text(), start.version);
    }));
    Ok(SipRequest::new(start, MessageCore::new(headers, eager.common, optional, body, image)))
}

/// The header block of `raw`, refused when it holds more lines than
/// [`SipParserLimits::max_header_lines`]: one scan of the head, before any
/// header is decoded or read. The refusal carries no partial.
fn bounded_header_block(
    raw: &[u8],
    limits: &SipParserLimits,
) -> Result<scanner::HeaderBlock, SipParseError> {
    let block = scanner::header_block(raw);
    if block.lines > limits.max_header_lines {
        return Err(SipParseError::new(format!(
            "header block of {} lines exceeds limit {}",
            block.lines, limits.max_header_lines
        )));
    }
    Ok(block)
}

/// The production parser. Built with `SipParserLimits`.
#[derive(Debug, Clone)]
pub struct CustomParser {
    limits: SipParserLimits,
}

impl CustomParser {
    pub fn new() -> Self {
        Self { limits: SipParserLimits::default() }
    }

    /// The start line and header block of `raw` read as a message's, its
    /// header fields returned without the mandatory-field extraction a whole
    /// message passes — the reading a message fragment (RFC 3420) takes.
    pub fn parse_header_fields(&self, raw: &[u8]) -> Result<Vec<SipHeader>, SipParseError> {
        let block = bounded_header_block(raw, &self.limits)?;
        if let Some(bare) = block.bare_break {
            return Err(bare.refusal());
        }
        let (text, not_utf8) = decode_image(head(raw, block.end)?);
        if let Some(refusal) = not_utf8 {
            return Err(refusal);
        }
        let mut s = Scanner::new(text.as_str().as_bytes());
        parse_start_line(&mut s, text.as_str(), &self.limits)?;
        let fragment = headers::BlankLine::Optional;
        let parsed = headers::parse_headers(&mut s, &text, block.lines, &self.limits, fragment)?;
        match parsed.refusal {
            Some(refusal) => Err(refusal),
            None => Ok(parsed.headers.into_lines()),
        }
    }

    pub fn with_limits(limits: SipParserLimits) -> Self {
        Self { limits }
    }

    /// The refusal `req`, read by this parser, draws at first ingress, with the
    /// partial that answers it: for an element that learns only after the read
    /// that a request is at its first hop. `None` for a request with a To-tag,
    /// which carries what hops added to its dialog's lines, or one within
    /// the bound.
    pub fn first_ingress_refusal(&self, req: &SipRequest) -> Option<SipParseError> {
        if req.to().tag().is_some() {
            return None;
        }
        let lines = scanner::header_block(req.image()).lines;
        let uri_length = req.request_uri().text().len();
        self.limits.first_ingress.refusal(lines, uri_length, req.headers().iter())?;
        let at_first_ingress = SipParserLimits { at_first_ingress: true, ..self.limits.clone() };
        Self::with_limits(at_first_ingress).parse(req.image()).err()
    }
}

impl Default for CustomParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SipParser for CustomParser {
    fn name(&self) -> &str {
        "custom"
    }

    fn parse(&self, raw: &[u8]) -> Result<SipMessage, SipParseError> {
        self.parse_shared(Bytes::copy_from_slice(raw))
    }

    fn parse_shared(&self, raw: Bytes) -> Result<SipMessage, SipParseError> {
        let limits = &self.limits;

        // Split first, decode second: the body stays raw bytes (binary-safe,
        // and a slice of `raw` rather than a copy), and only the header block
        // becomes the text image every span points into.
        // A limit is never answered: no partial.
        let block = bounded_header_block(&raw, limits)?;
        // A bare CR or LF leaves the line structure in doubt: never answered.
        if let Some(bare) = block.bare_break {
            return Err(bare.refusal());
        }
        let body_at = block.end;
        let (text, not_utf8) = decode_image(head(&raw, body_at)?);
        let image = text.as_str();

        let mut s = Scanner::new(image.as_bytes());
        // A refused start line or a limit is never answered: no partial.
        let start = parse_start_line(&mut s, image, limits)?;
        let headers::ParsedHeaders {
            headers: headers_vec,
            content_length,
            refusal,
            echoed_line_refused,
        } = headers::parse_headers(
            &mut s,
            &text,
            block.lines,
            limits,
            headers::BlankLine::Required,
        )?;

        let mode = if limits.wire_grammar { ExtractMode::Wire } else { ExtractMode::Hydrate };

        // One dispatch pass over the header list feeds both the mandatory field
        // extraction and the optional-header parse.
        let idx = HeaderIndex::build(&headers_vec);

        // Every refusal from here on carries the partial a 400 answers it
        // with, when the request has one.
        let refuse = |error: SipParseError| match &start {
            StartLine::Request(rl) if !echoed_line_refused => SipParseError {
                partial: scanner::span_text(image, &rl.method)
                    .ok()
                    .and_then(|method| partial::request_partial(method, &idx, limits, mode)),
                ..error
            },
            _ => error,
        };
        if let Some(refusal) = not_utf8.or(refusal) {
            return Err(refuse(refusal));
        }

        // The body bound is the declared Content-Length; a datagram that
        // declares none ends with its bytes (RFC 3261 §18.3), a stream that
        // declares none is undelimited and refused (§20.14).
        let body: Bytes = match content_length {
            Some(0) => Bytes::new(),
            Some(declared) => {
                let available = raw.len().saturating_sub(body_at);
                let bounded = usize::try_from(declared)
                    .ok()
                    .and_then(|len| body_at.checked_add(len))
                    .filter(|&end| end <= raw.len());
                let Some(end) = bounded else {
                    return Err(refuse(SipParseError::new(format!(
                        "Content-Length {declared} exceeds remaining bytes {available}"
                    ))));
                };
                body_of(&raw, body_at..end).map_err(refuse)?
            }
            None => match limits.framing {
                Framing::Datagram => body_of(&raw, body_at..raw.len()).map_err(refuse)?,
                Framing::Stream => {
                    return Err(refuse(SipParseError::new(
                        "Content-Length is mandatory over a stream transport (RFC 3261 §20.14)",
                    )))
                }
            },
        };

        match start {
            StartLine::Request(ref rl) => {
                // A method is case-sensitive (RFC 3261 §7.1): the token as
                // written, a known method only in its own spelling.
                let method = scanner::span_text(image, &rl.method)?;
                let uri = text.span(rl.uri.start, rl.uri.len());
                let eager: RequestEager =
                    extract_request_fields(&idx, &uri, limits, Some(method), mode)
                        .map_err(refuse)?;
                if limits.at_first_ingress && eager.common.to().tag().is_none() {
                    let lines = headers_vec.lines().iter();
                    if let Some(past) = limits.first_ingress.refusal(block.lines, uri.len(), lines)
                    {
                        return Err(refuse(past));
                    }
                }
                let optional = optional_headers::extract_optional_indexed(&idx);
                Ok(SipMessage::Request(SipRequest::new(
                    RequestLine {
                        method: crate::method::Method::from_wire(method),
                        uri: eager.request_uri,
                        version: SipStr::from_static(SIP_VERSION),
                    },
                    MessageCore::new(headers_vec, eager.common, optional, body, raw),
                )))
            }
            StartLine::Status(sl) => {
                let core = extract_response_fields(&idx, sl.status, limits, mode)?;
                let optional = optional_headers::extract_optional_indexed(&idx);
                Ok(SipMessage::Response(SipResponse::new(
                    StatusLine {
                        version: SipStr::from_static(SIP_VERSION),
                        status: sl.status,
                        reason: text.span(sl.reason.start, sl.reason.len()),
                    },
                    MessageCore::new(headers_vec, core, optional, body, raw),
                )))
            }
        }
    }
}

/// The body of `raw` at `range`, sharing its bytes.
fn body_of(raw: &Bytes, range: std::ops::Range<usize>) -> Result<Bytes, SipParseError> {
    crate::checked::slice(raw, range.clone())
        .ok_or_else(|| SipParseError::new(format!("body {range:?} past the datagram")))
}

/// The header block of `raw`, the `end` [`scanner::header_block`] found.
fn head(raw: &[u8], end: usize) -> Result<&[u8], SipParseError> {
    raw.get(..end)
        .ok_or_else(|| SipParseError::new(format!("header block end {end} past the datagram")))
}

/// The header block as one shared text image, and the refusal of a block
/// that is not UTF-8 (RFC 3629). A UTF-8 block is the image byte for byte.
///
/// A surrogate pair written as two adjacent three-byte forms (CESU-8,
/// `ED A0-AF 80-BF ED B0-BF 80-BF`) stands in the image as the one character
/// it encodes, in UTF-8: past this point it is ordinary non-ASCII, read where
/// the grammar allows it and refused where it does not. Every other byte that
/// is no UTF-8 (a lone or reversed half, a Latin-1 byte, an overlong form, a
/// five- or six-byte form) refuses the block, and stands as a CR in the
/// image: a CR stands alone in no line of a block whose breaks are all CRLF,
/// so the line holding one is refused like any line breaking the grammar, and
/// a field a response echoes reads only from bytes as received.
pub(crate) fn decode_image(header_block: &[u8]) -> (SharedText, Option<SipParseError>) {
    if let Ok(text) = std::str::from_utf8(header_block) {
        return (SharedText::new(text), None);
    }
    let mut image = String::with_capacity(header_block.len());
    let mut refusal = None;
    let mut rest = header_block;
    while !rest.is_empty() {
        let (valid, after) = match std::str::from_utf8(rest) {
            Ok(text) => (text, &[][..]),
            Err(e) => {
                let (valid, after) = rest.split_at_checked(e.valid_up_to()).unwrap_or((rest, &[]));
                (std::str::from_utf8(valid).unwrap_or_default(), after)
            }
        };
        image.push_str(valid);
        if after.is_empty() {
            break;
        }
        if let Some((character, tail)) = surrogate_pair(after) {
            image.push(character);
            rest = tail;
            continue;
        }
        let at = header_block.len() - after.len();
        refusal.get_or_insert_with(|| {
            SipParseError::new(format!(
                "header block is not UTF-8 (RFC 3629) at byte {at}, in {}",
                line_holding(header_block, at)
            ))
        });
        let bad = std::str::from_utf8(after)
            .err()
            .and_then(|e| e.error_len())
            .unwrap_or(after.len())
            .max(1);
        image.extend(std::iter::repeat_n('\r', bad.min(after.len())));
        rest = after.get(bad..).unwrap_or_default();
    }
    (SharedText::from(image), refusal)
}

/// What a refusal names the line of `block` holding byte `at` by: the start
/// line, or the header the line's name spells, its bytes shown lossily.
fn line_holding(block: &[u8], at: usize) -> String {
    let before = block.get(..at).unwrap_or_default();
    let Some(start) = memchr::memrchr(b'\n', before).map(|lf| lf + 1) else {
        return "the start line".to_string();
    };
    let line = block.get(start..).unwrap_or_default();
    let end = memchr::memchr2(b':', b'\r', line).unwrap_or(line.len());
    format!("the {} header", String::from_utf8_lossy(line.get(..end).unwrap_or_default()))
}

/// The character a well-formed surrogate pair at the head of `bytes` encodes,
/// each half as its three-byte form, and the bytes after it.
fn surrogate_pair(bytes: &[u8]) -> Option<(char, &[u8])> {
    let ([0xed, high_mid, high_low, 0xed, low_mid, low_low], tail) =
        bytes.split_first_chunk::<6>()?
    else {
        return None;
    };
    let continuation = |b: u8| (0x80..=0xbf).contains(&b);
    let well_formed = (0xa0..=0xaf).contains(high_mid)
        && (0xb0..=0xbf).contains(low_mid)
        && continuation(*high_low)
        && continuation(*low_low);
    if !well_formed {
        return None;
    }
    let half = |mid: u8, low: u8| (u32::from(mid & 0x0f) << 6) | u32::from(low & 0x3f);
    let scalar = 0x10000 + (half(*high_mid, *high_low) << 10) + half(*low_mid, *low_low);
    Some((char::from_u32(scalar)?, tail))
}
