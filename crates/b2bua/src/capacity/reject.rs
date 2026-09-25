//! The 503 a capacity bound answers a new call with.

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::types::SipHeader;
use sip_message::SipRequest;

/// Build the **503 Service Unavailable** refusing a new call at a memory
/// bound: the INVITE's Via/From/To/Call-ID/CSeq echoed, the caller's
/// `to_tag`, a `Retry-After`, and no `Reason` header.
pub fn build_capacity_reject_503(
    to_tag: String,
    req: &SipRequest,
    retry_after_sec: u32,
) -> sip_message::SipResponse {
    generate_response(
        req,
        503,
        "Service Unavailable",
        &GenerateResponseOpts {
            to_tag: Some(to_tag),
            extra_headers: vec![SipHeader {
                name: "Retry-After".to_string().into(),
                value: retry_after_sec.to_string().into(),
            }],
            ..Default::default()
        },
    )
}
