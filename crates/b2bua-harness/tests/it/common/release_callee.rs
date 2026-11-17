//! A service that releases the callee leg `b-1` on a deadline
//! ([`AT_SEC`] seconds into the call), the caller's leg staying up.

use b2bua::rules::{
    Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
    ServiceSeed, TimerDelay,
};
use b2bua::{define_service, sm_rule};
use call::{TimerKey, TimerType};
use sip_message::Method;

/// The release deadline, in seconds from the call's start: past the 32 s a
/// non-INVITE server transaction answered at the start holds its final
/// (Timer J, RFC 3261 §17.2.2), so anything the release sends on it reaches
/// the wire.
pub const AT_SEC: i64 = 40;
const DEADLINE: TimerType = TimerType::service(RELEASE, "deadline");

define_service! {
    id: "release-callee",
    machine: RELEASE,
    states: ReleaseState { Armed, Released },
    init: |_call: &RuleCall| {
        Some(ServiceSeed::new(ReleaseState::Armed.label()).with_actions(vec![
            RuleAction::arm(TimerKey::call(DEADLINE), TimerDelay::secs(AT_SEC)),
        ]))
    },
    rules: [ release_on_deadline() ],
}

fn release_on_deadline() -> RuleDefinition {
    sm_rule! {
        id: "release-callee-on-deadline",
        machine: RELEASE,
        active: [ ReleaseState::Armed ],
        transitions: [ ReleaseState::Armed => ReleaseState::Released ],
        effects: [ Effect::Originate { method: Method::Bye, label: "BYE → the callee" } ],
        matcher: Match::timer().timer_type(DEADLINE),
        handle: |_ctx: &RuleContext| {
            Some(RuleHandleResult::new(vec![
                RuleAction::DestroyLeg { leg_id: "b-1".into(), headers: vec![] },
                RuleAction::SetState { machine: RELEASE, to: ReleaseState::Released.label() },
            ]))
        },
    }
}
