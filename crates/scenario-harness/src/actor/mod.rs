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
//! acknowledged) or the 32 s ceiling elapses. The call then succeeds under one
//! of the endings its scenario accepts ([`ending`]), or fails at a named place
//! ([`verdict`]).
//!
//! # Concurrency model — joined futures on ONE task (no spawn)
//!
//! Actors are concurrent *futures* joined within the one per-call task via a
//! [`FuturesUnordered`], NOT `tokio::spawn`ed — for determinism under the paused
//! clock and no `'static` gymnastics. `drive_actors` `?`s the first fatal
//! actor error and otherwise parks (an actor reaching its exit cleanly must NOT
//! collapse the join). Everything is `Send`; both lanes use plain `tokio::time`
//! (deliberately no settle-driver abstraction).
//!
//! # Endpoints may host SEVERAL actors ([`shared_endpoint`])
//!
//! Actors whose [`ActorSpec::agent`] is the same UA stack share one endpoint —
//! a peer socket that both originates a call and receives one (an application
//! server looping a call back to it). That endpoint gets ONE receive pump,
//! joined with the actors here, which demultiplexes each inbound to its owner:
//! by dialog identity, and for an initial INVITE by the actors'
//! [`ActorSpec::claim`] rules. An actor that owns its endpoint outright pulls
//! [`Agent::recv_any`](crate::Agent::recv_any) itself, unchanged.
//!
//! Module map: the declarative vocabulary is `endpoint` + `goals` +
//! `delta` + `observe`; the live loop is `runner` with its arms in
//! `react` / `response` / `answer` / `drive` / `originate` /
//! `script` / `select` / `accept_delta` and its inbound fan-out in
//! [`shared_endpoint`]; the verdict machinery is `state` + `ledger` +
//! `settle` + `ending` + `verdict`; scenario surfaces are `spec` +
//! [`scenarios`].

mod accept_delta;
mod answer;
mod delta;
mod drive;
mod ending;
mod endpoint;
mod goals;
mod ledger;
mod observe;
mod originate;
mod react;
mod response;
mod runner;
pub mod scenarios;
mod script;
mod select;
mod settle;
pub mod shared_endpoint;
mod spec;
mod state;
mod verdict;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use futures::FutureExt;

use crate::realcall::{CallCtx, CallScope, ChallengeResponder};
use crate::StepError;

pub use endpoint::{
    ActorSpec, Automatics, CtxFeed, Disposition, Feed, MediaState, SUBFLOW_EARLY, SUBFLOW_REALIGN,
    SUBFLOW_REFER, SUBFLOW_RENEG,
};
pub use runner::{run_actor, ActorFailure, ActorState};
pub use shared_endpoint::{EndpointHandle, EndpointPump, Inbox};

pub use delta::{
    AcceptedDelta, AcceptedDeltaPolicy, DeltaContext, DeltaDecision, DeltaReaction, DialogSnapshot,
    ExpectedStimulus, ObservedStimulus,
};
pub use ending::{Branch, Ending, Expect};
pub use goals::{
    Barrier, BodyExpect, EarlyId, FinalAssert, Goal, GoalCursor, GoalStep, RequestKind,
};
pub use ledger::{ObligationKey, ObligationKind, ObligationLedger};
pub use observe::{ReceivedMessage, ReceptionContext, ReceptionObserver};
pub use settle::{SettleBarrier, SettleVerdict, T1};
pub use spec::{
    originating_role, run_actor_scenario, run_built_actor_call, ActorCall, ActorScenario,
    STEP_TIMEOUT,
};
pub use state::{
    await_pred, LegObservation, LegPhase, Observation, ObservedState, RecordedFinal, ReplayEntry,
    ResponseFact, StateInner, SubflowState,
};
pub use verdict::{verdict, FailedAt, ScenarioVerdict};

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
    /// Every phase barrier held (or the caller ended on an accepted final), the
    /// call tore down, and the ledger settled.
    Ok,
    /// A fatal step / barrier-timeout aborted the call (an actor error or an
    /// unmet phase barrier), at the named place.
    Failed(FailedAt, StepError),
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
    /// The plan's reception observer: invoked by every actor with the typed
    /// message each time one of its reception goals consumes one. Purely
    /// observational. `None` (the default) = hook absent, behavior unchanged.
    pub reception_observer: Option<ReceptionObserver>,
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
    /// The barrier being waited on, as an index: a plan phase, then
    /// [`TORN_DOWN`], then [`SETTLE`] ([`Self::barrier_name`]). Written by the
    /// controller, read when an actor fails — both on the call's one task, so a
    /// relaxed atomic (it keeps the call future `Send`) is all it needs.
    pending: AtomicUsize,
}

/// The teardown barrier: every leg terminated.
const TORN_DOWN: &str = "torn_down";
/// The acknowledgement-gated settle barrier.
const SETTLE: &str = "settle";

