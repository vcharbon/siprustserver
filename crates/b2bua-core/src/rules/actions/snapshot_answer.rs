//! A response to a relayed request's originator, built from the snapshot the
//! pending-request correlation holds: where it is routed, the headers it
//! states, and the stack's own answer on that transaction.

use call::RequestRecord;
use sip_message::draft::Entry;
use sip_message::generators::{self, GenerateError, GenerateRelayedResponseOpts};
use sip_message::header::{HeaderName, HeaderValue, MediaType, Via};
use sip_message::{SipHeader, SipResponse, SipStr};

/// The headers a response answering the relayed request `pending` states for
/// its originator (RFC 3261 §8.2.6.2): Via / From / To / Call-ID / CSeq equal
/// the originator's own, and the snapshot holds the originator's own bytes
/// (the `call` crate stores text, ADR-0008) — so each rides as a raw entry and
/// reaches the wire unaltered.
pub(super) fn snapshot_response_opts(
    pending: &RequestRecord,
    cseq_method: &str,
    body: Vec<u8>,
    content_type: Option<MediaType>,
    transparent_headers: Vec<SipHeader>,
    contact: Option<sip_message::header::Contact>,
) -> GenerateRelayedResponseOpts {
    let echo = |name: HeaderName, text: &str| Entry::raw(name, SipStr::owned(text));
    GenerateRelayedResponseOpts {
        vias: pending.source_vias.iter().map(|v| echo(HeaderName::Via, v)).collect(),
        record_routes: vec![],
        from: Some(echo(HeaderName::From, &pending.source_from)),
        to: Some(echo(HeaderName::To, &pending.source_to)),
        call_id: Some(echo(HeaderName::CallId, &pending.source_call_id)),
        cseq: Some(echo(HeaderName::CSeq, &format!("{} {}", pending.inbound_cseq, cseq_method))),
        body,
        transparent_headers,
        content_type,
        contact,
    }
}

/// The stack's own `status` answer to the relayed request `pending`: its
/// snapshot's headers and nothing else — no body, no relayed header, no
/// Contact. An originator whose snapshot no response can echo draws none.
pub(super) fn stack_answer(
    pending: &RequestRecord,
    cseq_method: &str,
    status: u16,
    reason: &str,
) -> Result<SipResponse, GenerateError> {
    let opts = snapshot_response_opts(pending, cseq_method, Vec::new(), None, Vec::new(), None);
    generators::generate_relayed_response(status, reason, &opts)
}

/// The sent-by a response to this hop is routed to (RFC 3261 §18.2.2). The
/// snapshot the pending-request correlation holds is text (the `call` crate has
/// no sip-message dependency), so reading one back is a parse.
pub(super) fn via_sent_by(via: &str) -> Option<(String, u16)> {
    let hop = Via::parse(&SipStr::owned(via)).ok()?;
    let (host, port) = hop.sent_by().pair();
    Some((host.to_string(), port))
}
