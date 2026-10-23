//! A timer's scope ends with what it guards (ADR-0042, ADR-0055 §10): the
//! caller's wait with its INVITE's final or the call's termination, an attempt
//! as it ends, a leg as it is terminated, a service timer with its machine.

use crate::common::representative_call;
use call::helpers::{end_attempt, open_attempt, scope_ended};
use call::model::*;

/// A call still in setup: no final sent to its caller, its termination not
/// begun, no attempt, no service machine active.
fn call() -> Call {
    let mut call = representative_call();
    call.a_leg.invite_final_sent = None;
    call.state = CallModelState::Active;
    call.attempts.clear();
    call.sm_cursors.clear();
    call
}

#[test]
fn the_callers_final_ends_the_wait_for_it() {
    let mut call = call();
    let key = TimerKey::until_answered(TimerType::SetupTimeout);
    assert!(!scope_ended(&call, &key), "the caller waits");
    call.a_leg.invite_final_sent = Some(486);
    assert!(scope_ended(&call, &key), "a refusal answers the caller too");
}

#[test]
fn the_termination_ends_the_wait_for_the_callers_final() {
    let mut call = call();
    call.state = CallModelState::Terminating;
    assert!(scope_ended(&call, &TimerKey::until_answered(TimerType::SetupTimeout)));
    assert!(!scope_ended(&call, &TimerKey::call(TimerType::TerminatingTimeout)));
}

#[test]
fn an_ended_attempt_ends_its_scope() {
    let mut call = call();
    let (id, _) = open_attempt(&mut call, AttemptCause::Route, Deadline::Off);
    let key = TimerKey::attempt(TimerType::NoAnswer, id);
    assert!(!scope_ended(&call, &key));
    end_attempt(&mut call, id, AttemptOutcome::Failed);
    assert!(scope_ended(&call, &key));
}

#[test]
fn a_gone_leg_ends_its_scope() {
    let call = call();
    assert!(scope_ended(&call, &TimerKey::leg(TimerType::NoAnswer, "b-9")));
    assert!(!scope_ended(&call, &TimerKey::leg(TimerType::KeepaliveTimeout, "a")));
}

#[test]
fn a_service_timer_ends_with_its_machine() {
    let mut call = call();
    let machine = MachineId::new("svc");
    let key = TimerKey::call(TimerType::service(machine.clone(), "watch"));
    assert!(scope_ended(&call, &key), "no cursor: the machine is inactive");
    call.sm_cursors.insert(machine, StateLabel::new("watching"));
    assert!(!scope_ended(&call, &key));
}
