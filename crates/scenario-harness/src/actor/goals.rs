//! The **goal cursor** — one endpoint's scripted *intent*, shrunk to an ordered
//! list of goals with barrier guards. The reactive answering is NOT scripted
//! (it lives in [`super::actor::default_react`]); a goal is only the deliberate
//! action an endpoint takes (originate the call, hang up, transfer). A goal may
//! wait on a barrier over the observed state before it fires — and because the
//! reactor runs concurrently in the same `select!`, a goal parked on a barrier
//! never blocks the reactor from answering inbound traffic (the structural fix
//! for the cascade).

use std::time::Duration;

use tokio::time::Instant;

use super::state::{await_pred, LegPhase, ObservedState, StateInner};
use crate::StepError;

/// A barrier guard over the observed multi-endpoint state. Small enum for P0;
/// P1 adds sub-flow-conjunction guards (`merged`) for the realign parallelism.
#[derive(Debug, Clone)]
pub enum Barrier {
    /// Fire immediately (no guard).
    None,
    /// Fire once every named leg is at least `Confirmed` — the `established`
    /// guard (bob's REFER / alice's BYE wait on it).
    AllConfirmed(&'static [&'static str]),
}

impl Barrier {
    /// Whether the guard holds against a state snapshot.
    pub fn holds(&self, s: &StateInner) -> bool {
        match self {
            Barrier::None => true,
            Barrier::AllConfirmed(roles) => {
                roles.iter().all(|r| s.leg_at_least(r, LegPhase::Confirmed))
            }
        }
    }

    /// A bounded label for the barrier-timeout `StepError::who` (never free-form).
    pub fn name(&self) -> &'static str {
        match self {
            Barrier::None => "none",
            Barrier::AllConfirmed(_) => "established",
        }
    }
}

/// One deliberate action an endpoint drives. `Copy` so the reactor's `select!`
/// arm can lift it out of the cursor without a borrow escaping the future.
#[derive(Debug, Clone, Copy)]
pub enum GoalStep {
    /// Originate the initial INVITE to a callee role (the caller's first goal).
    Invite { callee: &'static str },
    /// Hang up — send a BYE on the confirmed dialog.
    Bye,
}

/// A goal: its barrier guard + the step it drives once the guard holds.
#[derive(Debug, Clone)]
pub struct Goal {
    pub guard: Barrier,
    pub step: GoalStep,
}

impl Goal {
    pub fn new(guard: Barrier, step: GoalStep) -> Self {
        Self { guard, step }
    }
}

/// A sequential cursor over an endpoint's goals — the next un-fired goal is the
/// one the reactor's goal arm waits on.
pub struct GoalCursor {
    goals: Vec<Goal>,
    cursor: usize,
}

impl GoalCursor {
    pub fn new(goals: Vec<Goal>) -> Self {
        Self { goals, cursor: 0 }
    }

    /// Whether an un-fired goal remains.
    pub fn has_pending(&self) -> bool {
        self.cursor < self.goals.len()
    }

    /// Whether every goal has fired.
    pub fn is_exhausted(&self) -> bool {
        self.cursor >= self.goals.len()
    }

    fn pending(&self) -> Option<&Goal> {
        self.goals.get(self.cursor)
    }

    /// Resolve once the next pending goal's guard holds (immediately for
    /// [`Barrier::None`]), returning the step to drive — bounded by `timeout` so
    /// a genuinely stuck guard fails the actor rather than hanging. Parks forever
    /// if the cursor is exhausted (the reactor gates this arm on
    /// [`has_pending`](Self::has_pending), so the park is never actually polled).
    pub async fn next_ready(
        &self,
        obs: &ObservedState,
        timeout: Duration,
    ) -> Result<GoalStep, StepError> {
        let Some(goal) = self.pending() else {
            return std::future::pending().await;
        };
        let deadline = Instant::now() + timeout;
        await_pred(obs, goal.guard.name(), |s| goal.guard.holds(s), deadline).await?;
        Ok(goal.step)
    }

    /// Advance past the goal that just fired.
    pub fn advance(&mut self) {
        self.cursor += 1;
    }
}
