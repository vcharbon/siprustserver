//! The **actor-scenario adapter** — the trait fork beside
//! [`RealCallScenario`](crate::realcall::RealCallScenario) (plan §4.1/§4.3/§4.4).
//! An [`ActorScenario`] DECLARES a call (actor specs + barrier plan + settle
//! barrier + expected outcome) from a borrowed [`CallEnv`], extracting OWNED
//! state (cloned agents, copied knobs, an [`InvitePlan`](crate::realcall::InvitePlan))
//! so the built [`ActorCall`] is `Send + 'static`; [`run_actor_scenario`] drives
//! it and maps the [`CallVerdict`] back onto the EXACT `Result<(), StepError>`
//! contract the linear body had — same variant, same `who` — so the load
//! driver's classification (`loadgen/src/class.rs`) is untouched.
//!
//! The `who` strings minted here are load-report sample-directory keys — see
//! `docs/todos/actor-harness-p1-contract-table.md`. Byte-for-byte:
//! - the settle verdict → `Timeout { who: "settle" }` (fixed, decision 3);
//! - `Expect::AbandonedEarly` → `Timeout { who: "alice-abandoned-after-ringing" }`;
//! - `Expect::TransferDeclined` → `UnexpectedKind { who: "refer_charlie_reject" }`.

use super::actor::Disposition;
use super::state::ObservedState;
use super::{run_call_with, ActorSpec, BarrierPhase, CallPlan, CallVerdict, SettleBarrier};
use crate::realcall::{CallCtx, CallEnv, ScenarioId};
use crate::StepError;

/// A declarative multi-party call with its expected outcome — what an
/// [`ActorScenario`] builds per call.
pub struct ActorCall {
    pub actors: Vec<ActorSpec>,
    pub plan: Vec<BarrierPhase>,
    pub settle: SettleBarrier,
    pub expect: Expect,
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
    let ActorCall { actors, plan, settle, expect } = scenario.build(env)?;
    // The originating leg — the role a Reject terminal is attributed to.
    let caller = actors
        .iter()
        .find(|a| matches!(a.disposition, Disposition::Caller))
        .map(|a| a.role)
        .unwrap_or("alice");
    let obs = ObservedState::new();
    let verdict =
        run_call_with(CallPlan { actors, plan, settle }, obs.clone(), ctx, STEP_TIMEOUT).await;
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
