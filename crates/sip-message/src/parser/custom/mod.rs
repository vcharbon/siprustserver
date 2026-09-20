//! Custom SIP parser — RFC 3261 compliant, zero-regex, state-machine based.
//! Port of `src/sip/parsers/custom/` + `src/sip/parsers/extract-fields.ts`.
//!
//! Pipeline (each submodule ports the like-named TS file):
//!   scanner -> start_line -> headers -> structured_headers -> extract_fields
//!
//! Observationally pure: internal helpers may use early returns as control
//! flow, but every escape is translated into `Err(SipParseError)` at this
//! entry point. No panic ever crosses the boundary.
//!
//! STATUS: scaffolded, not yet ported. See MIGRATION_STATUS.md.

use bytes::Bytes;

use crate::draft::{RequestLine, StatusLine, SIP_VERSION};
use crate::error::SipParseError;
use crate::parser::{Framing, SipParser, SipParserLimits};
use crate::sip_str::{SharedText, SipStr};
use crate::types::{MessageCore, SipMessage, SipRequest, SipResponse};

pub(crate) mod compact_forms;
pub mod extract_fields;
pub mod header_index;
pub mod headers;
pub mod optional_headers;
pub mod scanner;
pub mod start_line;
pub mod structured_headers;

use extract_fields::{extract_request_fields, extract_response_fields, ExtractMode, RequestEager};
use header_index::HeaderIndex;
use scanner::Scanner;
use start_line::{parse_start_line, StartLine};

use crate::types::SipHeader;

/// Build a trusted [`SipRequest`] from already-structured components (method,
/// Request-URI, header list, body) — the port of `extract-fields.ts`'
/// `hydrateRequest`. Used by `generators` to assemble outbound messages.
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

/// The production parser. Built with `SipParserLimits`.
#[derive(Debug, Clone)]
pub struct CustomParser {
    limits: SipParserLimits,
}

impl CustomParser {
    pub fn new() -> Self {
        Self { limits: SipParserLimits::default() }
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
        let block = scanner::header_block(&raw);
        let body_at = block.end;
        let text = decode_image(&raw[..body_at]);
        let image = text.as_str();

        let mut s = Scanner::new(image.as_bytes());
        let start = parse_start_line(&mut s, image, limits)?;
        let parsed = headers::parse_headers(&mut s, &text, block.lines, limits)?;
        let headers_vec = parsed.headers;

        // The body bound is the declared Content-Length; a datagram that
        // declares none ends with its bytes (RFC 3261 §18.3), a stream that
        // declares none is undelimited and refused (§20.14).
        let available = raw.len() - body_at;
        let body: Bytes = match parsed.content_length {
            Some(0) => Bytes::new(),
            Some(declared) => {
                let content_length = declared as usize;
                if available < content_length {
                    return Err(SipParseError::new(format!(
                        "Content-Length {content_length} exceeds remaining bytes {available}"
                    )));
                }
                raw.slice(body_at..body_at + content_length)
            }
            None => match limits.framing {
                Framing::Datagram => raw.slice(body_at..),
                Framing::Stream => {
                    return Err(SipParseError::new(
                        "Content-Length is mandatory over a stream transport (RFC 3261 §20.14)",
                    ))
                }
            },
        };

        let mode = if limits.wire_grammar { ExtractMode::Wire } else { ExtractMode::Hydrate };

        // One dispatch pass over the header list feeds both the mandatory field
        // extraction and the optional-header parse.
        let idx = HeaderIndex::build(&headers_vec);

        match start {
            StartLine::Request(rl) => {
                let method = start_line::canonical_method(image, rl.method);
                let uri = text.span(rl.uri.start, rl.uri.len());
                let eager: RequestEager =
                    extract_request_fields(&idx, &uri, limits, Some(&method), mode)?;
                let optional = optional_headers::extract_optional_indexed(&idx);
                Ok(SipMessage::Request(SipRequest::new(
                    RequestLine {
                        method: crate::method::Method::from_wire(&method),
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

/// The header block as one shared text image. Predominantly-ASCII input takes
/// the `str::from_utf8` validation fast path; genuinely invalid bytes fall back
/// to the same lossy decode the per-field path used, so the image is
/// byte-identical either way.
fn decode_image(header_block: &[u8]) -> SharedText {
    match std::str::from_utf8(header_block) {
        Ok(s) => SharedText::new(s),
        Err(_) => SharedText::from(String::from_utf8_lossy(header_block).into_owned()),
    }
}
