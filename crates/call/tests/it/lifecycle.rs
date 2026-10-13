//! The lifecycle order the replication gates read (`call::helpers::lifecycle`):
//! the answer axis ranks first, so an unanswered teardown never ends an
//! answered call, and between bodies alike on the answer the one further
//! toward its end is ahead; between bodies alike on both, a re-INVITE round
//! one holds answered is ahead of the same round held open.

use crate::common;
use call::helpers::{lifecycle_advances, lifecycle_behind, round_answered_over};
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
fn an_answered_round_advances_the_record_holding_it_open() {
    let open = round(CallModelState::Active, ROUND, RoundStage::Open);
    for stage in [RoundStage::Answered, RoundStage::Acked] {
        let answered = round(CallModelState::Active, ROUND, stage);
        assert!(round_answered_over(&answered, &open), "{stage:?} over open");
        assert!(
            lifecycle_advances(&answered, &open),
            "{stage:?} is progress the open record lacks"
        );
        assert!(lifecycle_behind(&open, &answered), "the open record is behind {stage:?}");
        assert!(!lifecycle_behind(&answered, &open));
    }
}

#[test]
fn a_round_never_goes_back_to_open() {
    let open = round(CallModelState::Active, ROUND, RoundStage::Open);
    let answered = round(CallModelState::Active, ROUND, RoundStage::Answered);
    assert!(!round_answered_over(&open, &answered));
    assert!(!lifecycle_advances(&open, &answered), "an open round is no progress over its answer");
    let acked = round(CallModelState::Active, ROUND, RoundStage::Acked);
    assert!(
        !lifecycle_advances(&acked, &answered) && !lifecycle_advances(&answered, &acked),
        "the axis is the answer the open record cannot see, not the ACK"
    );
}

#[test]
fn another_round_answered_is_not_this_ones_answer() {
    let open = round(CallModelState::Active, ROUND, RoundStage::Open);
    let other = round(CallModelState::Active, "z9hG4bK-round-2", RoundStage::Answered);
    assert!(!round_answered_over(&other, &open), "a round is named by its transaction");
    assert!(!lifecycle_advances(&other, &open) && !lifecycle_behind(&open, &other));
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
