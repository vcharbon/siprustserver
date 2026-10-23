//! The lifecycle order the replication gates read (`call::helpers::lifecycle`):
//! the answer axis ranks first, so an unanswered teardown never ends an
//! answered call, and between bodies alike on the answer the one further
//! toward its end is ahead; between bodies alike on both, a re-INVITE round
//! one holds answered is ahead of the same round held open, and a round one
//! opened is ahead of a body that never reached it.

use crate::common;
use call::helpers::{
    lifecycle_advances, lifecycle_behind, lifecycle_progress, merge_rounds, round_ahead,
    round_further, round_opened, Progress,
};
use call::model::{Call, CallModelState};
use call::testkit::{stage_reinvite_round, RoundStage};

/// The representative call with its caller answered `final_sent` and in
/// `state`.
fn at(final_sent: Option<u16>, state: CallModelState) -> Call {
    let mut call = common::representative_call();
    *call.a_leg.invite_final_sent_mut() = final_sent;
    *call.a_leg.answer_preempted_mut() = None;
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
fn a_round_the_other_copy_opened_advances_a_record_that_never_reached_it() {
    let before = at(ANSWERED, CallModelState::Active);
    for stage in [RoundStage::Open, RoundStage::Answered, RoundStage::Acked] {
        let opened = round(CallModelState::Active, ROUND, stage);
        assert!(round_opened(&opened, &before), "{stage:?}");
        assert_eq!(lifecycle_progress(&opened, &before), Some(Progress::Round), "{stage:?}");
        assert!(lifecycle_behind(&before, &opened), "{stage:?}: the record lacking it is behind");
        assert!(!lifecycle_advances(&before, &opened), "{stage:?}: lacking a round is no progress");
    }
}

#[test]
fn a_round_both_copies_hold_is_no_opening() {
    let open = round(CallModelState::Active, ROUND, RoundStage::Open);
    let answered = round(CallModelState::Active, ROUND, RoundStage::Answered);
    assert!(!round_opened(&answered, &open) && !round_opened(&open, &answered));
    assert!(round_ahead(&answered, &open), "its answer is the progress");
    assert!(!round_ahead(&open, &answered));
}

#[test]
fn a_round_past_which_the_record_sent_is_no_opening() {
    let opened = round(CallModelState::Active, ROUND, RoundStage::Open);
    let mut sent = at(ANSWERED, CallModelState::Active);
    sent.b_legs[0].dialogs.confirmed_mut().expect("the callee's dialog").our_cseq += 1;
    assert!(!round_opened(&opened, &sent), "the record sent a request at the round's CSeq");
}

#[test]
fn rounds_opened_on_both_dialogs_crossed_are_each_progress_on_its_dialog() {
    let mut callers = at(ANSWERED, CallModelState::Active);
    stage_reinvite_round(&mut callers.b_legs[0], ROUND, RoundStage::Open);
    let mut callees = at(ANSWERED, CallModelState::Active);
    stage_reinvite_round(&mut callees.a_leg, "z9hG4bK-round-2", RoundStage::Open);
    assert!(round_opened(&callers, &callees) && round_opened(&callees, &callers));
    assert_eq!(lifecycle_progress(&callers, &callees), Some(Progress::Round));
    assert_eq!(lifecycle_progress(&callees, &callers), Some(Progress::Round));
    let both = merge_rounds(merge_rounds(callers.clone(), &callees), &callers);
    assert!(!round_ahead(&callers, &both) && !round_ahead(&callees, &both), "both rounds held");
}

#[test]
fn a_sequence_raised_past_a_round_does_not_reach_it() {
    let opened = round(CallModelState::Active, ROUND, RoundStage::Acked);
    let mut raised = at(ANSWERED, CallModelState::Active);
    let d = raised.b_legs[0].dialogs.confirmed_mut().expect("the callee's dialog");
    d.sent_invite = Some(call::model::RequestRecord {
        origin: call::model::RequestOrigin::Own,
        sent: Some(call::model::SentRequest {
            branch: "z9hG4bK-initial".into(),
            image: Vec::new(),
            destination: call::model::HostPort { host: "staged.invalid".into(), port: 5060 },
        }),
        ..call::model::RequestRecord::relayed("INVITE", 1, 0)
    });
    d.our_cseq += 5;
    assert!(round_opened(&opened, &raised), "an earlier transaction is no reach of the round");
}

#[test]
fn a_round_on_a_dialog_ahead_folds_whatever_the_other_dialog_holds() {
    let mut copy = round(CallModelState::Active, ROUND, RoundStage::Acked);
    stage_reinvite_round(&mut copy.a_leg, "z9hG4bK-round-2", RoundStage::Open);
    let mut record = round(CallModelState::Active, ROUND, RoundStage::Answered);
    stage_reinvite_round(&mut record.a_leg, "z9hG4bK-round-2", RoundStage::Answered);
    assert_eq!(lifecycle_progress(&copy, &record), Some(Progress::Round), "the callee's ACK");
    assert_eq!(lifecycle_progress(&record, &copy), Some(Progress::Round), "the caller's 2xx");
}

#[test]
fn an_answered_round_never_outranks_the_record_ending_the_call() {
    let ending = round(CallModelState::Terminating, ROUND, RoundStage::Open);
    let answered = round(CallModelState::Active, ROUND, RoundStage::Answered);
    assert!(!lifecycle_advances(&answered, &ending), "progress is a chain");
    assert!(lifecycle_behind(&answered, &ending), "the ending axis ranks before the round");
}

#[test]
fn crossed_rounds_fold_each_on_its_own_dialog() {
    let mut first_answered = round(CallModelState::Active, ROUND, RoundStage::Answered);
    stage_reinvite_round(&mut first_answered.a_leg, "z9hG4bK-round-2", RoundStage::Open);
    let mut second_answered = round(CallModelState::Active, ROUND, RoundStage::Open);
    stage_reinvite_round(&mut second_answered.a_leg, "z9hG4bK-round-2", RoundStage::Answered);
    let progress = lifecycle_progress(&first_answered, &second_answered);
    assert_eq!(progress, Some(Progress::Round), "the callee's round, answered");
    let merged = merge_rounds(second_answered.clone(), &first_answered);
    assert!(!round_ahead(&first_answered, &merged), "both answered rounds held");
    assert!(!round_ahead(&second_answered, &merged));
}

#[test]
fn an_ending_is_progress_whatever_round_the_record_holds() {
    let ending = at(ANSWERED, CallModelState::Terminated);
    let answered = round(CallModelState::Active, ROUND, RoundStage::Open);
    assert!(round_ahead(&answered, &ending), "the record holds a round the ending never read");
    assert_eq!(lifecycle_progress(&ending, &answered), Some(Progress::Lifecycle));
}
