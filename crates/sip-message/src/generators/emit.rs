//! Shared construction plumbing every generator funnels through: header
//! literals, name-addr wrapping, body framing, and the panic-on-malformed
//! hydration boundary.

use crate::parser::custom::{hydrate_request, hydrate_response};
use crate::types::{SipHeader, SipRequest, SipResponse};

pub(super) fn h(name: &str, value: impl Into<String>) -> SipHeader {
    SipHeader { name: name.to_string(), value: value.into() }
}

/// `hydrate_request` cannot fail for stack-built (well-formed) input; a
/// failure is a stack bug, surfaced as a panic at construction time rather
/// than threading a `Result` through every generator.
pub(super) fn make_request(
    method: &str,
    uri: &str,
    headers: Vec<SipHeader>,
    body: Vec<u8>,
) -> SipRequest {
    hydrate_request(method, uri, headers, body)
        .unwrap_or_else(|e| panic!("generators built a malformed request: {}", e.reason))
}

pub(super) fn make_response(
    status: u16,
    reason: &str,
    headers: Vec<SipHeader>,
    body: Vec<u8>,
) -> SipResponse {
    hydrate_response(status, reason, headers, body)
        .unwrap_or_else(|e| panic!("generators built a malformed response: {}", e.reason))
}

/// Wrap a bare URI in angle brackets, unless it already contains `<` (a full
/// name-addr with display name passes through unchanged).
pub(super) fn wrap_uri(uri_or_name_addr: &str) -> String {
    if uri_or_name_addr.contains('<') {
        uri_or_name_addr.to_string()
    } else {
        format!("<{uri_or_name_addr}>")
    }
}

/// Append Content-Type (when body is non-empty and the caller didn't already
/// include one) + Content-Length (RFC 3261 §7.4.1).
pub(super) fn append_body_headers(
    headers: &mut Vec<SipHeader>,
    body: &[u8],
    content_type: Option<&str>,
) {
    let has_ct = headers.iter().any(|hdr| hdr.name.eq_ignore_ascii_case("content-type"));
    if !body.is_empty() && !has_ct {
        headers.push(h("Content-Type", content_type.unwrap_or("application/sdp")));
    }
    headers.push(h("Content-Length", body.len().to_string()));
}
