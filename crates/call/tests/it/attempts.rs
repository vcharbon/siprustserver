//! The attempt writers: an attempt opens per routing answer, and ends once —
//! superseded or failed when the next one opens, else as the legs and the
//! call settle it.

use crate::common::representative_call;
use call::helpers::*;
use call::model::*;
use call::testkit::{dialled_leg, stage_leg};

/// A call whose only dialled leg is `b-1`, in `state`, under attempt 0.
fn call_with_attempt(state: LegState) -> Call {
    let mut call = representative_call();
    call.attempts.clear();
    let mut leg = dialled_leg("b-1", "b1-tag");
    stage_leg(&mut leg, state);
    call.b_legs = vec![leg];
    let (id, ended) = open_attempt(&mut call, AttemptCause::Route, Deadline::Off);
    assert_eq!((id, ended), (AttemptId(0), vec![]));
    join_attempt(&mut call, id, "b-1");
    call
}

#[test]
fn an_attempt_names_its_legs_and_its_legs_name_it() {
    let call = call_with_attempt(LegState::Trying);
    assert_eq!(call.attempts[0].legs, vec!["b-1".to_string()]);
    assert_eq!(call.b_legs[0].attempt, Some(AttemptId(0)));
    assert_eq!(call.attempts[0].decision_ordinal, call.decision_ordinal);
    assert_eq!(next_attempt_id(&call), AttemptId(1));
}

#[test]
fn the_next_attempt_supersedes_one_whose_leg_is_being_cancelled() {
    let mut call = call_with_attempt(LegState::Early);
    call.b_legs[0].outbound_invite_mut().unwrap().cancel_sent = true;
    let (id, ended) = open_attempt(&mut call, AttemptCause::Failover, Deadline::Off);
    assert_eq!((id, ended), (AttemptId(1), vec![AttemptId(0)]));
    assert_eq!(call.attempts[0].outcome, Some(AttemptOutcome::Superseded));
    assert!(call.attempts[1].is_open());
}

#[test]
fn an_attempt_whose_leg_still_rings_stays_open_beside_the_next() {
    let mut call = call_with_attempt(LegState::Early);
    let (id, ended) = open_attempt(&mut call, AttemptCause::Transfer, Deadline::Off);
    assert_eq!((id, ended), (AttemptId(1), vec![]));
    assert!(call.attempts[0].is_open() && call.attempts[1].is_open());
}

#[test]
fn the_next_attempt_ends_one_whose_legs_all_ended_as_failed() {
    let mut call = call_with_attempt(LegState::Terminated);
    let (_, ended) = open_attempt(&mut call, AttemptCause::Failover, Deadline::Off);
    assert_eq!(ended, vec![AttemptId(0)]);
    assert_eq!(call.attempts[0].outcome, Some(AttemptOutcome::Failed));
}

#[test]
fn settling_reads_the_legs_and_the_call() {
    let mut ringing = call_with_attempt(LegState::Early);
    assert_eq!(settle_attempts(&mut ringing), vec![], "a ringing attempt stays open");

    let mut cancelling = call_with_attempt(LegState::Early);
    cancelling.b_legs[0].outbound_invite_mut().unwrap().cancel_sent = true;
    assert_eq!(settle_attempts(&mut cancelling), vec![], "a cancelling leg has not ended");

    let mut refused = call_with_attempt(LegState::Terminated);
    assert_eq!(settle_attempts(&mut refused), vec![(AttemptId(0), AttemptOutcome::Failed)]);

    let mut answered = call_with_attempt(LegState::Confirmed);
    assert_eq!(settle_attempts(&mut answered), vec![(AttemptId(0), AttemptOutcome::Answered)]);

    let mut ending = call_with_attempt(LegState::Early);
    ending.state = CallModelState::Terminating;
    assert_eq!(settle_attempts(&mut ending), vec![(AttemptId(0), AttemptOutcome::CallEnded)]);
    assert_eq!(settle_attempts(&mut ending), vec![], "an attempt ends once");
}

#[test]
fn an_ended_attempt_never_reopens() {
    let mut call = call_with_attempt(LegState::Early);
    assert!(end_attempt(&mut call, AttemptId(0), AttemptOutcome::Refused));
    assert!(!end_attempt(&mut call, AttemptId(0), AttemptOutcome::Answered));
    assert_eq!(call.attempts[0].outcome, Some(AttemptOutcome::Refused));
}

#[test]
fn the_next_leg_id_is_the_one_a_dial_mints() {
    let call = call_with_attempt(LegState::Trying);
    assert_eq!(next_b_leg_id(&call), "b-2");
}

#[test]
fn a_ring_is_recorded_on_its_leg() {
    let mut call = call_with_attempt(LegState::Early);
    mark_rang(&mut call, "b-1");
    assert!(call.b_legs[0].ring.rang());
}