impl CallController {
    /// The name of the barrier at `index` (see [`Self::pending`]).
    fn barrier_name(&self, index: usize) -> &'static str {
        match self.plan.get(index) {
            Some(phase) => phase.name,
            None if index == self.plan.len() => TORN_DOWN,
            None => SETTLE,
        }
    }

    /// The barrier pending right now.
    fn pending_barrier(&self) -> &'static str {
        self.barrier_name(self.pending.load(Ordering::Relaxed))
    }

    /// Drive the plan barriers, then wait for teardown, then run the settle
    /// barrier — the reactors stay alive throughout (this future runs in the
    /// same `select!` as [`drive_actors`], so a re-emitted request is consumed
    /// and acked DURING settle). A caller that ended on an accepted final
    /// releases every remaining plan barrier; teardown and settle still hold.
    async fn drive_to_verdict(&self) -> CallVerdict {
        for (i, phase) in self.plan.iter().enumerate() {
            self.pending.store(i, Ordering::Relaxed);
            let deadline = tokio::time::Instant::now() + self.step_timeout;
            let held = |s: &StateInner| (phase.pred)(s) || s.accepted_ending();
            if let Err(e) = await_pred(&self.obs, phase.name, held, deadline).await {
                return CallVerdict::Failed(FailedAt::at_barrier(phase.name), e);
            }
        }
        // Teardown: every leg terminated. Bounded separately (teardown can take
        // the whole flow after the last phase barrier).
        self.pending.store(self.plan.len(), Ordering::Relaxed);
        let deadline = tokio::time::Instant::now() + self.step_timeout;
        if let Err(e) = await_pred(&self.obs, TORN_DOWN, |s| s.all_terminated(), deadline).await {
            return CallVerdict::Failed(FailedAt::at_barrier(TORN_DOWN), e);
        }
        self.pending.store(self.plan.len() + 1, Ordering::Relaxed);
        match self.settle.wait(&self.obs).await {
            SettleVerdict::Ok => CallVerdict::Ok,
            SettleVerdict::Fail(open) => CallVerdict::Settle(open),
        }
    }
}

/// Join the actor futures on ONE task. Returns the FIRST fatal actor failure;
/// once every actor has resolved cleanly it parks forever (`pending`), so the
/// controller's verdict — not a cleanly-finished actor — decides the call.
async fn drive_actors(
    mut actors: FuturesUnordered<impl std::future::Future<Output = Result<(), ActorFailure>>>,
) -> ActorFailure {
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
/// observed state (readable afterwards — the accepted-ending verdict) and
/// per-call recorder (the load driver's `CallCtx`), with no accepted ending: a
/// non-2xx final on a caller's establishing INVITE with goals still pending
/// fails the call. Builds one teardown scope
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
    run_call_accepting(call, ("", Expect::NONE), obs, ctx, step_timeout, challenge_responder).await
}

/// [`run_call_with`] where the `(caller role, accepted endings)` pair lets the
/// caller end its call cleanly on an accepted establishing-INVITE final
/// ([`Expect::accepts_final`]): its remaining goals and plan barriers are
/// dropped, while teardown and settle still hold the verdict.
pub(crate) async fn run_call_accepting(
    call: CallPlan,
    (caller, accepted): (&'static str, Expect),
    obs: ObservedState,
    ctx: &CallCtx,
    step_timeout: Duration,
    challenge_responder: Option<Arc<dyn ChallengeResponder>>,
) -> CallVerdict {
    let mut scopes = Vec::with_capacity(call.actors.len());
    let mut states = Vec::with_capacity(call.actors.len());
    let automatics = call.automatics;
    // Actors sharing ONE UA stack get ONE receive pump that demultiplexes to
    // them; an actor that owns its endpoint keeps pulling `recv_any` itself.
    let (pumps, mut seats) = shared_endpoint::wire_shared_endpoints(&call.actors, &obs);
    for spec in call.actors {
        let scope = Arc::new(CallScope::new());
        let seat = seats.remove(spec.role);
        let accepts = if spec.role == caller { accepted } else { Expect::NONE };
        let mut state = ActorState::from_spec(
            spec,
            obs.clone(),
            scope.clone(),
            ctx,
            step_timeout,
            challenge_responder.clone(),
            automatics,
            call.delta_policy.clone(),
            call.reception_observer.clone(),
        )
        .accepting(accepts);
        if let Some((inbox, handle)) = seat {
            state = state.on_shared_endpoint(inbox, handle);
        }
        states.push(state);
        scopes.push(scope);
    }

    let controller = CallController {
        obs: obs.clone(),
        plan: call.plan,
        settle: call.settle,
        step_timeout,
        pending: AtomicUsize::new(0),
    };

    let drive = async {
        // The pumps are joined with the actors: one task, one `select!` — a pump
        // is as reactive as the actors it feeds, and a fatal receive surfaces
        // through the same arm an actor's would.
        let actors: FuturesUnordered<futures::future::BoxFuture<'_, Result<(), ActorFailure>>> =
            states.into_iter().map(|s| run_actor(s).boxed()).collect();
        for pump in pumps {
            let who = pump.role();
            actors.push(
                pump.run()
                    .map(move |r| r.map_err(|cause| ActorFailure { who, goal: None, cause }))
                    .boxed(),
            );
        }
        tokio::select! {
            v = controller.drive_to_verdict() => v,
            f = drive_actors(actors) => {
                let at = FailedAt { who: f.who, goal: f.goal, barrier: controller.pending_barrier() };
                CallVerdict::Failed(at, f.cause)
            }
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
