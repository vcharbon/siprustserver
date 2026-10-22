//! The stateless 400 answering a request the parser refused (RFC 3261 §8.2.6,
//! §16.3): built from the refused request's [`RequestPartial`], never from a
//! parsed message.

use super::emit;
use super::error::GenerateError;
use super::response::{echo_to, echo_via};
use crate::draft::ResponseDraft;
use crate::header::{warning_value, HeaderName};
use crate::parser::custom::partial::RequestPartial;
use crate::sip_str::SipStr;
use crate::types::SipResponse;

/// The status of the answer to a refused request.
pub const INVALID_REQUEST_STATUS: u16 = 400;

/// The reason phrase of the answer to a refused request.
pub const INVALID_REQUEST_REASON: &str = "Invalid Request";

/// The warn-code a refusal's Warning carries: miscellaneous warning
/// (RFC 3261 §20.43).
const MISCELLANEOUS_WARNING: u16 = 399;

/// `400 Invalid Request` to the request `partial` holds: Via, From, To,
/// Call-ID and CSeq echoed as received, `to_tag` added to a To that has none
/// (§8.2.6.2), and `Warning: 399 <warn_agent> "<reason>"` naming the refusal.
pub fn generate_invalid_request(
    partial: &RequestPartial,
    reason: &str,
    warn_agent: &str,
    to_tag: &str,
) -> Result<SipResponse, GenerateError> {
    let mut draft =
        ResponseDraft::new(INVALID_REQUEST_STATUS, SipStr::from_static(INVALID_REQUEST_REASON));
    for via in partial.via_lines() {
        draft = echo_via(draft, via.clone(), None);
    }
    draft = draft.push_raw(HeaderName::From, partial.from_line().clone());
    draft = echo_to(
        draft,
        partial.to_line().clone(),
        INVALID_REQUEST_STATUS,
        partial.call_id_line().as_str(),
        Some(to_tag),
    );
    draft = draft
        .push_raw(HeaderName::CallId, partial.call_id_line().clone())
        .push_raw(HeaderName::CSeq, partial.cseq_line().clone())
        .push_raw(
            HeaderName::Warning,
            SipStr::owned(&warning_value(MISCELLANEOUS_WARNING, warn_agent, reason)),
        );
    emit::response(emit::framed(draft, Vec::new(), None))
}
