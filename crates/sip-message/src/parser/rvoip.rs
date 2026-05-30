//! `RvoipParser` — dev-only parity oracle wrapping `rvoip-sip-core`.
//! Port of the role played by `src/sip/parsers/native-adapter.ts`.
//!
//! Drives the compliance matrix as the looser, lexical reference impl. rvoip is
//! intentionally LESS strict than [`crate::CustomParser`] (ADR-0007 adds gates
//! on top); the matrix encodes where the two are expected to diverge (ADR-0001).
//! Feature-gated behind `rvoip-oracle` so production builds never link it.
//!
//! Oracle contract: `RvoipParser::parse` returns `Ok` **iff** rvoip accepts the
//! bytes. The matrix only consults that accept/reject verdict — it never reads
//! the returned [`SipMessage`]'s fields — so the structured-header fields below
//! are populated best-effort from rvoip's start line and headers, with safe
//! placeholders for the eager mandatory fields rvoip does not surface the same
//! way. This is a thin parity shell, NOT a second full field-extractor.

use crate::error::SipParseError;
use crate::parser::SipParser;
use crate::types::SipMessage;

#[derive(Debug, Clone, Default)]
pub struct RvoipParser;

#[cfg(feature = "rvoip-oracle")]
impl SipParser for RvoipParser {
    fn name(&self) -> &str {
        "rvoip"
    }

    fn parse(&self, raw: &[u8]) -> Result<SipMessage, SipParseError> {
        use rvoip_sip_core::parser::message::{parse_message_with_mode, ParseMode};
        use rvoip_sip_core::types::sip_message::Message;

        match parse_message_with_mode(raw, ParseMode::Strict) {
            Err(e) => Err(SipParseError::new(format!("rvoip rejected: {e}"))),
            Ok(Message::Request(req)) => Ok(SipMessage::Request(shell_request(&req, raw))),
            Ok(Message::Response(resp)) => Ok(SipMessage::Response(shell_response(&resp, raw))),
        }
    }
}

// Without the feature, the oracle is a no-op stub so `RvoipParser` always
// exists; the matrix `#[cfg]`s on the feature for the real second column.
#[cfg(not(feature = "rvoip-oracle"))]
impl SipParser for RvoipParser {
    fn name(&self) -> &str {
        "rvoip"
    }

    fn parse(&self, _raw: &[u8]) -> Result<SipMessage, SipParseError> {
        Err(SipParseError::new("RvoipParser requires the `rvoip-oracle` feature"))
    }
}

#[cfg(feature = "rvoip-oracle")]
fn shell_request(
    req: &rvoip_sip_core::types::sip_request::Request,
    raw: &[u8],
) -> crate::types::SipRequest {
    use crate::types::{ContactSet, SipRequest};
    let uri = req.uri.to_string();
    SipRequest {
        method: req.method.to_string(),
        request_uri: placeholder_request_uri(),
        uri,
        version: "SIP/2.0".to_string(),
        from: placeholder_name_addr(),
        to: placeholder_name_addr(),
        call_id: String::new(),
        cseq: placeholder_cseq(),
        via: placeholder_via(),
        contacts: ContactSet::Contacts(Vec::new()),
        optional: crate::parser::custom::optional_headers::extract_optional(&[]),
        headers: shell_headers(&req.headers),
        body: req.body.to_vec(),
        raw: raw.to_vec(),
    }
}

#[cfg(feature = "rvoip-oracle")]
fn shell_response(
    resp: &rvoip_sip_core::types::sip_response::Response,
    raw: &[u8],
) -> crate::types::SipResponse {
    use crate::types::{ContactSet, SipResponse};
    SipResponse {
        version: "SIP/2.0".to_string(),
        status: resp.status.as_u16(),
        reason: resp.reason.clone().unwrap_or_else(|| resp.status.to_string()),
        from: placeholder_name_addr(),
        to: placeholder_name_addr(),
        call_id: String::new(),
        cseq: placeholder_cseq(),
        via: placeholder_via(),
        contacts: ContactSet::Contacts(Vec::new()),
        optional: crate::parser::custom::optional_headers::extract_optional(&[]),
        headers: shell_headers(&resp.headers),
        body: resp.body.to_vec(),
        raw: raw.to_vec(),
    }
}

// --- placeholders for the eager mandatory fields (never read by the matrix) ---

#[cfg(feature = "rvoip-oracle")]
fn placeholder_name_addr() -> crate::types::NameAddr {
    crate::types::NameAddr {
        display_name: None,
        uri: String::new(),
        tag: None,
        params: crate::types::Params::new(),
    }
}

#[cfg(feature = "rvoip-oracle")]
fn placeholder_cseq() -> crate::types::CSeq {
    crate::types::CSeq { seq: 0, method: String::new() }
}

#[cfg(feature = "rvoip-oracle")]
fn placeholder_via() -> crate::types::NonEmpty<crate::types::Via> {
    crate::types::NonEmpty::new(crate::types::Via {
        transport: String::new(),
        host: String::new(),
        port: None,
        branch: None,
        params: crate::types::Params::new(),
    })
}

#[cfg(feature = "rvoip-oracle")]
fn placeholder_request_uri() -> crate::types::RequestUri {
    crate::types::RequestUri {
        scheme: String::new(),
        user: None,
        host: String::new(),
        port: None,
        params: std::collections::BTreeMap::new(),
    }
}

/// Render rvoip's typed headers back to `name: value` pairs (best-effort, via
/// each header's `Display`). Used only so `get_header` works on the shell; the
/// matrix does not depend on it.
#[cfg(feature = "rvoip-oracle")]
fn shell_headers(
    headers: &[rvoip_sip_core::types::header::TypedHeader],
) -> Vec<crate::types::SipHeader> {
    headers
        .iter()
        .filter_map(|h| {
            let line = h.to_string();
            line.split_once(':').map(|(name, value)| crate::types::SipHeader {
                name: name.trim().to_string(),
                value: value.trim().to_string(),
            })
        })
        .collect()
}
