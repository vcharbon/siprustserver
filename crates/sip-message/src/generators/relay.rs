//! B2BUA relay transparency: the stack-owned structural header set, the
//! non-structural pass-through extractor, and rebuilding a response for the
//! peer leg from snapshotted fields (RFC 3261 §16.6 / §12.1.1).

use super::emit;
use super::spec::ContactSpec;
use crate::draft::{Draft, ResponseDraft, StartKind};
use crate::header::{
    self, CSeq, CallId, HeaderClass, HeaderName, HeaderValue, MediaType, RecordRouteEntry, Via,
};
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

/// The typed twin of the stringly fields of [`GenerateRelayedResponseOpts`]:
/// each value supersedes the text that names the same header, so a relay that
/// already holds parsed values never round-trips them through a string.
#[derive(Debug, Clone, Default)]
pub struct RelayedResponseValues {
    /// Supersedes `vias`.
    pub hops: Vec<Via>,
    /// Supersedes `record_routes`.
    pub record_routes: Vec<RecordRouteEntry>,
    /// Supersedes `from`.
    pub from: Option<header::From>,
    /// Supersedes `to`.
    pub to: Option<header::To>,
    /// Supersedes `call_id`.
    pub call_id: Option<CallId>,
    /// Supersedes `cseq`.
    pub cseq: Option<CSeq>,
    /// Supersedes `contact`.
    pub contact: Option<header::Contact>,
    /// Supersedes `content_type`.
    pub content_type: Option<MediaType>,
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
    /// Typed values, each superseding its stringly counterpart above.
    pub values: RelayedResponseValues,
}

/// Put a list of typed values on the draft, or the text lines they supersede.
fn lines<S: StartKind, H: HeaderValue>(
    mut draft: Draft<S>,
    values: &[H],
    text: &[String],
) -> Draft<S> {
    if values.is_empty() {
        for line in text {
            draft = draft.push_raw(H::header_name(), SipStr::owned(line));
        }
        return draft;
    }
    for value in values {
        draft = draft.push(value.clone());
    }
    draft
}

/// Put one typed value on the draft, or the text line it supersedes.
fn line<S: StartKind, H: HeaderValue>(
    draft: Draft<S>,
    value: &Option<H>,
    text: &str,
) -> Draft<S> {
    match value {
        Some(value) => draft.push(value.clone()),
        None => draft.push_raw(H::header_name(), SipStr::owned(text)),
    }
}

/// Rebuild a B2BUA-side response for relay to a peer leg (RFC 3261 §16.6 /
/// §12.1.1).
pub fn generate_relayed_response(
    status: u16,
    reason: &str,
    opts: &GenerateRelayedResponseOpts,
) -> SipResponse {
    let values = &opts.values;
    let mut draft = ResponseDraft::new(status, SipStr::owned(reason));
    draft = lines(draft, &values.hops, &opts.vias);
    draft = lines(draft, &values.record_routes, &opts.record_routes);
    draft = line(draft, &values.from, &opts.from);
    draft = line(draft, &values.to, &opts.to);
    draft = line(draft, &values.call_id, &opts.call_id);
    draft = line(draft, &values.cseq, &opts.cseq);

    draft = emit::extra_headers(draft, &opts.transparent_headers);

    let contact = values.contact.clone().or_else(|| opts.contact.as_ref().map(ContactSpec::value));
    if let Some(contact) = contact {
        draft = draft.push(contact);
    }

    let content_type = emit::media_type(&values.content_type, &opts.content_type);
    emit::response(emit::framed(draft, opts.body.clone(), content_type))
}
