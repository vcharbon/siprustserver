//! The caller's INVITE, rebuilt from the call's snapshot of it by the step
//! that reads it (ADR-0052 §11).

use sip_message::draft::{IncompleteDraft, RequestDraft};
use sip_message::header::{HeaderName, MaxForwards, Uri};
use sip_message::{Method, SipRequest, SipStr};

use crate::model::{RuleContext, RuleHandleResult};

/// Rebuild the a-leg's original INVITE as a `SipRequest`. Every header rides
/// as an unparsed line, so the rebuilt message carries the caller's bytes
/// exactly as they arrived. A caller that omitted `Max-Forwards` gets RFC 3261
/// §8.1.1.6's default, which the transfer leg originated from this rebuild
/// then carries.
///
/// The Request-URI is read verbatim, not refused: this is a round-trip of text
/// the inbound parser's own strict gates already admitted. Nothing routes on
/// it — the b-leg's Request-URI comes from the decision.
pub fn rebuild(snap: &call::ALegInviteSnapshot) -> Result<SipRequest, IncompleteDraft> {
    let mut draft =
        RequestDraft::new(Method::Invite, Uri::parse_or_verbatim(&SipStr::owned(&snap.uri)));
    for h in &snap.headers {
        draft = draft.push_raw(HeaderName::from(h.name.as_str()), SipStr::owned(&h.value));
    }
    if !draft.has(&HeaderName::MaxForwards) {
        draft = draft.push(MaxForwards::DEFAULT);
    }
    draft.with_body(snap.body.clone().into()).freeze()
}

impl RuleContext<'_> {
    /// The caller's INVITE, rebuilt from the call's record. `None` is a record
    /// that does not rebuild it, which the rule answers with
    /// [`RuleHandleResult::unrecoverable`].
    pub fn caller_invite(&self) -> Option<SipRequest> {
        rebuild(self.call.a_leg_invite()).ok()
    }
}

impl RuleHandleResult {
    /// What a rule returns when the call's record does not rebuild the
    /// caller's INVITE it reads: no action, and the engine takes the call as
    /// unrecoverable.
    pub fn unrecoverable() -> Self {
        Self { unrecoverable: true, ..Self::default() }
    }
}
