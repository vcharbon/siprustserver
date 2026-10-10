//! The writers of the call's attempts (`call.attempts`): open one, join a leg
//! to it, end it, and settle the open ones against the legs and the call. An
//! attempt opens once per routing answer that dials, and ends once.

use crate::model::{
    Attempt, AttemptCause, AttemptId, AttemptOutcome, Call, CallModelState, Deadline, Leg,
    LegDisposition, LegState,
};

/// The id the next attempt of `call` opens under.
pub fn next_attempt_id(call: &Call) -> AttemptId {
    AttemptId(call.attempts.len() as u32)
}

/// The attempt `id` names on `call`.
pub fn find_attempt(call: &Call, id: AttemptId) -> Option<&Attempt> {
    call.attempts.iter().find(|a| a.id == id)
}

/// Open the next attempt for `cause` under the call's current decision. An
/// attempt still open whose legs no longer ring ends first: `Failed` when
/// every leg it dialled has ended, `Superseded` when one is still being
/// cancelled. One with a leg still ringing, not being cancelled, stays open
/// beside the new one. The new attempt's id, and the attempts it ended.
pub fn open_attempt(
    call: &mut Call,
    cause: AttemptCause,
    no_answer: Deadline,
) -> (AttemptId, Vec<AttemptId>) {
    let mut ended = Vec::new();
    for at in 0..call.attempts.len() {
        if !call.attempts[at].is_open() || rings(call, &call.attempts[at]) {
            continue;
        }
        let outcome = if all_legs_ended(call, &call.attempts[at]) {
            AttemptOutcome::Failed
        } else {
            AttemptOutcome::Superseded
        };
        call.attempts[at].outcome = Some(outcome);
        ended.push(call.attempts[at].id);
    }
    let id = next_attempt_id(call);
    call.attempts.push(Attempt {
        id,
        cause,
        decision_ordinal: call.decision_ordinal,
        legs: Vec::new(),
        no_answer,
        outcome: None,
    });
    (id, ended)
}

/// Record that attempt `id` dialled `leg_id`: the attempt lists it, the leg
/// names the attempt.
pub fn join_attempt(call: &mut Call, id: AttemptId, leg_id: &str) {
    if let Some(attempt) = call.attempts.iter_mut().find(|a| a.id == id) {
        attempt.legs.push(leg_id.to_string());
    }
    if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
        leg.attempt = Some(id);
    }
}

/// End attempt `id` with `outcome`; `false` when it was not open.
pub fn end_attempt(call: &mut Call, id: AttemptId, outcome: AttemptOutcome) -> bool {
    match call.attempts.iter_mut().find(|a| a.id == id && a.is_open()) {
        Some(attempt) => {
            attempt.outcome = Some(outcome);
            true
        }
        None => false,
    }
}

/// End every open attempt the call's state has decided: `Answered` when one of
/// its legs answered, `CallEnded` once the call's termination began, `Failed`
/// when every leg it dialled has ended. The attempts ended, with their
/// outcomes.
pub fn settle_attempts(call: &mut Call) -> Vec<(AttemptId, AttemptOutcome)> {
    let going_away = matches!(call.state, CallModelState::Terminating | CallModelState::Terminated);
    let mut settled = Vec::new();
    for at in 0..call.attempts.len() {
        let attempt = &call.attempts[at];
        if !attempt.is_open() {
            continue;
        }
        let legs = || attempt.legs.iter().filter_map(|id| b_leg(call, id));
        let outcome = if legs().any(|l| l.disposition() == LegDisposition::Bridged) {
            AttemptOutcome::Answered
        } else if going_away {
            AttemptOutcome::CallEnded
        } else if all_legs_ended(call, attempt) {
            AttemptOutcome::Failed
        } else {
            continue;
        };
        call.attempts[at].outcome = Some(outcome);
        settled.push((call.attempts[at].id, outcome));
    }
    settled
}

/// Whether a dialled leg has ended: it reads `Terminated` and waits on
/// nothing — no CANCEL awaiting its INVITE's final, no BYE awaiting its own.
pub fn dialled_leg_ended(leg: &Leg) -> bool {
    leg.state() == LegState::Terminated
        && leg.disposition() != LegDisposition::Cancelling
        && leg.end() != Some(crate::model::ByeDisposition::ByeSent)
}

/// Whether a leg of `attempt` is live: neither ended nor being cancelled (a
/// cancelled leg reads `Terminated` from its CANCEL on).
fn rings(call: &Call, attempt: &Attempt) -> bool {
    attempt.legs.iter().filter_map(|id| b_leg(call, id)).any(|l| l.state() != LegState::Terminated)
}

fn all_legs_ended(call: &Call, attempt: &Attempt) -> bool {
    !attempt.legs.is_empty()
        && attempt.legs.iter().all(|id| b_leg(call, id).is_none_or(dialled_leg_ended))
}

fn b_leg<'a>(call: &'a Call, leg_id: &str) -> Option<&'a Leg> {
    call.b_legs.iter().find(|l| l.leg_id == leg_id)
}
