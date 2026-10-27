//! The reaper's verdicts as the rules read them: an overflow verdict tears
//! the call down on the wire, the message-cap verdict bounds its work.

use b2bua_core::effects::CriticalStateEffect;
use b2bua_core::reaper_verdict::{OUTCOME_MESSAGE_CAP, OUTCOME_OVERFLOW};
use b2bua_core::turn_driver::{Driver, Session, Step, Turn};
use call::{CallModelState, TerminationCause, TimerType};
use sip_message::Method;

use crate::calls::{bridged, ringing};

/// The requests `turn` sent toward `leg` with `method`.
fn requests_to(turn: &Turn, leg: &str, method: &str) -> usize {
    turn.methods_to(leg).iter().filter(|(m, _)| m == method).count()
}

/// A call bob hung up: his BYE began its teardown.
fn hung_up(driver: &Driver) -> Session<'_> {
    let mut call = bridged(driver);
    call.run([Step::Sends("b-1", Method::Bye)]);
    assert_eq!(call.call().state, CallModelState::Terminating);
    call
}

/// A ringing call condemned for flooding its overflow: its callee is alive
/// and is CANCELed, and the CDR names the teardown.
#[test]
fn an_overflow_verdict_on_a_ringing_call_cancels_the_callee() {
    let driver = Driver::new();
    let mut call = ringing(&driver);
    let turn = call.turned(Step::Reaper(OUTCOME_OVERFLOW));
    assert_eq!(requests_to(turn, "b-1", "CANCEL"), 1, "the ringing callee is CANCELed");
    assert!(
        call.call().cdr_events.iter().any(|e| e.reason.as_deref() == Some("dispatch-overflow")),
        "the CDR names the teardown"
    );
}

/// A call already terminating — bob hung up — is left to that teardown: the
/// verdict records nothing, keeps the cause, and re-arms no timer.
#[test]
fn an_overflow_verdict_on_a_terminating_call_changes_nothing() {
    let driver = Driver::new();
    let mut call = hung_up(&driver);
    let turn = call.turned(Step::Reaper(OUTCOME_OVERFLOW));
    assert!(
        !turn.result.effects.critical.iter().any(|e| matches!(
            e,
            CriticalStateEffect::ScheduleTimer(t)
                if *t.timer_type() == TimerType::TerminatingTimeout
        )),
        "the terminating timer is not re-armed"
    );
    let record = call.call();
    assert!(
        !record.cdr_events.iter().any(|e| e.reason.as_deref() == Some("dispatch-overflow")),
        "no overflow teardown is recorded"
    );
    assert_eq!(
        record.termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteBye),
        "the cause bob gave stays"
    );
}

/// An Active call past its lifetime message cap is torn down on the wire:
/// the ringing callee is CANCELed, the cause is the cap.
#[test]
fn a_message_cap_verdict_on_a_ringing_call_cancels_the_callee() {
    let driver = Driver::new();
    let mut call = ringing(&driver);
    let turn = call.turned(Step::Reaper(OUTCOME_MESSAGE_CAP));
    assert_eq!(requests_to(turn, "b-1", "CANCEL"), 1, "the ringing callee is CANCELed");
    let record = call.call();
    assert_eq!(record.termination.as_ref().map(|t| t.cause), Some(TerminationCause::MessageCap));
    assert!(record.cdr_events.iter().any(|e| e.reason.as_deref() == Some("message-cap-lifetime")));
}

/// A terminating call past its cap — bob already sent his BYE — is forced
/// terminal now: its unresolved legs resolved, the reason recorded, the cause
/// it already had kept.
#[test]
fn a_message_cap_verdict_on_a_terminating_call_forces_it_terminal() {
    let driver = Driver::new();
    let mut call = hung_up(&driver);
    let turn = call.turned(Step::Reaper(OUTCOME_MESSAGE_CAP));
    assert_eq!(requests_to(turn, "b-1", "CANCEL"), 0, "nothing more on the wire");
    let record = call.call();
    assert!(record.b_legs.iter().all(call::helpers::leg_is_resolved));
    assert!(record.cdr_events.iter().any(|e| e.reason.as_deref() == Some("message-cap-lifetime")));
    assert_eq!(
        record.termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteBye),
        "the first termination names who ended the call"
    );
}
