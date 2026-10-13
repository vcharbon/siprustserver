//! The lifecycle order the replication gates read (`call::helpers::lifecycle`):
//! the answer axis ranks first, so an unanswered teardown never ends an
//! answered call, and between bodies alike on the answer the one further
//! toward its end is ahead.

use crate::common;
use call::helpers::{lifecycle_advances, lifecycle_behind};
use call::model::{Call, CallModelState};

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
