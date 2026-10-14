//! The lifecycle order the replication gates read (`call::helpers::lifecycle`):
//! the answer axis ranks first, so an unanswered teardown never ends an
//! answered call, and between bodies alike on the answer the one further
//! toward its end is ahead; between bodies alike on both, a re-INVITE round
//! one holds answered is ahead of the same round held open.

use crate::common;
use call::helpers::{
    lifecycle_advances, lifecycle_behind, lifecycle_progress, round_further, Progress,
};
use call::model::{Call, CallModelState};
use call::testkit::{stage_reinvite_round, RoundStage};

/// The representative call with its caller answered `final_sent` and in
/// `state`.
fn at(final_sent: Option<u16>, state: CallModelState) -> Call {
    let mut call = common::representative_call();
    call.a_leg.invite_final_sent = final_sent;
    call.a_leg.answer_preempted = None;
    call.state = state;
    call
}

const ANSWERED: Option<u16> = Some(200);
const REFUSED: Option<u16> = Some(480);

#[test]
fn an_unanswered_teardown_is_behind_an_answered_call() {
    let ended = at(REFUSED, CallModelState::Terminated);
    let answered = at(ANSWERED, CallModelState::Active);
    assert!(lifecycle_behind(&ended, &answered), "the teardown never ends the answered call");
    assert!(!lifecycle_behind(&answered, &ended), "the answered call wins the crossed axes");
    assert!(!lifecycle_advances(&ended, &answered) && !lifecycle_advances(&answered, &ended));
}

#[test]
fn between_bodies_alike_on_the_answer_the_one_further_toward_its_end_is_ahead() {
    let live = at(ANSWERED, CallModelState::Active);
    let ending = at(ANSWERED, CallModelState::Terminating);
    let ended = at(ANSWERED, CallModelState::Terminated);
    assert!(lifecycle_behind(&live, &ending) && lifecycle_behind(&ending, &ended));
    assert!(!lifecycle_behind(&ended, &live), "an answered call's end is its progress");
    let ringing = at(None, CallModelState::Active);
    let refused = at(REFUSED, CallModelState::Terminated);
    assert!(lifecycle_behind(&ringing, &refused), "a ringing call's teardown is its progress");
}

#[test]
fn a_body_is_never_behind_itself() {
    let answered = at(ANSWERED, CallModelState::Active);
    assert!(!lifecycle_behind(&answered, &answered));
}

/// The answered representative call in `state`, a re-INVITE round on the
/// callee's dialog standing at `stage` on the client transaction `branch`.
fn round(state: CallModelState, branch: &str, stage: RoundStage) -> Call {
    let mut call = at(ANSWERED, state);
    stage_reinvite_round(&mut call.b_legs[0], branch, stage);
    call
}

const ROUND: &str = "z9hG4bK-round-1";

#[test]
fn a_round_ended_in_the_other_copy_advances_the_record_holding_it_open() {
    let open = round(CallModelState::Active, ROUND, RoundStage::Open);
    for stage in [RoundStage::Answered, RoundStage::Acked, RoundStage::Refused] {
        let ended = round(CallModelState::Active, ROUND, stage);
        assert!(round_further(&ended, &open), "{stage:?} over open");
        assert_eq!(lifecycle_progress(&ended, &open), Some(Progress::Round), "{stage:?}");
        assert!(lifecycle_behind(&open, &ended), "the open record is behind {stage:?}");
        assert!(!lifecycle_behind(&ended, &open));
    }
}

#[test]
fn a_next_round_begun_in_the_other_copy_ended_the_one_held_open() {
    let open = round(CallModelState::Active, ROUND, RoundStage::Open);
    let mut next = round(CallModelState::Active, ROUND, RoundStage::Acked);
    stage_reinvite_round(&mut next.b_legs[0], "z9hG4bK-round-2", RoundStage::Open);
    assert!(lifecycle_advances(&next, &open), "the first round's end, seen in the next one");
}

#[test]
fn an_acked_round_advances_the_record_awaiting_its_ack() {
    let answered = round(CallModelState::Active, ROUND, RoundStage::Answered);
    let acked = round(CallModelState::Active, ROUND, RoundStage::Acked);
    assert!(lifecycle_advances(&acked, &answered), "the ACK left in the other copy");
    assert!(lifecycle_behind(&answered, &acked));
}

#[test]
fn a_round_never_goes_back() {
    let open = round(CallModelState::Active, ROUND, RoundStage::Open);
    let answered = round(CallModelState::Active, ROUND, RoundStage::Answered);
    let acked = round(CallModelState::Active, ROUND, RoundStage::Acked);
    assert!(!round_further(&open, &answered) && !round_further(&open, &acked));
    assert!(!lifecycle_advances(&open, &answered), "an open round is no progress over its end");
    assert!(!lifecycle_advances(&answered, &acked), "an ACK awaited is no progress over it sent");
}

#[test]
fn a_body_that_never_saw_the_round_is_not_past_it() {
    let open = round(CallModelState::Active, ROUND, RoundStage::Open);
    let before = at(ANSWERED, CallModelState::Active);
    assert!(!round_further(&before, &open), "its sequence stands before the round's CSeq");
    assert!(!lifecycle_advances(&before, &open) && !lifecycle_behind(&open, &before));
}

#[test]
fn an_answered_round_never_outranks_the_record_ending_the_call() {
    let ending = round(CallModelState::Terminating, ROUND, RoundStage::Open);
    let answered = round(CallModelState::Active, ROUND, RoundStage::Answered);
    assert!(!lifecycle_advances(&answered, &ending), "progress is a chain");
    assert!(lifecycle_behind(&answered, &ending), "the ending axis ranks before the round");
}

#[test]
fn crossed_rounds_are_no_progress_either_way() {
    let mut first_answered = round(CallModelState::Active, ROUND, RoundStage::Answered);
    stage_reinvite_round(&mut first_answered.a_leg, "z9hG4bK-round-2", RoundStage::Open);
    let mut second_answered = round(CallModelState::Active, ROUND, RoundStage::Open);
    stage_reinvite_round(&mut second_answered.a_leg, "z9hG4bK-round-2", RoundStage::Answered);
    assert!(!lifecycle_advances(&first_answered, &second_answered), "progress is a chain");
    assert!(!lifecycle_advances(&second_answered, &first_answered), "on every round");
}
