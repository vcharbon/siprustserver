//! The parties' verdict of a cell, judged by its incident (ADR-0055 §11). The
//! uncut call must succeed. Once an incident strikes, the system under test
//! may end the call cleanly at any instant (§1, §4): the parties accept a
//! clean end then, and the verdict says so. Beyond that, only the declared
//! outcomes pass:
//! - a split, healed or not, is judged on leaks, cleanup and the CDR count
//!   alone, never on the parties' verdict;
//! - a kill of the primary at a logged or left sub-point may leave a dialog
//!   to its endpoint's own timer, after a teardown whose write the backup
//!   never applied (§5: a quiet call of a departed primary stays reactive);
//! - a kill of the primary at a logged or left sub-point of a turn taken on
//!   something no peer repeats (a response, a timer, an off-turn answer) may
//!   lose that turn's effects: the parties wait for them in vain, and every
//!   dialog, early or confirmed, is left to its endpoint's own timers;
//! - an incident that loses the call (a stall then a kill, a call with no
//!   backup) may leave the caller to her own Timer B, her INVITE drawing no
//!   final, and a callee to its own timers, its dialog confirmed.
//!
//! Nothing else is accepted: a request left without its final, or a dialog
//! left open in any other way, fails the cell.

use scenario_harness::actor::{FailedAt, LegPhase, ObservedState, ScenarioVerdict, StateInner};
use scenario_harness::StepError;

use super::Incident;
use crate::turn_points::TurnPoint;

/// The role whose INVITE opens every shape's call.
const CALLER: &str = "alice";

/// Where an incident struck.
#[derive(Clone, Copy)]
pub(super) struct CutAt {
    pub incident: Incident,
    pub point: TurnPoint,
    /// The cut turn handled a SIP request.
    pub on_request: bool,
}

/// Whether `verdict`, with what `obs` saw each leg left with, is an outcome
/// the cut — `None` for the uncut call — accepts.
pub(super) fn accepted(cut: Option<CutAt>, verdict: &ScenarioVerdict, obs: &ObservedState) -> bool {
    let Some(CutAt { incident, point, on_request }) = cut else { return verdict.is_success() };
    if incident.splits() || verdict.is_success() {
        return true;
    }
    let ScenarioVerdict::Failure { at, cause } = verdict else { return false };
    let StepError::Timeout { .. } = cause else { return false };
    let torn_down = at.barrier == FailedAt::TORN_DOWN;
    obs.with_snapshot(|s| match incident {
        Incident::PrimaryKilled => {
            matches!(point, TurnPoint::Logged | TurnPoint::Left)
                && (!on_request || (torn_down && teardown_lost_in_the_window(s)))
        }
        _ if incident.loses_the_call() => {
            caller_left_to_timer_b(s) || (torn_down && callees_left_to_their_timers(s))
        }
        _ => false,
    })
}

/// A teardown began — a leg ended — and every leg still up holds a confirmed
/// dialog its endpoint's own timer ends.
fn teardown_lost_in_the_window(s: &StateInner) -> bool {
    let ended = s.leg_phases().any(|(_, phase)| phase == LegPhase::Terminated);
    ended && live(s).all(|(_, phase)| phase == LegPhase::Confirmed)
}

/// The caller's INVITE drew no final: her own Timer B answers it.
fn caller_left_to_timer_b(s: &StateInner) -> bool {
    let caller = s.leg(CALLER);
    caller.phase() < LegPhase::Confirmed && caller.first_final().is_none()
}

/// Every leg still up is a callee holding a confirmed dialog.
fn callees_left_to_their_timers(s: &StateInner) -> bool {
    live(s).all(|(role, phase)| role != CALLER && phase == LegPhase::Confirmed)
}

/// The legs holding a dialog, early or confirmed.
fn live(s: &StateInner) -> impl Iterator<Item = (&'static str, LegPhase)> + '_ {
    s.leg_phases().filter(|(_, phase)| matches!(phase, LegPhase::Early | LegPhase::Confirmed))
}
