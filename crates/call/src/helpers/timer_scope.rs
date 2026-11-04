//! Whether the scope a timer guards has ended on the call as it stands
//! (ADR-0042, ADR-0055 §10): a timer of an ended scope never fires, whatever
//! the ledger holds.

use crate::model::{Call, CallModelState, LegState, TimerKey, TimerScope, TimerType};

use super::{find_attempt, find_leg};

/// Whether the scope `key` guards has ended for good on `call`: its leg is
/// terminated or gone, its attempt ended or gone, or its owning service
/// machine is inactive (for a [`TimerType::Service`] timer). The call's own
/// scope and an obligation's last as long as the call does; the call's setup
/// is read at the fire ([`setup_over`]).
pub fn scope_ended(call: &Call, key: &TimerKey) -> bool {
    let ended = match &key.scope {
        TimerScope::Call | TimerScope::Obligation(_) | TimerScope::Setup => false,
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

/// Whether the call's setup is over as `call` stands ([`TimerScope::Setup`]):
/// a leg, the originator's or any dialled one (a media server's included),
/// holds a confirmed dialog, or the call's termination began.
pub fn setup_over(call: &Call) -> bool {
    std::iter::once(&call.a_leg).chain(call.b_legs.iter()).any(|l| l.state() == LegState::Confirmed)
        || matches!(call.state, CallModelState::Terminating | CallModelState::Terminated)
}
