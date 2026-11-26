//! The methods `SendRequestToLeg` sends
//! ([`b2bua_sdk::model::RuleAction::SendRequestToLeg`]), and the defect a turn
//! reports for any other.

use call::Call;
use sip_message::generators::InDialogMethod;
use sip_message::Method;

use super::ActionExecutor;

/// Whether `SendRequestToLeg` sends `m`: a method whose request keeps the
/// records this action writes. A NOTIFY rides its subscription (RFC 6665
/// §8.1.2), a PRACK the reliable provisional it acknowledges (RFC 3262 §7.2),
/// a REFER the subscription it creates (RFC 3515 §2.4.4).
pub(super) fn sent_to_a_leg(m: &InDialogMethod) -> bool {
    match m {
        InDialogMethod::Bye
        | InDialogMethod::Invite
        | InDialogMethod::Options
        | InDialogMethod::Info
        | InDialogMethod::Update
        | InDialogMethod::Message => true,
        InDialogMethod::Notify | InDialogMethod::Prack | InDialogMethod::Refer => false,
    }
}

/// Why `SendRequestToLeg` does not send `method`, and what does.
fn refused_why(method: &str) -> &'static str {
    match Method::from_wire(method) {
        Method::Notify => "a NOTIFY rides its subscription (SendNotify)",
        Method::Prack => "a PRACK is the stack's own, to each reliable provisional",
        Method::Refer => "this stack originates no REFER",
        _ => "no in-dialog request this action sends",
    }
}

impl ActionExecutor<'_> {
    /// The turn reports that the rule asked a `method` request toward `leg_id`
    /// that `SendRequestToLeg` does not send
    /// ([`REQUEST_TO_LEG_METHOD`](crate::turn_notes::REQUEST_TO_LEG_METHOD)).
    pub(super) fn method_not_sent(&mut self, call: &Call, leg_id: &str, method: &str) {
        let rule = self.turn.rule.unwrap_or("(none)");
        let detail =
            format!("rule '{rule}': {method} to leg {leg_id} not sent: {}", refused_why(method));
        self.turn.notes.defect(crate::turn_notes::REQUEST_TO_LEG_METHOD, detail, call);
    }
}
