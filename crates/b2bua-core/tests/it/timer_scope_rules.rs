//! A timer dies with its scope (ADR-0042, ADR-0055 §10): the setup deadline
//! guards the call's setup, and its fire once the setup is over — a leg holds
//! a confirmed dialog, or the call is terminating — reaches no rule, whatever
//! the ledger holds. The rule that handles the fire checks no stale ledger.

use call::{Call, CallModelState, TimerEntry, TimerType};

use crate::turn_lane::drive::{dialled, run, run_on, Dialled, Step};

/// The setup deadline `call`'s ledger holds.
fn setup_deadline(call: &Call) -> Option<TimerEntry> {
    call.timers.iter().find(|t| t.timer_type() == &TimerType::SetupTimeout).cloned()
}

/// The dialled call after `script`, and the setup deadline the route armed.
fn after(script: &[Step]) -> (Dialled, TimerEntry, Call) {
    let dialled = dialled();
    let armed = setup_deadline(&dialled.call).expect("the route armed the setup deadline");
    let call = run(&dialled, script, usize::MAX, &[]).call;
    (dialled, armed, call)
}

#[test]
fn the_callers_answer_ends_the_setup_deadline() {
    let (_, _, call) = after(&[Step::BobAnswers(180), Step::BobAnswers(200)]);
    assert!(call::helpers::caller_answered(&call), "bob's 2xx answered the caller");
    assert_eq!(setup_deadline(&call), None, "the answer took the deadline off the ledger");
}

/// The ledger still holds the deadline the route armed, the call answered:
/// its fire reaches no rule, so a service authoring the setup treatment ends
/// no answered call.
#[test]
fn a_setup_deadline_held_past_the_answer_reaches_no_rule() {
    let (dialled, armed, mut call) = after(&[Step::BobAnswers(180), Step::BobAnswers(200)]);
    call.timers.push(armed);
    let fired = run_on(&dialled, call, &[Step::Fires(TimerType::SetupTimeout)]);
    assert_eq!(fired.turns.len(), 1, "the fire takes a turn");
    assert!(fired.sent.is_empty(), "nothing is sent: {:?}", fired.sent);
    assert_eq!(fired.call.state, CallModelState::Active, "the answered call stays up");
    assert_eq!(fired.call.a_leg.invite_final_sent(), Some(200), "no final follows the 2xx");
    assert_eq!(setup_deadline(&fired.call), None, "the fired instance left the ledger");
}

/// The same for a deadline held past the start of the termination.
#[test]
fn a_setup_deadline_held_past_the_termination_reaches_no_rule() {
    let (dialled, armed, mut call) = after(&[Step::BobAnswers(180), Step::AliceCancels]);
    assert_eq!(call.state, CallModelState::Terminating, "the caller's CANCEL ends the call");
    let final_sent = call.a_leg.invite_final_sent();
    call.timers.push(armed);
    let fired = run_on(&dialled, call, &[Step::Fires(TimerType::SetupTimeout)]);
    assert_eq!(fired.turns.len(), 1, "the fire takes a turn");
    assert!(fired.sent.is_empty(), "nothing is sent: {:?}", fired.sent);
    assert_eq!(fired.call.a_leg.invite_final_sent(), final_sent, "no second final");
}
