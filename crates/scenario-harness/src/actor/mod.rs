//! Per-endpoint **actor** test harness — an alternative executor for the
//! portable multi-party real-call scenarios ([`crate::realcall`]) that replaces
//! linear choreography with N autonomous per-endpoint UA actors coordinated by
//! a controller through observed-state barriers, with the verdict gated by an
//! acknowledgement ledger behind a 32 s settle barrier.
//!
//! Two endurance failure modes shape the design: a dropped datagram must never
//! cascade into a total-call failure (the recv-sequence is NOT the control
//! flow), and the verdict must never race the protocol's own re-emission of a
//! lost message. Both dissolve here: the reactor stays reactive so a
//! late/reordered/retransmitted datagram is always consumed, and the settle
//! barrier holds the verdict until the ledger closes (every in-dialog request
//! acknowledged) or the 32 s ceiling elapses.
//!
//! # Concurrency model — joined futures on ONE task (no spawn)
//!
//! Actors are concurrent *futures* joined within the one per-call task via a
//! [`FuturesUnordered`], NOT `tokio::spawn`ed — for determinism under the paused
//! clock and no `'static` gymnastics. [`drive_actors`] `?`s the first fatal
//! actor error and otherwise parks (an actor reaching its exit cleanly must NOT
//! collapse the join). Everything is `Send`; both lanes use plain `tokio::time`
//! (deliberately no settle-driver abstraction).
//!
//! Module map: the declarative vocabulary is [`endpoint`] + [`goals`] +
//! [`delta`]; the live loop is [`runner`] with its arms in [`react`] /
//! [`response`] / [`answer`] / [`drive`] / [`originate`] / [`script`] /
//! [`accept_delta`]; the verdict machinery is [`state`] + [`ledger`] +
//! [`settle`]; scenario surfaces are [`spec`] + [`scenarios`].

mod accept_delta;
mod answer;
mod delta;
mod drive;
mod endpoint;
mod goals;
mod ledger;
mod originate;
mod react;
mod response;
mod runner;
pub mod scenarios;
mod script;
mod settle;
mod spec;
mod state;

use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use futures::FutureExt;

use crate::realcall::{CallCtx, CallScope, ChallengeResponder};
use crate::StepError;

pub use endpoint::{
    ActorSpec, Automatics, CtxFeed, Disposition, Feed, MediaState, SUBFLOW_EARLY, SUBFLOW_REALIGN,
    SUBFLOW_RENEG, SUBFLOW_REFER,
};
pub use runner::{run_actor, ActorState};

pub use delta::{
    AcceptedDelta, AcceptedDeltaPolicy, DeltaContext, DeltaDecision, DeltaReaction, DialogSnapshot,
    ExpectedStimulus, ObservedStimulus,
};
pub use goals::{Barrier, BodyExpect, EarlyId, FinalAssert, Goal, GoalCursor, GoalStep, RequestKind};
pub use ledger::{ObligationKey, ObligationKind, ObligationLedger};
pub use settle::{SettleBarrier, SettleVerdict, T1};
pub use spec::{
    into_result, originating_role, run_actor_scenario, run_built_actor_call, ActorCall,
    ActorScenario, Expect, ExpectBranch, STEP_TIMEOUT,
};
pub use state::{
    await_pred, LegObservation, LegPhase, Observation, ObservedState, RecordedFinal, ReplayEntry,
    ResponseFact, StateInner, SubflowState,
};

/// One phase barrier in the controller's plan — a named predicate over the
/// observed state that must hold (bounded by the controller's `step_timeout`)
/// before the call proceeds. The name is a bounded label for the timeout
/// `StepError`.
pub struct BarrierPhase {
    name: &'static str,
    pred: Box<dyn Fn(&StateInner) -> bool + Send + Sync>,
}

/// Build a [`BarrierPhase`] from a name + predicate.
pub fn phase(
    name: &'static str,
    pred: impl Fn(&StateInner) -> bool + Send + Sync + 'static,
) -> BarrierPhase {
    BarrierPhase { name, pred: Box::new(pred) }
}

/// The verdict of one actor-driven call.
#[derive(Debug)]
pub enum CallVerdict {
    /// Every phase barrier held, the call tore down, and the ledger settled.
    Ok,
    /// A fatal step / barrier-timeout aborted the call (an actor error or an
    /// unmet phase barrier).
    Failed(StepError),
    /// The call tore down but the settle barrier's ceiling elapsed with
    /// obligations still open — names each one.
    Settle(Vec<String>),
}

impl CallVerdict {
    /// Whether the call reached the fully-settled happy path.
    pub fn is_ok(&self) -> bool {
        matches!(self, CallVerdict::Ok)
    }
}

/// A declarative multi-party call: the actor specs + the barrier plan + the
/// settle barrier. The runner ([`run_call`]) turns it into joined actor futures
/// driven to a [`CallVerdict`].
pub struct CallPlan {
    pub actors: Vec<ActorSpec>,
    pub plan: Vec<BarrierPhase>,
    pub settle: SettleBarrier,
    /// The lane-chosen stack automatics for scripted endpoints — emitted
    /// identically on every lane (the cross-lane behavior contract).
    pub automatics: Automatics,
    /// The plan's accepted-delta policy (ADR-0024 §6): consulted by every
    /// actor when a due reception expectation meets a non-matching but
    /// classifiable inbound. `None` (the default) = hook absent, behavior
    /// unchanged.
    pub delta_policy: Option<AcceptedDeltaPolicy>,
}

