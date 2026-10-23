//! CANCEL generation for an outstanding INVITE (RFC 3261 §9.1).

use super::emit;
use super::error::GenerateError;
use super::spec::InviteClientTransactionHandle;
use crate::draft::RequestDraft;
use crate::header::{CSeq, ContentLength, HeaderName, MaxForwards};
use crate::method::Method;
use crate::sip_str::SipStr;
use crate::types::{SipHeader, SipRequest};

/// Build a CANCEL for the outstanding INVITE (RFC 3261 §9.1): the CANCEL "MUST
/// have a single Via header field value, and that value MUST equal the top Via
/// header field value of the request being cancelled" — copied verbatim (same
/// branch, the transaction-correlation key); Request-URI / Call-ID / From / To
/// echo the INVITE; CSeq number reused with method CANCEL. §9.1 also requires
/// the CANCEL to be routed the SAME way as the INVITE — "the Route header fields
/// of the CANCEL … MUST equal" the INVITE's — so its Route set is reproduced
/// verbatim (load-bearing when the INVITE carried a preloaded outbound-proxy
/// Route: without it the CANCEL bypasses the proxy the INVITE traversed, and the
/// cross-message audit flags `cancel-route-echoes-invite`). An INVITE missing a
/// header the CANCEL echoes is a [`GenerateError::Missing`].
///
/// `extra_headers` ride after the generator's own, in the order given: RFC 3326
/// §2 scopes `Reason` to CANCEL and BYE, so a back-to-back UA cancelling on a
/// peer's behalf restates here what that peer said about the cancellation.
pub fn generate_cancel(
    invite_txn: &InviteClientTransactionHandle,
    extra_headers: &[SipHeader],
) -> Result<SipRequest, GenerateError> {
    let invite = &invite_txn.original_invite;
    let echoed = |name: HeaderName, input: &'static str| -> Result<SipStr, GenerateError> {
        invite.raw_text(name).next().ok_or(GenerateError::Missing(input))
    };

    let mut draft = RequestDraft::new(Method::Cancel, invite.request_uri().clone())
        .push_raw(HeaderName::Via, echoed(HeaderName::Via, "the INVITE's Via")?)
        .push(MaxForwards::DEFAULT)
        .push_raw(HeaderName::From, echoed(HeaderName::From, "the INVITE's From")?)
        .push_raw(HeaderName::To, echoed(HeaderName::To, "the INVITE's To")?)
        .push_raw(HeaderName::CallId, echoed(HeaderName::CallId, "the INVITE's Call-ID")?)
        .push(CSeq::new(invite.cseq().seq(), Method::Cancel));
    for route in invite.raw_text(HeaderName::Route) {
        draft = draft.push_raw(HeaderName::Route, route);
    }
    draft = emit::extra_headers(draft, extra_headers);

    emit::request(draft.push(ContentLength::new(0)))
}
