//! The timer-scope model invariants: a timer dies with the scope it guards.

use call::helpers::find_leg;
use call::{Leg, LegState, TimerType};

use super::Turn;

/// A leg-scoped timer dies with its leg: no armed timer names a leg that is
/// Terminated or no longer in the record.
pub(super) fn no_timer_on_a_terminated_leg(turn: &Turn<'_>) -> Result<(), String> {
    for timer in &turn.after.timers {
        let Some(leg_id) = timer.leg_id() else { continue };
        let state = find_leg(turn.after, leg_id).map(Leg::state);
        if matches!(state, None | Some(LegState::Terminated)) {
            let state = state.map_or("gone".to_string(), |s| format!("{s:?}"));
            let timer_type = timer.timer_type();
            return Err(format!(
                "timer {timer_type:?}:{leg_id} is armed on leg {leg_id}, which is {state}"
            ));
        }
    }
    Ok(())
}

/// An attempt-scoped timer dies with its attempt: no armed timer names an
/// attempt that ended or is not in the record.
pub(super) fn no_timer_on_an_ended_attempt(turn: &Turn<'_>) -> Result<(), String> {
    for timer in &turn.after.timers {
        let Some(id) = timer.key.attempt_id() else { continue };
        match call::helpers::find_attempt(turn.after, id) {
            Some(attempt) if attempt.is_open() => {}
            Some(attempt) => {
                return Err(format!(
                    "timer {:?} is armed on {id:?}, which ended {:?}",
                    timer.key, attempt.outcome
                ))
            }
            None => return Err(format!("timer {:?} is armed on {id:?}, which is gone", timer.key)),
        }
    }
    Ok(())
}

/// A service timer dies with its machine: no armed service timer names a
/// machine with no cursor.
pub(super) fn no_timer_of_an_inactive_machine(turn: &Turn<'_>) -> Result<(), String> {
    for timer in &turn.after.timers {
        if let TimerType::Service { service_id, .. } = timer.timer_type() {
            if !turn.after.sm_cursors.contains_key(service_id) {
                return Err(format!(
                    "timer {:?} is armed for machine {}, which is inactive",
                    timer.key,
                    service_id.as_str()
                ));
            }
        }
    }
    Ok(())
}
