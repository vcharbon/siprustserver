//! A timer dies with its scope (ADR-0042, ADR-0055 §10): the setup deadline
//! guards the call's setup, and its fire once the setup is over — a leg holds
//! a confirmed dialog, or the call is terminating — reaches no rule, whatever
//! the ledger holds. The rule that handles the fire checks no stale ledger.

use b2bua_core::turn_driver::{Driver, Step};
use call::{Call, CallModelState, TimerEntry, TimerType};

/// The setup deadline `call`'s ledger holds.
fn setup_deadline(call: &Call) -> Option<TimerEntry> {
    call.timers.iter().find(|t| t.timer_type() == &TimerType::SetupTimeout).cloned()
}

#[test]
fn the_callers_answer_ends_the_setup_deadline() {
    let driver = Driver::new();
    let mut call = driver.dial();
    assert!(setup_deadline(call.call()).is_some(), "the route armed the setup deadline");
    call.run([Step::Answers("b-1", 180), Step::Answers("b-1", 200)]);
    assert!(call::helpers::caller_answered(call.call()), "bob's 2xx answered the caller");
    assert_eq!(setup_deadline(call.call()), None, "the answer took the deadline off the ledger");
}

/// The ledger still holds the deadline the route armed, the call answered —
/// a record no event sequence lands, built here to hold the screen to it: its
/// fire reaches no rule, so a service authoring the setup treatment ends no
/// answered call.
#[test]
fn a_setup_deadline_held_past_the_answer_reaches_no_rule() {
    let driver = Driver::new();
    let mut call = driver.dial();
    let armed = setup_deadline(call.call()).expect("the route armed the setup deadline");
    call.run([Step::Answers("b-1", 180), Step::Answers("b-1", 200)]);
    let mut held = call.call().clone();
    held.timers.push(armed);
    call.revert_to(held);
    let fire = call.turned(Step::Fires(TimerType::SetupTimeout));
    assert!(
        fire.methods_to("a").is_empty() && fire.methods_to("b-1").is_empty(),
        "nothing is sent"
    );
    assert_eq!(fire.rule, None, "no rule reads the fire");
    assert_eq!(call.call().state, CallModelState::Active, "the answered call stays up");
    assert_eq!(call.call().a_leg.invite_final_sent(), Some(200), "no final follows the 2xx");
    assert_eq!(setup_deadline(call.call()), None, "the fired instance left the ledger");
}

/// The same for a deadline held past the start of the termination.
#[test]
fn a_setup_deadline_held_past_the_termination_reaches_no_rule() {
    let driver = Driver::new();
    let mut call = driver.dial();
    let armed = setup_deadline(call.call()).expect("the route armed the setup deadline");
    call.run([Step::Answers("b-1", 180), Step::AliceCancels]);
    assert_eq!(call.call().state, CallModelState::Terminating, "the caller's CANCEL ends the call");
    let final_sent = call.call().a_leg.invite_final_sent();
    let mut held = call.call().clone();
    held.timers.push(armed);
    call.revert_to(held);
    let fire = call.turned(Step::Fires(TimerType::SetupTimeout));
    assert!(
        fire.methods_to("a").is_empty() && fire.methods_to("b-1").is_empty(),
        "nothing is sent"
    );
    assert_eq!(call.call().a_leg.invite_final_sent(), final_sent, "no second final");
}
