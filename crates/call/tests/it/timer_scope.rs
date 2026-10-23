//! A timer's scope ends with what it guards (ADR-0042, ADR-0055 §10): the
//! call's setup while a leg holds a confirmed dialog or once the call is
//! terminating, an attempt as it ends, a leg as it is terminated, a service
//! timer with its machine.

use crate::common::representative_call;
use call::helpers::{add_b_leg, end_attempt, open_attempt, scope_ended, setup_over};
use call::model::*;
use call::testkit::{dialled_leg, stage_leg};

/// A call still in setup: no final sent to its caller, no dialled leg, its
/// termination not begun, no attempt, no service machine active.
fn call() -> Call {
    let mut call = representative_call();
    call.a_leg.invite_final_sent = None;
    call.a_leg.dialogs = Default::default();
    stage_leg(&mut call.a_leg, LegState::Early);
    call.b_legs.clear();
    call.state = CallModelState::Active;
    call.attempts.clear();
    call.sm_cursors.clear();
    call
}

/// A media server's leg answering ends the setup, as a callee's does: the
/// setup is over while any leg holds a confirmed dialog.
#[test]
fn an_answered_media_server_leg_ends_the_setup() {
    let mut media = dialled_leg("b-1", "mrf-tag");
    media.kind = Some(LegKind::Media);
    stage_leg(&mut media, LegState::Early);
    let mut call = add_b_leg(call(), media);
    assert!(!setup_over(&call), "the media server rings");
    stage_leg(&mut call.b_legs[0], LegState::Confirmed);
    assert!(setup_over(&call), "the media server answered");
}

#[test]
fn the_termination_ends_the_setup() {
    let mut call = call();
    assert!(!setup_over(&call));
    call.state = CallModelState::Terminating;
    assert!(setup_over(&call));
}

/// The setup is read as the call stands at the fire, never left for good: a
/// confirmed dialog may end before the setup does.
#[test]
fn the_setup_scope_is_read_at_the_fire() {
    let mut call = call();
    call.state = CallModelState::Terminating;
    assert!(!scope_ended(&call, &TimerKey::setup(TimerType::SetupTimeout)));
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
