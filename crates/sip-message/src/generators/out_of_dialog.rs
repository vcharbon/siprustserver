//! Out-of-dialog request generation: initial INVITE, one-shot OPTIONS,
//! MESSAGE, REGISTER, SUBSCRIBE, PUBLISH (RFC 3261 §8.1.1).

use super::emit;
use super::methods::OutOfDialogMethod;
use super::spec::{ContactSpec, ViaSpec};
use crate::draft::RequestDraft;
use crate::header::{self, CSeq, CallId, HeaderName, MaxForwards, MediaType, Uri, Via};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest};

/// The typed twin of the stringly fields of
/// [`GenerateOutOfDialogRequestOpts`]: each value supersedes the text that
/// names the same header, and reaches the wire without a re-parse.
#[derive(Debug, Clone, Default)]
pub struct OutOfDialogValues {
    /// Supersedes `request_uri`.
    pub uri: Option<Uri>,
    /// Supersedes `from_uri` + `from_tag`.
    pub from: Option<header::From>,
    /// Supersedes `to_uri` + `to_tag`.
    pub to: Option<header::To>,
    /// Supersedes `via`.
    pub hop: Option<Via>,
    /// Supersedes `contact`.
    pub contact: Option<header::Contact>,
    /// Supersedes `content_type`.
    pub content_type: Option<MediaType>,
}

#[derive(Debug, Clone, Default)]
pub struct GenerateOutOfDialogRequestOpts {
    pub request_uri: String,
    pub call_id: String,
    pub from_uri: String,
    pub from_tag: String,
    pub to_uri: String,
    pub to_tag: Option<String>,
    pub cseq: u32,
    pub via: Option<ViaSpec>,
    pub contact: Option<ContactSpec>,
    pub max_forwards: Option<u32>,
    pub body: Vec<u8>,
    pub content_type: Option<String>,
    pub extra_headers: Vec<SipHeader>,
    /// Typed values, each superseding its stringly counterpart above.
    pub values: OutOfDialogValues,
}

/// Build an out-of-dialog request (RFC 3261 §8.1.1).
pub fn generate_out_of_dialog_request(
    method: OutOfDialogMethod,
    opts: &GenerateOutOfDialogRequestOpts,
) -> SipRequest {
    let values = &opts.values;
    let uri = values.uri.clone().unwrap_or_else(|| emit::uri(&opts.request_uri));
    let hop =
        values.hop.clone().unwrap_or_else(|| opts.via.as_ref().expect("ViaSpec required").value());
    let contact = values
        .contact
        .clone()
        .unwrap_or_else(|| opts.contact.as_ref().expect("ContactSpec required").value());
    let verb = Method::from(method);

    let mut draft = RequestDraft::new(verb.clone(), uri)
        .push(hop)
        .push(MaxForwards::new(opts.max_forwards.unwrap_or(emit::DEFAULT_MAX_FORWARDS)));

    draft = match &values.from {
        Some(from) => draft.push(from.clone()),
        None => draft
            .push_raw(HeaderName::From, emit::name_addr_text(&opts.from_uri, Some(&opts.from_tag))),
    };
    draft = match &values.to {
        Some(to) => draft.push(to.clone()),
        None => draft
            .push_raw(HeaderName::To, emit::name_addr_text(&opts.to_uri, opts.to_tag.as_deref())),
    };

    draft = draft
        .push(CallId::new(SipStr::owned(&opts.call_id)))
        .push(CSeq::new(opts.cseq, verb))
        .push(contact);

    draft = emit::extra_headers(draft, &opts.extra_headers);
    let content_type = emit::media_type(&values.content_type, &opts.content_type);
    emit::request(emit::framed(draft, opts.body.clone(), content_type))
}
