//! The **actor-scenario adapter** (plan §4.1/§4.3/§4.4). An [`ActorScenario`]
//! DECLARES a call (actor specs + barrier plan + settle barrier + accepted
//! endings) from a borrowed [`CallEnv`], extracting OWNED state (cloned agents,
//! copied knobs, an [`InvitePlan`](crate::realcall::InvitePlan)) so the built
//! [`ActorCall`] is `Send + 'static`; [`run_actor_scenario`] drives it to a
//! [`ScenarioVerdict`] — the accepted branch, or where and why the call failed
//! (the load driver's classification, `loadgen/src/class.rs`, reads it).

use super::delta::AcceptedDeltaPolicy;
use super::ending::Expect;
use super::endpoint::{Automatics, Disposition};
use super::goals::GoalStep;
use super::observe::ReceptionObserver;
use super::state::{ObservedState, ReplayEntry};
use super::verdict::{verdict, ScenarioVerdict};
use super::{run_call_accepting, ActorSpec, BarrierPhase, CallPlan, CallVerdict, SettleBarrier};
use crate::realcall::{CallCtx, CallEnv, ScenarioId};
use crate::{StepError, WaiverScope};

/// A declarative multi-party call with its accepted endings — what an
/// [`ActorScenario`] builds per call.
pub struct ActorCall {
    pub actors: Vec<ActorSpec>,
    pub plan: Vec<BarrierPhase>,
    pub settle: SettleBarrier,
    /// The endings this call accepts as success ([`Expect`]).
    pub expect: Expect,
    /// Structural RFC-audit waivers that ride this plan (ADR-0024 §6). The load
    /// lane merges them with the case-level waivers and filters findings through
    /// the `WaiverScope` path; the functional lane's `Harness::waive` consumes
    /// the same shape. Empty (the default) = no waiver.
    pub waivers: Vec<WaiverScope>,
    /// The lane-chosen stack automatics for this plan (ADR-0024 §5): plumbed
    /// onto the [`CallPlan`] by [`run_actor_scenario`]. Default = none.
    pub automatics: Automatics,
    /// The controller's per-barrier / goal-guard wait ceiling for this call.
    /// `None` uses [`STEP_TIMEOUT`] (`64·T1 = 32 s`, the protocol re-emission
    /// bound). A plan whose honoured inter-message dwells span longer than that
    /// (a capture-replay honouring the wire timing) sets a larger ceiling so a
    /// long-but-healthy call does not trip `torn_down`; free under a paused clock,
    /// the honest wall duration on a real one.
    pub ceiling: Option<std::time::Duration>,
    /// The plan's accepted-delta policy (ADR-0024 §6), riding the plan like
    /// `waivers`. `None` (the default) = hook absent, behavior unchanged.
    pub delta_policy: Option<AcceptedDeltaPolicy>,
    /// The plan's reception observer, riding the plan like `delta_policy`:
    /// invoked with the typed message each time a reception goal consumes
    /// one, and unable to change the outcome. `None` (the default) = hook
    /// absent, behavior unchanged.
    pub reception_observer: Option<ReceptionObserver>,
}

impl ActorCall {
    /// A plan with no waivers, default automatics, and the standard ceiling — the
    /// additive base every existing scenario keeps, so only a plan that NEEDS them
    /// names them.
    pub fn new(
        actors: Vec<ActorSpec>,
        plan: Vec<BarrierPhase>,
        settle: SettleBarrier,
        expect: Expect,
    ) -> Self {
        ActorCall {
            actors,
            plan,
            settle,
            expect,
            waivers: Vec::new(),
            automatics: Automatics::default(),
            ceiling: None,
            delta_policy: None,
            reception_observer: None,
        }
    }

    /// Set the controller wait ceiling (see [`ActorCall::ceiling`]).
    pub fn with_ceiling(mut self, ceiling: std::time::Duration) -> Self {
        self.ceiling = Some(ceiling);
        self
    }

    /// Attach the plan's structural RFC-audit waivers (ADR-0024 §6).
    pub fn with_waivers(mut self, waivers: Vec<WaiverScope>) -> Self {
        self.waivers = waivers;
        self
    }

    /// Attach the plan's lane-chosen stack automatics (ADR-0024 §5).
    pub fn with_automatics(mut self, automatics: Automatics) -> Self {
        self.automatics = automatics;
        self
    }

    /// Attach the plan's accepted-delta policy (ADR-0024 §6).
    pub fn with_delta_policy(mut self, policy: AcceptedDeltaPolicy) -> Self {
        self.delta_policy = Some(policy);
        self
    }

    /// Attach the plan's reception observer — the observational twin of
    /// [`with_delta_policy`](Self::with_delta_policy): it sees the typed
    /// message of every goal-driven reception and decides nothing.
    pub fn with_reception_observer(mut self, observer: ReceptionObserver) -> Self {
        self.reception_observer = Some(observer);
        self
    }
}

