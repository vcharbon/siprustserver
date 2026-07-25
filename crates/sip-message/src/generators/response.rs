//! UAS response generation: echo Via / From / To / Call-ID / CSeq from the
//! request being answered (RFC 3261 §8.2.6.2). Rebuilding a response from
//! B2BUA-snapshotted fields lives in [`super::relay`].

use super::emit::{append_body_headers, h, make_response};
use super::spec::ContactSpec;
use crate::message_helpers::{get_header, stamp_received_rport_on_via};
use crate::parser::custom::structured_headers::parse_name_addr;
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest, SipResponse};

/// Deterministic fallback To-tag for a non-100 response whose request carried a
/// tag-less To and whose caller supplied no `to_tag`. Derived from the Call-ID so
/// it is stable per call (a retransmit re-derives the same tag) and unique across
/// calls. This only fires on a degenerate path — it exists so the worker emits a
/// well-formed response instead of panicking. See [`generate_response`].
fn fallback_to_tag(call_id: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    call_id.hash(&mut hasher);
    format!("b2bua-fb-{:016x}", hasher.finish())
}

#[derive(Debug, Clone, Default)]
pub struct GenerateResponseOpts {
    /// Tag added to To when status > 100 and the request's To lacks one.
    pub to_tag: Option<String>,
    pub contact: Option<ContactSpec>,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub extra_headers: Vec<SipHeader>,
    /// Source the request arrived from — stamps `received=` / `rport=` on the
    /// topmost echoed Via (RFC 3261 §18.2.1 + RFC 3581 §4).
    pub incoming_source: Option<(String, u16)>,
}

/// Build a UAS response to `incoming_request`, echoing Via / From / To /
/// Call-ID / CSeq (RFC 3261 §8.2.6.2).
pub fn generate_response(
    incoming_request: &SipRequest,
    status: u16,
    reason: &str,
    opts: &GenerateResponseOpts,
) -> SipResponse {
    let body = opts.body.clone();

    let raw_to = get_header(&incoming_request.headers, "to").unwrap_or("");
    let from = get_header(&incoming_request.headers, "from").unwrap_or("");
    let call_id = get_header(&incoming_request.headers, "call-id").unwrap_or("");
    let cseq = get_header(&incoming_request.headers, "cseq").unwrap_or("");

    // A non-100 response MUST carry a To-tag (RFC 3261 §8.2.6.2); hydrate_response
    // rejects one that doesn't. A To that already has a tag (in-dialog) is echoed;
    // otherwise `opts.to_tag` is added when supplied, else the deterministic
    // fallback — a worker must never panic building a response (that kills the
    // handler task and leaks the dialog).
    let to = if status > 100 && parse_name_addr(&SipStr::owned(raw_to)).tag.is_none() {
        let tag = opts
            .to_tag
            .clone()
            .unwrap_or_else(|| fallback_to_tag(call_id));
        format!("{raw_to};tag={tag}")
    } else {
        raw_to.to_string()
    };

    let mut headers: Vec<SipHeader> = Vec::new();

    // Echo every Via in order; stamp the topmost from `incoming_source`.
    let mut stamped_top_via = false;
    for hdr in &incoming_request.headers {
        if !hdr.name.eq_ignore_ascii_case("via") {
            continue;
        }
        let value = match (&opts.incoming_source, stamped_top_via) {
            (Some((ip, port)), false) => {
                SipStr::owned(&stamp_received_rport_on_via(&hdr.value, ip, *port))
            }
            _ => hdr.value.clone(),
        };
        stamped_top_via = true;
        headers.push(h("Via", value));
    }

    // Echo Record-Route verbatim (RFC 3261 §16.6) — but NOT on a 100 Trying: a
    // 100 establishes no dialog, so its Record-Route is inert (the UAC ignores it)
    // and merely bloats the provisional. Dialog-establishing 18x/2xx still carry it.
    if status != 100 {
        for hdr in &incoming_request.headers {
            if hdr.name.eq_ignore_ascii_case("record-route") {
                headers.push(h("Record-Route", hdr.value.clone()));
            }
        }
    }

    headers.push(h("From", from));
    headers.push(h("To", to));
    headers.push(h("Call-ID", call_id));
    headers.push(h("CSeq", cseq));

    if let Some(contact) = &opts.contact {
        headers.push(h("Contact", contact.header_value()));
    }

    headers.extend(opts.extra_headers.iter().cloned());
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    make_response(status, reason, headers, body)
}
