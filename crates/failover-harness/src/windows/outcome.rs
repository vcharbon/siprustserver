//! The party outcomes the window table accepts beyond a met goal (ADR-0055
//! §11): what the parties of a cut call may be left with, each the predicate
//! of one row. A failed verdict no row accepts fails the cell: a request left
//! without its final, or a dialog left open in any other way.

use scenario_harness::actor::{FailedAt, LegPhase, ObservedState, ScenarioVerdict, StateInner};
use scenario_harness::StepError;

/// What the sweep measured of the cut turn, read by the outcome rows.
#[derive(Clone, Copy, Debug, Default)]
pub struct CutFacts {
    /// The backup held the cut turn's write and its effects had left: the
    /// applied sub-point under `off` or past the cap, the left one under
    /// `applied`.
    pub held_and_left: bool,
    /// The backup held the cut turn's write, its effects left or not.
    pub held: bool,
    /// The cut turn handled a SIP request.
    pub on_request: bool,
    /// The cut turn was taken on an outside service's answer, which the copy
    /// asks for again while its record awaits it (ADR-0039 decision 5).
    pub on_answer: bool,
    /// Every request that reached a worker other than the primary drew its
    /// final there.
    pub living_answered: bool,
    /// A turn of the primary sent a request to an outside service, its
    /// effects left, and the backup never applied its write: the answer comes
    /// back to the dead node, and the copy never learns the request left.
    pub unapplied_call_out_left: bool,
}

/// An outcome row's predicate: whether it accepts `verdict`, a failure, with
/// what `obs` saw each leg left with, at a cut measured as `cut`.
pub type OutcomeFn = fn(&CutFacts, &ScenarioVerdict, &ObservedState) -> bool;

/// `W-split-outcome`: any verdict; a split is judged on leaks, cleanup and the
/// CDR count alone.
pub(super) fn any(_: &CutFacts, _: &ScenarioVerdict, _: &ObservedState) -> bool {
    true
}

/// `W-stall-outcome`: any verdict, so long as every request that reached a
/// living worker drew its final there.
pub(super) fn living_answered(cut: &CutFacts, _: &ScenarioVerdict, _: &ObservedState) -> bool {
    cut.living_answered
}

/// `W-unbacked-lost`: the parties timed out waiting, every request that
/// reached a living worker having drawn its final there.
pub(super) fn lost_like_a_non_ha_call(
    cut: &CutFacts,
    verdict: &ScenarioVerdict,
    _: &ObservedState,
) -> bool {
    timed_out(verdict) && cut.living_answered
}

/// `W-off-answer-lost`: a turn's request to an outside service left before
/// the backup applied its write; the parties timed out waiting for an outcome
/// that answer brings, never for the teardown, every request that reached a
/// living worker having drawn its final there.
pub(super) fn off_answer_lost(
    cut: &CutFacts,
    verdict: &ScenarioVerdict,
    _: &ObservedState,
) -> bool {
    let ScenarioVerdict::Failure { at, cause: StepError::Timeout { .. } } = verdict else {
        return false;
    };
    at.barrier != FailedAt::TORN_DOWN && cut.unapplied_call_out_left && cut.living_answered
}

/// `W-kill-effects-lost`: the backup did not hold the write of a turn taken
/// on something no peer repeats with its effects left — a response, a timer —
/// and the parties timed out waiting for them. A turn taken on an outside
/// answer is not in this window: the copy's record still awaits that answer
/// and asks for it again.
pub(super) fn effects_lost(cut: &CutFacts, verdict: &ScenarioVerdict, _: &ObservedState) -> bool {
    timed_out(verdict) && before_the_write(cut) && !cut.on_request && !cut.on_answer
}

/// `W-kill-teardown-lost`: the backup did not hold the write of a teardown
/// the primary began with its effects left; the parties timed out with every
/// leg still up holding a confirmed dialog its endpoint's own timer ends.
pub(super) fn teardown_lost(
    cut: &CutFacts,
    verdict: &ScenarioVerdict,
    obs: &ObservedState,
) -> bool {
    let ScenarioVerdict::Failure { at, cause: StepError::Timeout { .. } } = verdict else {
        return false;
    };
    before_the_write(cut)
        && at.barrier == FailedAt::TORN_DOWN
        && obs.with_snapshot(teardown_lost_in_the_window)
}

/// The parties gave up waiting.
fn timed_out(verdict: &ScenarioVerdict) -> bool {
    matches!(verdict, ScenarioVerdict::Failure { cause: StepError::Timeout { .. }, .. })
}

/// The kill came before the backup held the cut turn's write with its effects
/// left.
fn before_the_write(cut: &CutFacts) -> bool {
    !cut.held_and_left
}

/// A teardown began — a leg ended — and every leg still up holds a confirmed
/// dialog its endpoint's own timer ends.
fn teardown_lost_in_the_window(s: &StateInner) -> bool {
    let ended = s.leg_phases().any(|(_, phase)| phase == LegPhase::Terminated);
    ended && live(s).all(|(_, phase)| phase == LegPhase::Confirmed)
}

/// The legs holding a dialog, early or confirmed.
fn live(s: &StateInner) -> impl Iterator<Item = (&'static str, LegPhase)> + '_ {
    s.leg_phases().filter(|(_, phase)| matches!(phase, LegPhase::Early | LegPhase::Confirmed))
}
