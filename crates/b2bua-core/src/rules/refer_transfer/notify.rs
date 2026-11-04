//! The RFC 3515 implicit-subscription progress vocabulary: sipfrag NOTIFYs
//! toward the referrer, with the RFC 3265 §3.2.4 Subscription-State fragments.

use call::TransferState;
use sip_message::sipfrag::sipfrag_from_status;

use b2bua_sdk::model::RuleAction;

pub(super) const SUB_STATE_ACTIVE_60: &str = "active;expires=60";
pub(super) const SUB_STATE_TERMINATED_NORESOURCE: &str = "terminated;reason=noresource";
pub(super) const SUB_STATE_TERMINATED_TIMEOUT: &str = "terminated;reason=timeout";
const SIPFRAG_CT: &str = "message/sipfrag;version=2.0";

/// A refer-event NOTIFY toward the referrer carrying `code reason` as a sipfrag
/// status line. The core sends none once the subscription is terminated (RFC
/// 6665 §4.2.2): after a terminating NOTIFY, or a final or timeout that ended
/// it.
pub(super) fn notify(
    st: &TransferState,
    subscription_state: &str,
    code: u16,
    reason: &str,
) -> RuleAction {
    RuleAction::SendNotify {
        leg_id: st.referrer_leg_id.clone(),
        event: st.notify_event.clone(),
        subscription_state: subscription_state.to_string(),
        content_type: Some(SIPFRAG_CT.to_string()),
        body: sipfrag_from_status(code, reason),
    }
}
