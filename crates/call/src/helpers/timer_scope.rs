//! Whether the scope a timer guards has ended on the call as it stands
//! (ADR-0042, ADR-0055 §10): a timer of an ended scope never fires, whatever
//! the ledger holds.

use crate::model::{Call, CallModelState, LegState, TimerKey, TimerScope, TimerType};

use super::{find_attempt, find_leg};

/// Whether the scope `key` guards has ended on `call`: its leg is terminated
/// or gone, its attempt ended or gone, the caller's INVITE has its final or
/// the call's termination began (for [`TimerScope::UntilAnswered`]), or its
/// owning service machine is inactive (for a [`TimerType::Service`] timer).
/// The call's own scope and an obligation's last as long as the call does.
pub fn scope_ended(call: &Call, key: &TimerKey) -> bool {
    let ended = match &key.scope {
        TimerScope::Call | TimerScope::Obligation(_) => false,
        TimerScope::UntilAnswered => {
            call.a_leg.invite_final_sent.is_some()
                || matches!(call.state, CallModelState::Terminating | CallModelState::Terminated)
        }
        TimerScope::Attempt(id) => find_attempt(call, *id).is_none_or(|a| !a.is_open()),
        TimerScope::Leg(leg_id) => {
            find_leg(call, leg_id).is_none_or(|l| l.state() == LegState::Terminated)
        }
    };
    ended
        || matches!(
            &key.timer_type,
            TimerType::Service { service_id, .. } if !call.sm_cursors.contains_key(service_id)
        )
}
