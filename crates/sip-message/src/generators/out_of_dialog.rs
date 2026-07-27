//! Out-of-dialog request generation: initial INVITE, one-shot OPTIONS,
//! MESSAGE, REGISTER, SUBSCRIBE, PUBLISH (RFC 3261 §8.1.1).

use super::emit;
use super::methods::OutOfDialogMethod;
use crate::draft::RequestDraft;
use crate::header::{self, CSeq, CallId, MaxForwards, MediaType, Uri, Via};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest};

/// Inputs for [`generate_out_of_dialog_request`]. Every header is a typed
/// value; the recipe adds only what §8.1.1 makes mandatory and never re-parses
/// what the caller supplies.
#[derive(Debug, Clone, Default)]
pub struct GenerateOutOfDialogRequestOpts {
    /// Request-URI. Required.
    pub request_uri: Option<Uri>,
    pub call_id: String,
    /// From, tag included. Required.
    pub from: Option<header::From>,
    /// To — tagged only for a request that targets an existing dialog. Required.
    pub to: Option<header::To>,
    pub cseq: u32,
    /// This hop's own Via. Required.
    pub via: Option<Via>,
    /// Contact. Required for a dialog-forming request (§8.1.1.8).
    pub contact: Option<header::Contact>,
    pub max_forwards: Option<u32>,
    pub body: Vec<u8>,
    pub content_type: Option<MediaType>,
    /// Caller-stated header lines, carried verbatim (name spelling included).
    pub extra_headers: Vec<SipHeader>,
}

/// Build an out-of-dialog request (RFC 3261 §8.1.1).
pub fn generate_out_of_dialog_request(
    method: OutOfDialogMethod,
    opts: &GenerateOutOfDialogRequestOpts,
) -> SipRequest {
    let uri = opts.request_uri.clone().expect("request_uri required");
    let hop = opts.via.clone().expect("via required");
    let contact = opts.contact.clone().expect("contact required");
    let from = opts.from.clone().expect("from required");
    let to = opts.to.clone().expect("to required");
    let verb = Method::from(method);

    let draft = RequestDraft::new(verb.clone(), uri)
        .push(hop)
        .push(MaxForwards::new(opts.max_forwards.unwrap_or(emit::DEFAULT_MAX_FORWARDS)))
        .push(from)
        .push(to)
        .push(CallId::new(SipStr::owned(&opts.call_id)))
        .push(CSeq::new(opts.cseq, verb))
        .push(contact);

    let draft = emit::extra_headers(draft, &opts.extra_headers);
    emit::request(emit::framed(draft, opts.body.clone(), opts.content_type.clone()))
}
