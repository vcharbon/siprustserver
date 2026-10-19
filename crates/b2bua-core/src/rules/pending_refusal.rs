//! The refusal of an in-dialog INVITE or UPDATE that a treatment holds off
//! while it is in flight (a release reroute, a promotion window).

use b2bua_sdk::model::{RuleAction, RuleContext};

/// The §14.2 / RFC 3311 §5.2 answer where an offer of the sender's is open on
/// the dialog ([`RuleContext::offer_refusal`]), else 491 Request Pending
/// (RFC 5407 §3.1: retry once the treatment settles).
pub(crate) fn refuse_pending(ctx: &RuleContext) -> RuleAction {
    match ctx.offer_refusal() {
        Some(refusal) => RuleAction::RefuseGlare { refusal },
        None => RuleAction::Respond {
            status: 491,
            reason: "Request Pending".into(),
            body: vec![],
            content_type: None,
        },
    }
}
