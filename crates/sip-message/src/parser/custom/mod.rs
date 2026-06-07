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

use crate::error::SipParseError;
use crate::parser::{SipParser, SipParserLimits};
use crate::types::{NonEmpty, SipMessage, SipRequest, SipResponse};

pub mod scanner;
pub mod start_line;
pub mod headers;
pub mod structured_headers;
pub mod extract_fields;
pub mod optional_headers;
mod compact_forms;

use extract_fields::{
    extract_request_fields, extract_response_fields, ExtractMode, RequestEager,
};
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
    body: Vec<u8>,
) -> Result<SipRequest, SipParseError> {
    let limits = SipParserLimits::default();
    let eager = extract_request_fields(&headers, uri, &limits, Some(method), ExtractMode::Hydrate)?;
    let c = eager.common;
    let via = non_empty_vias(c.vias)?;
    let optional = optional_headers::extract_optional(&headers);
    Ok(SipRequest {
        method: crate::method::Method::from_wire(method),
        uri: uri.to_string(),
        request_uri: eager.request_uri,
        version: "SIP/2.0".to_string(),
        from: c.from,
        to: c.to,
        call_id: c.call_id,
        cseq: c.cseq,
        via,
        contacts: c.contacts,
        optional,
        headers,
        body,
        raw: Vec::new(),
    })
}

/// Build a trusted [`SipResponse`] from already-structured components — the
/// port of `hydrateResponse`. See [`hydrate_request`].
pub fn hydrate_response(
    status: u16,
    reason: &str,
    headers: Vec<SipHeader>,
    body: Vec<u8>,
) -> Result<SipResponse, SipParseError> {
    let limits = SipParserLimits::default();
    let c = extract_response_fields(&headers, status, &limits, ExtractMode::Hydrate)?;
    let via = non_empty_vias(c.vias)?;
    let optional = optional_headers::extract_optional(&headers);
    Ok(SipResponse {
        version: "SIP/2.0".to_string(),
        status,
        reason: reason.to_string(),
        from: c.from,
        to: c.to,
        call_id: c.call_id,
        cseq: c.cseq,
        via,
        contacts: c.contacts,
        optional,
        headers,
        body,
        raw: Vec::new(),
    })
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
        let limits = &self.limits;

        let mut s = Scanner::new(raw);
        let start = parse_start_line(&mut s, limits)?;
        let parsed = headers::parse_headers(&mut s, limits)?;
        let headers_vec = parsed.headers;
        let content_length = parsed.content_length as usize;

        let body: Vec<u8> = if content_length > 0 {
            if s.remaining() < content_length {
                return Err(SipParseError::new(format!(
                    "Content-Length {} exceeds remaining bytes {}",
                    content_length,
                    s.remaining()
                )));
            }
            raw[s.pos..s.pos + content_length].to_vec()
        } else {
            Vec::new()
        };

        let mode = if limits.wire_grammar { ExtractMode::Wire } else { ExtractMode::Hydrate };

        match start {
            StartLine::Request(rl) => {
                let eager: RequestEager = extract_request_fields(
                    &headers_vec,
                    &rl.uri,
                    limits,
                    Some(&rl.method),
                    mode,
                )?;
                let c = eager.common;
                let via = non_empty_vias(c.vias)?;
                let optional = optional_headers::extract_optional(&headers_vec);
                Ok(SipMessage::Request(SipRequest {
                    method: crate::method::Method::from_wire(&rl.method),
                    uri: rl.uri,
                    request_uri: eager.request_uri,
                    version: rl.version,
                    from: c.from,
                    to: c.to,
                    call_id: c.call_id,
                    cseq: c.cseq,
                    via,
                    contacts: c.contacts,
                    optional,
                    headers: headers_vec,
                    body,
                    raw: raw.to_vec(),
                }))
            }
            StartLine::Status(sl) => {
                let c = extract_response_fields(&headers_vec, sl.status, limits, mode)?;
                let via = non_empty_vias(c.vias)?;
                let optional = optional_headers::extract_optional(&headers_vec);
                Ok(SipMessage::Response(SipResponse {
                    version: sl.version,
                    status: sl.status,
                    reason: sl.reason,
                    from: c.from,
                    to: c.to,
                    call_id: c.call_id,
                    cseq: c.cseq,
                    via,
                    contacts: c.contacts,
                    optional,
                    headers: headers_vec,
                    body,
                    raw: raw.to_vec(),
                }))
            }
        }
    }
}

/// Build a `NonEmpty<Via>` from the extracted Via list. `extract_*_fields`
/// already rejects an empty Via set, so this is a belt-and-suspenders guard.
fn non_empty_vias(vias: Vec<crate::types::Via>) -> Result<NonEmpty<crate::types::Via>, SipParseError> {
    let mut it = vias.into_iter();
    match it.next() {
        Some(head) => Ok(NonEmpty::from_parts(head, it.collect())),
        None => Err(SipParseError::new("Missing mandatory Via header")),
    }
}
