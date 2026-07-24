//! Out-of-dialog request generation: initial INVITE, one-shot OPTIONS,
//! MESSAGE, REGISTER, SUBSCRIBE, PUBLISH (RFC 3261 §8.1.1).

use super::emit::{append_body_headers, h, make_request, wrap_uri};
use super::methods::OutOfDialogMethod;
use super::spec::{ContactSpec, ViaSpec};
use crate::types::{SipHeader, SipRequest};

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
}

/// Build an out-of-dialog request (RFC 3261 §8.1.1).
pub fn generate_out_of_dialog_request(
    method: OutOfDialogMethod,
    opts: &GenerateOutOfDialogRequestOpts,
) -> SipRequest {
    let body = opts.body.clone();
    let max_forwards = opts.max_forwards.unwrap_or(70);
    let via = opts.via.as_ref().expect("ViaSpec required");
    let contact = opts.contact.as_ref().expect("ContactSpec required");

    let to_value = match &opts.to_tag {
        Some(tag) => format!("{};tag={}", wrap_uri(&opts.to_uri), tag),
        None => wrap_uri(&opts.to_uri),
    };

    let mut headers: Vec<SipHeader> = vec![
        h("Via", via.header_value()),
        h("Max-Forwards", max_forwards.to_string()),
        h("From", format!("{};tag={}", wrap_uri(&opts.from_uri), opts.from_tag)),
        h("To", to_value),
        h("Call-ID", opts.call_id.clone()),
        h("CSeq", format!("{} {}", opts.cseq, method.as_str())),
        h("Contact", contact.header_value()),
    ];
    headers.extend(opts.extra_headers.iter().cloned());
    append_body_headers(&mut headers, &body, opts.content_type.as_deref());

    make_request(method.as_str(), &opts.request_uri, headers, body)
}
