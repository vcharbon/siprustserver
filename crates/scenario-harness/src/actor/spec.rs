//! The **actor-scenario adapter** (plan §4.1/§4.3/§4.4). An [`ActorScenario`]
//! DECLARES a call (actor specs + barrier plan + settle barrier + expected
//! outcome) from a borrowed [`CallEnv`], extracting OWNED state (cloned agents,
//! copied knobs, an [`InvitePlan`](crate::realcall::InvitePlan)) so the built
//! [`ActorCall`] is `Send + 'static`; [`run_actor_scenario`] drives it and maps
//! the [`CallVerdict`] onto the EXACT `Result<(), StepError>` downstream
//! contract — the `StepError` variant + `who` the load driver's classification
//! (`loadgen/src/class.rs`) buckets on.
//!
//! The `who` strings minted here are load-report sample-directory keys — see
//! `docs/todos/actor-harness-p1-contract-table.md`. Byte-for-byte:
//! - the settle verdict → `Timeout { who: "settle" }` (fixed, decision 3);
//! - `Expect::AbandonedEarly` → `Timeout { who: "alice-abandoned-after-ringing" }`;
//! - `Expect::TransferDeclined` → `UnexpectedKind { who: "refer_charlie_reject" }`.

use super::actor::{Automatics, Disposition};
use super::goals::GoalStep;
use super::state::ObservedState;
use super::{run_call_with, ActorSpec, BarrierPhase, CallPlan, CallVerdict, SettleBarrier};
use crate::realcall::{CallCtx, CallEnv, ScenarioId};
use crate::{StepError, WaiverScope};

/// A declarative multi-party call with its expected outcome — what an
/// [`ActorScenario`] builds per call.
pub struct ActorCall {
    pub actors: Vec<ActorSpec>,
    pub plan: Vec<BarrierPhase>,
    pub settle: SettleBarrier,
    pub expect: Expect,
    /// Structural RFC-audit waivers that ride this plan (ADR-0024 §6). The load
    /// lane merges them with the case-level waivers and filters findings through
    /// the `WaiverScope` path; the functional lane's `Harness::waive` consumes
    /// the same shape. Empty (the default) = no waiver.
    pub waivers: Vec<WaiverScope>,
    /// The lane-chosen stack automatics for this plan (ADR-0024 §5): plumbed
    /// onto the [`CallPlan`] by [`run_actor_scenario`]. Default = none.
    pub automatics: Automatics,
}

