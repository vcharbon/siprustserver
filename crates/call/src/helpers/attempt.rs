//! The writers of the call's attempts (`call.attempts`): open one, join a leg
//! to it, end it, and settle the open ones against the legs and the call. An
//! attempt opens once per routing answer that dials, and ends once.

use crate::model::{
    Attempt, AttemptCause, AttemptId, AttemptOutcome, Call, CallModelState, Deadline, Leg,
    LegDisposition, LegState,
};

/// The id the next attempt of `call` opens under.
pub fn next_attempt_id(call: &Call) -> AttemptId {
    AttemptId(call.attempts.len())
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
    let decided: Vec<Option<AttemptOutcome>> = call
        .attempts
        .iter()
        .map(|a| {
            (a.is_open() && !rings(call, a)).then(|| {
                if all_legs_ended(call, a) {
                    AttemptOutcome::Failed
                } else {
                    AttemptOutcome::Superseded
                }
            })
        })
        .collect();
    let ended = set_outcomes(call, &decided).into_iter().map(|(id, _)| id).collect();
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
    let decided: Vec<Option<AttemptOutcome>> = call
        .attempts
        .iter()
        .map(|attempt| {
            if !attempt.is_open() {
                return None;
            }
            let legs = || attempt.legs.iter().filter_map(|id| b_leg(call, id));
            if legs().any(|l| l.disposition() == LegDisposition::Bridged) {
                Some(AttemptOutcome::Answered)
            } else if going_away {
                Some(AttemptOutcome::CallEnded)
            } else if all_legs_ended(call, attempt) {
                Some(AttemptOutcome::Failed)
            } else {
                None
            }
        })
        .collect();
    set_outcomes(call, &decided)
}

/// End each attempt with the outcome `decided` holds at its position; the
/// attempts ended, with their outcomes, in order.
fn set_outcomes(
    call: &mut Call,
    decided: &[Option<AttemptOutcome>],
) -> Vec<(AttemptId, AttemptOutcome)> {
    let mut ended = Vec::new();
    for (attempt, outcome) in call.attempts.iter_mut().zip(decided) {
        if let Some(outcome) = *outcome {
            attempt.outcome = Some(outcome);
            ended.push((attempt.id, outcome));
        }
    }
    ended
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
