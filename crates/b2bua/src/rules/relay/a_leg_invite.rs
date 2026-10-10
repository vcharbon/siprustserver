//! [`rebuild_a_leg_invite`]: the caller's INVITE, rebuilt from the call's
//! snapshot of it.

use sip_message::draft::RequestDraft;
use sip_message::header::{HeaderName, MaxForwards, Uri};
use sip_message::{Method, SipRequest, SipStr};

/// Rebuild the a-leg's original INVITE as a `SipRequest` (for `generate_response`).
/// Every header rides as an unparsed line, so the rebuilt message carries the
/// caller's bytes exactly as they arrived. A caller that omitted `Max-Forwards`
/// gets RFC 3261 §8.1.1.6's default, which is the count [`super::build_b_leg`] then
/// carries onto the transfer leg it originates from this rebuild.
///
/// The Request-URI is read verbatim, not refused: this is a round-trip of text
/// the inbound parser's own strict gates already admitted, so a refusal here
/// would drop a call the stack accepted. Nothing routes on it — the b-leg's
/// Request-URI comes from the decision, through [`super::build_b_leg`]'s reader.
pub fn rebuild_a_leg_invite(snap: &call::ALegInviteSnapshot) -> SipRequest {
    let mut draft =
        RequestDraft::new(Method::Invite, Uri::parse_or_verbatim(&SipStr::owned(&snap.uri)));
    for h in &snap.headers {
        draft = draft.push_raw(HeaderName::from(h.name.as_str()), SipStr::owned(&h.value));
    }
    if !draft.has(&HeaderName::MaxForwards) {
        draft = draft.push(MaxForwards::DEFAULT);
    }
    draft
        .with_body(snap.body.clone().into())
        .freeze()
        .expect("a-leg INVITE snapshot is well-formed")
}