impl ActorCall {
    /// A plan with no waivers and default automatics — the additive base every
    /// existing scenario keeps, so only a plan that NEEDS them names them.
    pub fn new(
        actors: Vec<ActorSpec>,
        plan: Vec<BarrierPhase>,
        settle: SettleBarrier,
        expect: Expect,
    ) -> Self {
        ActorCall { actors, plan, settle, expect, waivers: Vec::new(), automatics: Automatics::default() }
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
}

/// The outcome a body DECLARES — mapped onto the linear bodies' exact
/// `Result<(), StepError>` downstream contract by [`into_result`].
#[derive(Debug, Clone, Copy)]
pub enum Expect {
    /// The happy path: every barrier held, torn down, settled → `Ok(())`.
    HappyBye,
    /// The caller's INVITE is rejected with this final (e.g. 486) — the
    /// `invite_reject` contract: `WrongStatus { who: <caller>, expected: 200,
    /// got, reason }`.
    Reject(u16),
    /// The caller abandons mid-ring (CANCEL) — the `abandon_ringing` synthetic
    /// terminal, byte-for-byte.
    AbandonedEarly,
    /// The transfer target declines the REFER'd leg — the
    /// `refer_charlie_reject` synthetic terminal, byte-for-byte.
    TransferDeclined,
    /// A **branch-aware race oracle** (C2/E5): the shape declares the SET of
    /// RFC-legal terminal outcomes and [`into_result`] maps whichever branch the
    /// observed state shows — a CANCEL×200 crossing terminates EITHER confirmed
    /// (the 200 crossed/beat the CANCEL, §9.2 — the caller ACKed then BYE'd) OR
    /// cancelled (the CANCEL won — 487+ACK). Both are legal; the load lane
    /// accepts whichever occurred. Bounded: each branch maps to a fixed variant.
    EitherOf(&'static [ExpectBranch]),
}

/// One RFC-legal terminal outcome of an [`Expect::EitherOf`] race (C2/E5). The
/// actual branch is read from the observed state by [`into_result`]; each maps
/// to a fixed, bounded downstream `Result` so the load classification stays
/// low-cardinality.
#[derive(Debug, Clone, Copy)]
pub enum ExpectBranch {
    /// The caller confirmed the call (a 2xx to its INVITE) and it tore down →
    /// `Ok(())`. Selected when the caller leg has NO non-2xx final on its
    /// establishing INVITE (the 200 crossed/beat the CANCEL, §9.2).
    Answered,
    /// The caller's establishing INVITE drew `code` (487 — the CANCEL won) →
    /// the abandoned terminal (`Timeout`, bounded — the `abandon_ringing`
    /// class). Selected when the caller leg saw `code`.
    Cancelled { code: u16 },
}

/// A scenario expressed as per-endpoint actors — the fork twin of
/// [`RealCallScenario`](crate::realcall::RealCallScenario). `Send + Sync` and
/// stateless: all per-call state is extracted (OWNED) into the returned
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
/// — the caller a terminal `Expect` is attributed to, and the §14.1 glare
/// owner. Falls back to `Disposition::Caller`, then `"alice"`, so a purely
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
            actors
                .iter()
                .find(|a| matches!(a.disposition, Disposition::Caller))
                .map(|a| a.role)
        })
        .unwrap_or("alice")
}

/// Map a finished call's [`CallVerdict`] + declared [`Expect`] onto the linear
/// body's exact `Result` contract (plan §4.3). `caller` is the originating
/// leg's role (the `who` a `Reject` terminal carries).
pub fn into_result(
    expect: Expect,
    verdict: CallVerdict,
    obs: &ObservedState,
    caller: &'static str,
) -> Result<(), StepError> {
    match verdict {
        // A fatal step / unmet barrier: the SAME StepError surfaces (an agent
        // step carries the agent's role; a barrier timeout its bounded name).
        CallVerdict::Failed(e) => Err(e),
        // The settle ceiling elapsed with open obligations: the FIXED mapping
        // (decision 3). The open-obligation names are diagnostics, never keyed.
        CallVerdict::Settle(_open) => Err(StepError::Timeout { who: "settle".to_string() }),
        CallVerdict::Ok => match expect {
            Expect::HappyBye => Ok(()),
            // The linear `invite_reject` terminal: alice expected the 200 and
            // got the reject final (`failures.rs:53`).
            Expect::Reject(code) => {
                let reason = obs.with_snapshot(|s| s.leg(caller).final_reason(code));
                match reason {
                    Some(reason) => Err(StepError::WrongStatus {
                        who: caller.to_string(),
                        expected: 200,
                        got: code,
                        reason,
                    }),
                    // The declared expect was never reached — a genuine
                    // deviation (the call "succeeded" where a reject was the
                    // point), surfaced under the caller's bounded role.
                    None => Err(StepError::UnexpectedKind {
                        who: caller.to_string(),
                        detail: format!("expected {code} reject was never observed"),
                    }),
                }
            }
            // The `abandon_ringing` synthetic terminal (`failures.rs:104`).
            Expect::AbandonedEarly => Err(StepError::Timeout {
                who: "alice-abandoned-after-ringing".to_string(),
            }),
            // The `refer_charlie_reject` synthetic terminal (`failures.rs:178`).
            Expect::TransferDeclined => Err(StepError::UnexpectedKind {
                who: "refer_charlie_reject".to_string(),
                detail: "transfer declined by charlie (603)".to_string(),
            }),
            // C2/E5 branch oracle: pick the branch the observed state shows.
            // A `Cancelled { code }` branch is selected iff the caller's
            // establishing INVITE drew that non-2xx final (the CANCEL won);
            // otherwise the `Answered` branch (the 200 crossed/beat the CANCEL,
            // §9.2 — the call confirmed and tore down normally).
            Expect::EitherOf(branches) => {
                let cancelled = branches.iter().find_map(|b| match b {
                    ExpectBranch::Cancelled { code } => obs
                        .with_snapshot(|s| s.leg(caller).saw_final(*code))
                        .then_some(*code),
                    ExpectBranch::Answered => None,
                });
                if cancelled.is_some() {
                    // The abandoned terminal — the SAME bounded class as
                    // `AbandonedEarly` (the load lane's `timeout` bucket).
                    return Err(StepError::Timeout {
                        who: "alice-abandoned-after-ringing".to_string(),
                    });
                }
                if branches.iter().any(|b| matches!(b, ExpectBranch::Answered)) {
                    return Ok(());
                }
                // Neither declared branch was observed — a genuine deviation.
                Err(StepError::UnexpectedKind {
                    who: caller.to_string(),
                    detail: "no declared EitherOf branch was observed".to_string(),
                })
            }
        },
    }
}

