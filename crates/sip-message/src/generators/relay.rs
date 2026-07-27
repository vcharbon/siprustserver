//! B2BUA relay transparency: the stack-owned structural header set, the
//! non-structural pass-through extractor, and rebuilding a response for the
//! peer leg from snapshotted fields (RFC 3261 §16.6 / §12.1.1).

use super::emit;
use crate::draft::{Entry, ResponseDraft};
use crate::header::{self, HeaderClass, HeaderName, MediaType};
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipMessage, SipResponse};

/// True iff the relay owns this header rather than copying the peer's (RFC 3261
/// §16.6): the stack-owned structural set, plus `Content-Type` — the relay
/// emits its own body, so the media type describing it is the relay's to state.
fn relay_owns(name: &str) -> bool {
    HeaderName::class_of(name) == HeaderClass::Structural
        || HeaderName::known(name) == Some(HeaderName::ContentType)
}

/// Every header from `msg` the relay does NOT own — callers pass the result
/// through `extra_headers` when relaying so transparent fields (Allow,
/// Supported, P-Asserted-Identity, …) flow through unchanged while the
/// generator owns the dialog headers.
pub fn extract_non_structural_headers(msg: &SipMessage) -> Vec<SipHeader> {
    msg.headers().iter().filter(|hdr| !relay_owns(&hdr.name)).cloned().collect()
}

/// Inputs for [`generate_relayed_response`]. RFC 3261 §8.2.6.2 makes Via /
/// From / To / Call-ID / CSeq echoes of the request being answered, so each is
/// a draft [`Entry`]: a relay holding the peer's own bytes passes
/// [`Entry::raw`] and they are memcpy'd through untouched; a relay holding a
/// parsed value passes [`Entry::typed`] and it renders once.
#[derive(Debug, Clone, Default)]
pub struct GenerateRelayedResponseOpts {
    /// Via lines from the target-facing request, echoed in order. Required.
    pub vias: Vec<Entry>,
    /// From of the request being answered. Required.
    pub from: Option<Entry>,
    /// To of the request being answered, tagged. Required.
    pub to: Option<Entry>,
    /// Call-ID of the request being answered. Required.
    pub call_id: Option<Entry>,
    /// CSeq of the request being answered. Required.
    pub cseq: Option<Entry>,
    pub body: Vec<u8>,
    pub content_type: Option<MediaType>,
    /// Non-structural headers carried through from the source response (§16.6).
    pub transparent_headers: Vec<SipHeader>,
    /// Record-Route headers reflected in received order.
    pub record_routes: Vec<Entry>,
    pub contact: Option<header::Contact>,
}

/// Put an echoed header on the draft, naming the header it must carry so a
/// missing input fails at the freeze gate rather than silently.
fn echo(draft: ResponseDraft, entry: &Option<Entry>, name: HeaderName) -> ResponseDraft {
    match entry {
        Some(entry) => draft.push_entry(entry.clone()),
        None => draft.push_raw(name, SipStr::EMPTY),
    }
}

/// Rebuild a B2BUA-side response for relay to a peer leg (RFC 3261 §16.6 /
/// §12.1.1).
pub fn generate_relayed_response(
    status: u16,
    reason: &str,
    opts: &GenerateRelayedResponseOpts,
) -> SipResponse {
    let mut draft = ResponseDraft::new(status, SipStr::owned(reason));
    for entry in opts.vias.iter().chain(&opts.record_routes) {
        draft = draft.push_entry(entry.clone());
    }
    draft = echo(draft, &opts.from, HeaderName::From);
    draft = echo(draft, &opts.to, HeaderName::To);
    draft = echo(draft, &opts.call_id, HeaderName::CallId);
    draft = echo(draft, &opts.cseq, HeaderName::CSeq);

    draft = emit::extra_headers(draft, &opts.transparent_headers);

    if let Some(contact) = opts.contact.clone() {
        draft = draft.push(contact);
    }

    emit::response(emit::framed(draft, opts.body.clone(), opts.content_type.clone()))
}