/// A scenario expressed as per-endpoint actors. `Send + Sync` and stateless:
/// all per-call state is extracted (OWNED) into the returned
/// [`ActorCall`]. `build` is fallible for the linear bodies' guard errors
/// (e.g. `refer` bound without a charlie leg) — those `StepError`s are part of
/// the downstream contract and are returned byte-for-byte.
pub trait ActorScenario: Send + Sync {
    /// The body's intrinsic name (panic messages, direct functional use).
    fn id(&self) -> ScenarioId;
    /// Declare one call against the bound environment.
    fn build(&self, env: &CallEnv<'_>) -> Result<ActorCall, StepError>;
}

/// The role whose FIRST goal originates the dialog (`Invite`/`InviteTemplate`)
/// — the caller the accepted endings are read on, and the §14.1 glare owner. Falls back to `Disposition::Caller`, then `"alice"`, so a purely
/// reactive plan still attributes deterministically.
pub fn originating_role(actors: &[ActorSpec]) -> &'static str {
    actors
        .iter()
        .find(|a| {
            a.goals.first().is_some_and(|g| {
                matches!(g.step, GoalStep::Invite { .. } | GoalStep::InviteTemplate { .. })
            })
        })
        .map(|a| a.role)
        .or_else(|| {
            actors.iter().find(|a| matches!(a.disposition, Disposition::Caller)).map(|a| a.role)
        })
        .unwrap_or("alice")
}

/// Drive one [`ActorScenario`] to its [`ScenarioVerdict`] — the actor
/// analogue of one `RealCallScenario::run` invocation. Builds the call from the
/// env (a build guard error is [`ScenarioVerdict::build_failure`]), runs the
/// joined actors + controller (teardown scopes survive a panic — see
/// [`run_call_with`](super::run_call_with)), and reads the verdict.
///
/// `ctx` is the caller's per-call recorder (the load driver's — phases,
/// checkpoints, anchors and the ringing gate feed classification/sampling
/// exactly as a linear body's would).
pub async fn run_actor_scenario(
    scenario: &dyn ActorScenario,
    env: &CallEnv<'_>,
    ctx: &CallCtx,
) -> ScenarioVerdict {
    match scenario.build(env) {
        Ok(call) => run_built_actor_call(call, env, ctx).await,
        Err(cause) => ScenarioVerdict::build_failure(cause),
    }
}

/// Drive an ALREADY-built [`ActorCall`] — the load-lane seam: the driver builds
/// the call ONCE (to read its [`ActorCall::waivers`] for the audit), then runs
/// the same value here, so a plan is never built twice. Identical to
/// [`run_actor_scenario`] past the build.
pub async fn run_built_actor_call(
    call: ActorCall,
    env: &CallEnv<'_>,
    ctx: &CallCtx,
) -> ScenarioVerdict {
    let ActorCall {
        actors,
        plan,
        settle,
        expect,
        waivers: _,
        automatics,
        ceiling,
        delta_policy,
        reception_observer,
    } = call;
    // The originating leg — the one the accepted endings are read on, keyed on
    // which actor's first goal originates the dialog.
    let caller = originating_role(&actors);
    let expect = expect.in_context(env);
    let obs = ObservedState::new();
    // A plan whose honoured dwells span longer than the standard re-emission
    // bound raises the controller ceiling; the default is `STEP_TIMEOUT`.
    let step_timeout = ceiling.unwrap_or(STEP_TIMEOUT);
    // The deferred-auth adapter (RFC 3261 §22.2) reaches the caller's
    // establishing INVITE from the call env — `None` on every current surface
    // (no CLI flag mints one yet), so a challenge classifies unchanged.
    let run = run_call_accepting(
        CallPlan { actors, plan, settle, automatics, delta_policy, reception_observer },
        (caller, expect),
        obs.clone(),
        ctx,
        step_timeout,
        env.challenge_responder.clone(),
    )
    .await;
    // A settle breach's open-obligation names go to the sample DETAIL channel
    // (bounded leg/kind/cseq strings from `describe_open`) — the case key stays
    // the fixed `settle` barrier.
    if let CallVerdict::Settle(open) = &run {
        for o in open {
            ctx.note(format!("settle: {o}"));
        }
    }
    // Every accepted delta rides the sample DETAIL channel too — a blessed
    // substitution is never silent on any lane (a consumer buckets on it).
    for e in obs.accepted_deltas() {
        if let ReplayEntry::AcceptedDelta { leg, step, expected, observed, rule } = e {
            ctx.note(format!(
                "accepted-delta: {leg} step {step} expected {expected} observed {observed} rule {rule}"
            ));
        }
    }
    verdict(expect, run, &obs, caller)
}

/// The per-barrier / per-goal-guard wait bound: `64·T1 = 32 s` — the protocol's
/// own re-emission ceiling (RFC 3261 Timer B/F/H), so a barrier never gives up
/// while a compliant retransmit could still land. One barrier spans several
/// linear steps plus their realistic dwells, so the linear per-receive timeout
/// is not the right unit; under a paused clock the bound auto-advances and a
/// stuck call fails fast either way.
pub const STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(32);
