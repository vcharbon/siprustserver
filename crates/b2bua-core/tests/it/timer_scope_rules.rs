//! A timer dies with its scope (ADR-0042, ADR-0055 §10): the setup deadline
//! guards the caller until its INVITE is answered, and a fire whose scope
//! ended reaches no rule, whatever the ledger holds. A rule that handles the
//! fire checks no stale ledger.

use b2bua_core::rule_chain::timer_screen;
use b2bua_core::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;
use call::{Call, TimerEntry, TimerType};
use sip_ids::TurnIds;

use crate::turn_lane::drive::{dialled, run, Step, NOW};

/// The setup deadline `call`'s ledger holds.
fn setup_deadline(call: &Call) -> Option<TimerEntry> {
    call.timers.iter().find(|t| t.timer_type() == &TimerType::SetupTimeout).cloned()
}

/// bob rings, then answers: the caller is answered.
fn answered() -> (TimerEntry, Call) {
    let dialled = dialled();
    let armed = setup_deadline(&dialled.call).expect("the route armed the setup deadline");
    let script = [Step::BobAnswers(180), Step::BobAnswers(200)];
    let call = run(&dialled, &script, usize::MAX, &[]).call;
    assert!(call::helpers::caller_answered(&call), "bob's 2xx answered the caller");
    (armed, call)
}

#[test]
fn the_callers_answer_ends_the_setup_deadline() {
    let (_, call) = answered();
    assert_eq!(setup_deadline(&call), None, "the answer took the deadline off the ledger");
}

/// The ledger still holds the deadline the route armed, the caller answered:
/// its fire reaches no rule, so a service authoring the setup treatment
/// answers no answered caller.
#[test]
fn a_setup_deadline_held_past_the_answer_reaches_no_rule() {
    let (armed, mut call) = answered();
    call.timers.push(armed.clone());
    let fire = CallEvent::Timer {
        key: armed.key,
        generation: armed.generation,
        call_ref: call.call_ref.clone(),
        incarnation: Some(call.incarnation().to_string()),
    };
    let mut turn = TurnScope::of(TurnIds::fixed(9));
    let screened = timer_screen::screen(&mut call, &fire, NOW, &mut turn.notes);
    assert!(
        screened.is_err(),
        "the fire of an answered caller's deadline is dropped: {screened:?}"
    );
}
