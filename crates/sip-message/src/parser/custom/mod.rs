//! Custom SIP parser — RFC 3261 compliant, zero-regex, state-machine based.
//!
//! Pipeline:
//!   scanner -> start_line -> headers -> structured_headers -> extract_fields
//!
//! Observationally pure: internal helpers may use early returns as control
//! flow, but every escape is translated into `Err(SipParseError)` at this
//! entry point. No panic ever crosses the boundary.

use bytes::Bytes;

use crate::draft::{RequestLine, StatusLine, SIP_VERSION};
use crate::error::SipParseError;
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
pub mod optional_headers;
pub mod partial;
pub(crate) mod request_uri;
mod routes;
pub mod scanner;
pub mod start_line;
pub mod structured_headers;
mod top_via;

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
    let limits = SipParserLimits::default();
    let uri = SipStr::owned(uri);
    let idx = HeaderIndex::build(&headers);
    let eager = extract_request_fields(&idx, &uri, &limits, Some(method), ExtractMode::Hydrate)?;
    let optional = optional_headers::extract_optional_indexed(&idx);
    let body: Bytes = body.into();
    let start = RequestLine {
        method: crate::method::Method::from_wire(method),
        uri: eager.request_uri,
        version: SipStr::from_static(SIP_VERSION),
    };
    // A message always carries its datagram: `image()` is what the transport
    // sends, so a hydrated request renders it once here.
    let image = Bytes::from(crate::serializer::render(&headers, &body, |out| {
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
        let parsed = headers::parse_headers(&mut s, &text, block.lines, &self.limits)?;
        match parsed.refusal {
            Some(refusal) => Err(refusal),
            None => Ok(parsed.headers),
        }
    }

    pub fn with_limits(limits: SipParserLimits) -> Self {
        Self { limits }
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
        } = headers::parse_headers(&mut s, &text, block.lines, limits)?;

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
                let optional = optional_headers::extract_optional_indexed(&idx);
                Ok(SipMessage::Request(SipRequest::new(
                    RequestLine {
                        method: crate::method::Method::from_wire(method),
                        uri: eager.request_uri,
                        version: text.span(rl.version.start, rl.version.len()),
                    },
                    MessageCore::new(headers_vec, eager.common, optional, body, raw),
                )))
            }
            StartLine::Status(sl) => {
                let core = extract_response_fields(&idx, sl.status, limits, mode)?;
                let optional = optional_headers::extract_optional_indexed(&idx);
                Ok(SipMessage::Response(SipResponse::new(
                    StatusLine {
                        version: text.span(sl.version.start, sl.version.len()),
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
/// In a block that is not, each byte that is no UTF-8 stands as a CR in the
/// image: a CR stands alone in no line of a block whose breaks are all CRLF,
/// so the line holding one is refused like any line breaking the grammar, and
/// a field a response echoes reads only from bytes exactly as received.
fn decode_image(header_block: &[u8]) -> (SharedText, Option<SipParseError>) {
    let e = match std::str::from_utf8(header_block) {
        Ok(text) => return (SharedText::new(text), None),
        Err(e) => e,
    };
    let refusal = SipParseError::new(format!(
        "header block is not UTF-8 (RFC 3629) at byte {}",
        e.valid_up_to()
    ));
    let mut image = String::with_capacity(header_block.len());
    for chunk in header_block.utf8_chunks() {
        image.push_str(chunk.valid());
        image.extend(chunk.invalid().iter().map(|_| '\r'));
    }
    (SharedText::from(image), Some(refusal))
}
