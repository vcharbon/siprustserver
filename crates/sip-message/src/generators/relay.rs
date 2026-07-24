//! B2BUA relay transparency: the stack-owned structural header set, the
//! non-structural pass-through extractor, and rebuilding a response for the
//! peer leg from snapshotted fields (RFC 3261 §16.6 / §12.1.1).

use super::emit::{append_body_headers, h, make_response};
use super::spec::ContactSpec;
use crate::types::{SipHeader, SipMessage, SipResponse};

// RFC 3261 §16.6 — stack-owned headers; never copied transparently.
const STRUCTURAL_HEADERS: &[&str] = &[
    "via",
    "contact",
    "from",
    "to",
    "call-id",
    "cseq",
    "max-forwards",
    "content-length",
    "content-type",
    "record-route",
    "route",
];

/// Every header from `msg` whose name is NOT in the stack-owned structural set
/// — callers pass the result through `extra_headers` when relaying so
/// transparent fields (Allow, Supported, P-Asserted-Identity, …) flow through
/// unchanged while the generator owns the dialog headers.
pub fn extract_non_structural_headers(msg: &SipMessage) -> Vec<SipHeader> {
    msg.headers()
        .iter()
        .filter(|hdr| {
            let lower = hdr.name.to_ascii_lowercase();
            !STRUCTURAL_HEADERS.contains(&lower.as_str())
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct GenerateRelayedResponseOpts {
    /// Via headers from the target-facing request (one per entry).
    pub vias: Vec<String>,
    pub from: String,
    pub to: String,
    pub call_id: String,
    /// Full CSeq value (`"<number> <METHOD>"`).
    pub cseq: String,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    /// Non-structural headers carried through from the source response (§16.6).
    pub transparent_headers: Vec<SipHeader>,
    /// Record-Route headers reflected verbatim, in received order.
    pub record_routes: Vec<String>,
    pub contact: Option<ContactSpec>,
}

/// Rebuild a B2BUA-side response for relay to a peer leg (RFC 3261 §16.6 /
/// §12.1.1).
pub fn generate_relayed_response(
    status: u16,
    reason: &str,
    opts: &GenerateRelayedResponseOpts,
) -> SipResponse {
    let body = opts.body.clone();

    let mut headers: Vec<SipHeader> = Vec::new();
    for via in &opts.vias {
        headers.push(h("Via", via.clone()));
    }
    for rr in &opts.record_routes {
        headers.push(h("Record-Route", rr.clone()));
    }
    headers.push(h("From", opts.from.clone()));
    headers.push(h("To", opts.to.clone()));
    headers.push(h("Call-ID", opts.call_id.clone()));
    headers.push(h("CSeq", opts.cseq.clone()));

    headers.extend(opts.transparent_headers.iter().cloned());

    if let Some(contact) = &opts.contact {
        headers.push(h("Contact", contact.header_value()));
    }
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    make_response(status, reason, headers, body)
}