/// The controller: owns the shared observed state, the barrier plan, the settle
/// barrier, and each actor's teardown scope. Drives the phase barriers → the
/// `torn_down` barrier → the settle barrier to a verdict, with the actor
/// reactors running concurrently throughout.
struct CallController {
    obs: ObservedState,
    plan: Vec<BarrierPhase>,
    settle: SettleBarrier,
    /// The per-barrier wait bound (the same 32 s ceiling the actors' goal
    /// guards use — [`spec::STEP_TIMEOUT`]); a barrier that never holds fails
    /// the call rather than hanging.
    step_timeout: Duration,
}

impl CallController {
    /// Drive the plan barriers, then wait for teardown, then run the settle
    /// barrier — the reactors stay alive throughout (this future runs in the
    /// same `select!` as [`drive_actors`], so a re-emitted request is consumed
    /// and acked DURING settle).
    async fn drive_to_verdict(&self) -> CallVerdict {
        for phase in &self.plan {
            let deadline = tokio::time::Instant::now() + self.step_timeout;
            let held = await_pred(&self.obs, phase.name, |s| (phase.pred)(s), deadline).await;
            if let Err(e) = held {
                return CallVerdict::Failed(e);
            }
        }
        // Teardown: every leg terminated. Bounded separately (teardown can take
        // the whole flow after the last phase barrier).
        let deadline = tokio::time::Instant::now() + self.step_timeout;
        if let Err(e) =
            await_pred(&self.obs, "torn_down", |s| s.all_terminated(), deadline).await
        {
            return CallVerdict::Failed(e);
        }
        match self.settle.wait(&self.obs).await {
            SettleVerdict::Ok => CallVerdict::Ok,
            SettleVerdict::Fail(open) => CallVerdict::Settle(open),
        }
    }
}

/// Join the actor futures on ONE task. Returns the FIRST fatal actor error;
/// once every actor has resolved cleanly it parks forever (`pending`), so the
/// controller's verdict — not a cleanly-finished actor — decides the call.
async fn drive_actors(
    mut actors: FuturesUnordered<impl std::future::Future<Output = Result<(), StepError>>>,
) -> StepError {
    while let Some(r) = actors.next().await {
        if let Err(e) = r {
            return e;
        }
    }
    std::future::pending().await
}

/// Run one declarative [`CallPlan`] to a verdict with its own fresh observed
/// state + per-call recorder — the self-contained (SUT-less toy call) form.
/// Adapter surfaces use [`run_call_with`] to share the driver's `CallCtx` and
/// read the observed state after the verdict.
pub async fn run_call(call: CallPlan, step_timeout: Duration) -> CallVerdict {
    let ctx = CallCtx::new();
    run_call_with(call, ObservedState::new(), &ctx, step_timeout, None).await
}

/// Run one declarative [`CallPlan`] to a verdict over a caller-provided
/// observed state (readable afterwards — the `Expect::Reject` mapping) and
/// per-call recorder (the load driver's `CallCtx`). Builds one teardown scope
/// per actor and the joined actor futures; races the controller's verdict
/// against them; then tears down every scope (a no-op on a clean call,
/// best-effort CANCEL/BYE on an aborted one).
///
/// `challenge_responder` is the per-call deferred-auth adapter (RFC 3261 §22.2,
/// [`CallEnv::challenge_responder`](crate::realcall::CallEnv)) — wired onto each
/// actor so the caller's establishing INVITE honours a `401`/`407` challenge;
/// `None` (the default / the toy call) keeps a challenge classified as
/// `status_401/407` unchanged.
///
/// **Panic-safe:** the scopes are owned HERE, outside a `catch_unwind` around
/// the drive, so a panicking actor still gets its call torn down (no leaked
/// dialog on the SUT) before the panic resumes to the caller's own
/// `catch_unwind` (the load driver's per-call boundary, which classifies it).
pub async fn run_call_with(
    call: CallPlan,
    obs: ObservedState,
    ctx: &CallCtx,
    step_timeout: Duration,
    challenge_responder: Option<Arc<dyn ChallengeResponder>>,
) -> CallVerdict {
    let mut scopes = Vec::with_capacity(call.actors.len());
    let mut states = Vec::with_capacity(call.actors.len());
    let automatics = call.automatics;
    for spec in call.actors {
        let scope = Arc::new(CallScope::new());
        states.push(ActorState::from_spec(
            spec,
            obs.clone(),
            scope.clone(),
            ctx,
            step_timeout,
            challenge_responder.clone(),
            automatics,
            call.delta_policy.clone(),
        ));
        scopes.push(scope);
    }

    let controller = CallController {
        obs: obs.clone(),
        plan: call.plan,
        settle: call.settle,
        step_timeout,
    };

    let drive = async {
        let actors: FuturesUnordered<_> = states.into_iter().map(run_actor).collect();
        tokio::select! {
            v = controller.drive_to_verdict() => v,
            e = drive_actors(actors) => CallVerdict::Failed(e),
        }
    };
    let result = std::panic::AssertUnwindSafe(drive).catch_unwind().await;

    // The loser future is dropped; teardown acts on whatever each scope last
    // registered (Terminated → no-op on the happy path) — including after a
    // caught panic, so a panicking actor never leaks SUT state.
    for scope in &scopes {
        scope.teardown().await;
    }
    match result {
        Ok(verdict) => verdict,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[cfg(test)]
mod tests;