/// Drive one [`ActorScenario`] to its downstream `Result` contract — the actor
/// analogue of one `RealCallScenario::run` invocation. Builds the call from the
/// env, runs the joined actors + controller (teardown scopes survive a panic —
/// see [`run_call_with`]), and maps the verdict via [`into_result`].
///
/// `ctx` is the caller's per-call recorder (the load driver's — phases,
/// checkpoints, anchors and the ringing gate feed classification/sampling
/// exactly as a linear body's would).
pub async fn run_actor_scenario(
    scenario: &dyn ActorScenario,
    env: &CallEnv<'_>,
    ctx: &CallCtx,
) -> Result<(), StepError> {
    run_built_actor_call(scenario.build(env)?, env, ctx).await
}

/// Drive an ALREADY-built [`ActorCall`] — the load-lane seam: the driver builds
/// the call ONCE (to read its [`ActorCall::waivers`] for the audit), then runs
/// the same value here, so a plan is never built twice. Identical to
/// [`run_actor_scenario`] past the build.
pub async fn run_built_actor_call(
    call: ActorCall,
    env: &CallEnv<'_>,
    ctx: &CallCtx,
) -> Result<(), StepError> {
    let ActorCall { actors, plan, settle, expect, waivers: _, automatics } = call;
    // The originating leg — the role a Reject terminal is attributed to,
    // keyed on which actor's first goal originates the dialog.
    let caller = originating_role(&actors);
    let obs = ObservedState::new();
    // The deferred-auth adapter (RFC 3261 §22.2) reaches the caller's
    // establishing INVITE from the call env — `None` on every current surface
    // (no CLI flag mints one yet), so a challenge classifies unchanged.
    let verdict = run_call_with(
        CallPlan { actors, plan, settle, automatics },
        obs.clone(),
        ctx,
        STEP_TIMEOUT,
        env.challenge_responder.clone(),
    )
    .await;
    // A settle breach's open-obligation names go to the sample DETAIL channel
    // (bounded leg/kind/cseq strings from `describe_open`) — the case key stays
    // the fixed `settle@<phase>` (contract table §2).
    if let CallVerdict::Settle(open) = &verdict {
        for o in open {
            ctx.note(format!("settle: {o}"));
        }
    }
    into_result(expect, verdict, &obs, caller)
}

/// The per-barrier / per-goal-guard wait bound: `64·T1 = 32 s` — the protocol's
/// own re-emission ceiling (RFC 3261 Timer B/F/H), so a barrier never gives up
/// while a compliant retransmit could still land. One barrier spans several
/// linear steps plus their realistic dwells, so the linear per-receive timeout
/// is not the right unit; under a paused clock the bound auto-advances and a
/// stuck call fails fast either way.
pub const STEP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(32);
