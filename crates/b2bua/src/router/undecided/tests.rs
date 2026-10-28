//! The undecided predicate: a call is undecided from its birth until its first
//! routing decision applies, the caller is answered, or the call ends.

use call::{Call, CallModelState, DecisionKind, LegState};

use super::is_undecided;
use crate::config::B2buaConfig;
use crate::initial_invite::build_initial_call;
use crate::router::test_support::{invite, src};

/// A call as the INVITE's turn creates it, before the decision answers.
fn born() -> Call {
    let config = B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    build_initial_call(
        &invite("w0", "w1", "undecided"),
        src(),
        &config,
        &mut sip_ids::TurnIds::fixed(1),
        0,
    )
}

#[test]
fn a_call_as_born_is_undecided() {
    assert!(is_undecided(&born()));
}

#[test]
fn a_call_with_an_applied_decision_is_decided() {
    let mut call = born();
    call::helpers::mark_decision(&mut call, 5, DecisionKind::Route, Some("a".into()), None);
    assert!(!is_undecided(&call), "a decision is on the record");
}

#[test]
fn a_decision_recorded_on_a_cancelled_setup_leaves_the_call_undecided() {
    let mut call = born();
    call::helpers::mark_decision(&mut call, 5, DecisionKind::Route, Some("a".into()), None);
    call.setup_cancelled = true;
    assert!(is_undecided(&call), "the recorded decision was never applied");
}

#[test]
fn a_call_whose_caller_was_answered_is_decided() {
    let mut call = born();
    *call.a_leg.invite_final_sent_mut() = Some(503);
    assert!(!is_undecided(&call), "the caller already has her final");
}

#[test]
fn a_call_that_dialled_a_leg_is_decided() {
    let mut call = born();
    let mut b = call.a_leg.clone();
    b.leg_id = "b-1".into();
    call::helpers::add_b_leg(&mut call, b);
    assert!(!is_undecided(&call), "a leg was dialled");
}

#[test]
fn an_ended_call_is_not_undecided() {
    let mut call = born();
    call.state = CallModelState::Terminated;
    call::testkit::stage_leg(&mut call.a_leg, LegState::Terminated);
    assert!(!is_undecided(&call), "nothing is left to settle");
}

#[test]
fn a_call_with_an_attempt_open_is_decided() {
    let mut call = born();
    call::helpers::open_attempt(&mut call, call::AttemptCause::Route, call::Deadline::Off);
    assert!(!is_undecided(&call), "an attempt is dialling");
}
