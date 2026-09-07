//! CANCEL generation for an outstanding INVITE (RFC 3261 §9.1).

use super::emit;
use super::spec::InviteClientTransactionHandle;
use crate::draft::RequestDraft;
use crate::header::{CSeq, ContentLength, HeaderName, MaxForwards};
use crate::method::Method;
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
/// cross-message audit flags `cancel-route-echoes-invite`). Panics when the INVITE
/// is missing a required header.
///
/// `extra_headers` ride after the generator's own, in the order given: RFC 3326
/// §2 scopes `Reason` to CANCEL and BYE, so a back-to-back UA cancelling on a
/// peer's behalf restates here what that peer said about the cancellation.
pub fn generate_cancel(
    invite_txn: &InviteClientTransactionHandle,
    extra_headers: &[SipHeader],
) -> SipRequest {
    let invite = &invite_txn.original_invite;
    let echoed = |name: HeaderName| {
        invite
            .raw_text(name.clone())
            .next()
            .unwrap_or_else(|| panic!("generate_cancel: INVITE missing {name}"))
    };

    let mut draft = RequestDraft::new(Method::Cancel, invite.request_uri().clone())
        .push_raw(HeaderName::Via, echoed(HeaderName::Via))
        .push(MaxForwards::DEFAULT)
        .push_raw(HeaderName::From, echoed(HeaderName::From))
        .push_raw(HeaderName::To, echoed(HeaderName::To))
        .push_raw(HeaderName::CallId, echoed(HeaderName::CallId))
        .push(CSeq::new(invite.cseq().seq(), Method::Cancel));
    for route in invite.raw_text(HeaderName::Route) {
        draft = draft.push_raw(HeaderName::Route, route);
    }
    draft = emit::extra_headers(draft, extra_headers);

    emit::request(draft.push(ContentLength::new(0)))
}
