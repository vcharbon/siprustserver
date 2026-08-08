//! The RFC 3515 implicit-subscription progress vocabulary: sipfrag NOTIFYs
//! toward the referrer, with the RFC 3265 §3.2.4 Subscription-State fragments.

use sip_message::sipfrag::sipfrag_from_status;

use crate::rules::model::RuleAction;

pub(super) const SUB_STATE_ACTIVE_60: &str = "active;expires=60";
pub(super) const SUB_STATE_TERMINATED_NORESOURCE: &str = "terminated;reason=noresource";
pub(super) const SUB_STATE_TERMINATED_TIMEOUT: &str = "terminated;reason=timeout";
const SIPFRAG_CT: &str = "message/sipfrag;version=2.0";

/// A refer-event NOTIFY carrying `code reason` as a sipfrag status line.
pub(super) fn notify(
    leg_id: &str,
    subscription_state: &str,
    code: u16,
    reason: &str,
) -> RuleAction {
    RuleAction::SendNotify {
        leg_id: leg_id.to_string(),
        event: "refer".to_string(),
        subscription_state: subscription_state.to_string(),
        content_type: Some(SIPFRAG_CT.to_string()),
        body: sipfrag_from_status(code, reason),
    }
}
