//! Real-call scenarios — the choreography shared by the load generator
//! (`crates/loadgen`, fallible: a deviation is a counted [`StepError`]) and the
//! in-process functional leak gate ([`run_asserting`], strict: a deviation is a
//! `panic!`). Each scenario drives one full call through the SUT using the
//! fallible `try_*` agent surface and registers its dialog in a [`CallScope`] so
//! a failure mid-flow still tears the call down — the invariant that keeps a
//! failed call from leaking SUT state under load AND lets the same flow serve as
//! a deterministic no-leak assertion in CI.
//!
//! A scenario is portable to load/endurance **iff** it lives here: it must be a
//! "real call" expressible against `try_*` with no byte-exact assertions. The
//! raw-bytes torture cases (`dsl::Scenario`) are deliberately NOT here.

use std::sync::Arc;

use async_trait::async_trait;

use crate::{Dialog, StepError, ANSWER_SDP, OFFER_SDP};

mod env;
mod scope;
pub mod scenarios;

pub use env::{CallCtx, CallEnv, CorrelationStamp};
pub use scope::CallScope;

/// Stable scenario name — the Prometheus `scenario` label and report dir name.
pub type ScenarioId = &'static str;

/// A real-call scenario: one full call through the SUT. Implementations are
/// `Send + Sync` and stateless (all per-call state is in the args), so the same
/// instance can be shared across the load fleet and reused by a functional test.
#[async_trait]
pub trait RealCallScenario: Send + Sync {
    /// Stable identifier (label / report dir).
    fn id(&self) -> ScenarioId;
    /// Whether this scenario needs a third (transfer-target) leg bound.
    fn needs_charlie(&self) -> bool {
        false
    }
    /// Whether this call is an emergency (Resource-Priority `esnet.0`) — the SUT
    /// force-admits it under overload, so it must never be shed. Default: no.
    fn emergency(&self) -> bool {
        false
    }
    /// Drive one call. Return `Ok` on the happy path or a [`StepError`] on an
    /// expected failure; the driver/leak-gate tears the call down regardless.
    async fn run(
        &self,
        env: &CallEnv<'_>,
        scope: &CallScope,
        ctx: &CallCtx,
    ) -> Result<(), StepError>;
}

/// Run a scenario once as an in-process functional leak gate and **return** its
/// outcome (no panic). Drives the call with a fresh per-call `CallScope`/`CallCtx`,
/// then ALWAYS tears the call down (release any dialog on the SUT) before
/// returning — so the caller can assert on the SUT (`active_calls() == 0`,
/// `assert_fully_reaped()`) with no leaked dialog left behind, whatever the
/// outcome. This is the runner for the **voluntarily-failing** scenarios (an
/// `Err` is the EXPECTED outcome): the leak-gate's most valuable assertion is
/// that a FAILED call still cleans up, so it must observe the `Err` rather than
/// `panic!` on it (which [`run_asserting`] does).
///
/// Use a fresh per-call `token` (the correlation value). Realistic dwell timing
/// rides [`CallEnv::for_functional`]; under a paused clock it is free.
pub async fn run_collecting(
    scenario: &dyn RealCallScenario,
    env: &CallEnv<'_>,
) -> Result<(), StepError> {
    let scope = CallScope::new();
    let ctx = CallCtx::new();
    let result = scenario.run(env, &scope, &ctx).await;
    // Tear down first (release any dialog on the SUT), then hand the result back —
    // so a failing assertion in the caller never leaves a leaked dialog that
    // contaminates a later test.
    scope.teardown().await;
    result
}

/// Run a scenario as an **in-process functional leak gate**: drive it once, tear
/// the call down, and `panic!` if it did not reach the happy path — the strict
/// (assert-on-deviation) analogue of the load driver's count-and-classify. After
/// this returns the caller asserts on the SUT (e.g. `active_calls() == 0`,
/// `assert_fully_reaped()`), so a scenario that leaks state is caught
/// deterministically without a cluster or a soak run.
///
/// The happy-path sibling of [`run_collecting`]: it delegates the run+teardown to
/// it and `panic!`s on the `Err` a happy-path scenario must never return.
///
/// Use a fresh per-call `token` (the correlation value). Realistic dwell timing
/// rides [`CallEnv::for_functional`]; under a paused clock it is free.
pub async fn run_asserting(scenario: &dyn RealCallScenario, env: &CallEnv<'_>) {
    if let Err(e) = run_collecting(scenario, env).await {
        panic!("real-call scenario `{}` failed: {e:?}", scenario.id());
    }
}

/// Shared INVITE/180/200/ACK establishment — the fallible analogue of
/// `callflow::establish`. Registers the early CANCEL handle, then the confirmed
/// dialog, in `scope`, and marks the `time_to_200` checkpoint. Returns the
/// caller's confirmed [`Dialog`].
pub async fn establish(
    env: &CallEnv<'_>,
    scope: &CallScope,
    ctx: &CallCtx,
) -> Result<Dialog, StepError> {
    let inv = env.alice.invite(env.bob).with_sdp(OFFER_SDP).through(env.via);
    let mut call = env.prepare_invite(inv).send().await;
    scope.set_early(call.cancel_handle());

    // The call may be ADMITTED (the SUT forwards our INVITE to bob) or SHED
    // (under overload the SUT replies a final — e.g. 503 — directly to alice, and
    // bob never sees it; an emergency call is force-admitted and never shed). We
    // cannot know which up front, so race bob's inbound INVITE against alice's
    // response. `try_expect(180)` skips the auto-100 Trying and surfaces a shed
    // final as `WrongStatus { got: 503 }`, which the driver buckets as
    // `status_503`. (Only an admitted call reaches bob; an admitted call's own
    // 180 arrives only after we answer bob below, so the second arm resolving is
    // unambiguously the SUT's own final to alice.)
    let mut uas = tokio::select! {
        biased;
        bob = env.bob.try_receive("INVITE") => bob?,
        shed = call.try_expect(180) => {
            let e = shed.err().unwrap_or_else(|| StepError::UnexpectedKind {
                who: "alice".to_string(),
                detail: "early 180 before bob saw the INVITE".to_string(),
            });
            // A real FINAL response (status ≥ 200, e.g. a 503 overload shed)
            // completes the INVITE transaction — there is nothing to CANCEL/BYE,
            // and CANCELing an already-answered INVITE just churns the SUT. Mark
            // the scope terminated so teardown is a no-op. A non-180 PROVISIONAL
            // (183 early media) is NOT a final — leave the scope Early so a still-
            // pending INVITE is CANCELed; likewise a bare timeout (no response).
            if matches!(&e, StepError::WrongStatus { got, .. } if *got >= 200) {
                scope.mark_terminated();
            }
            return Err(e);
        }
    };
    uas.respond(180, "Ringing").await;
    // The 180 is a NON-PRACK provisional: best-effort, so it MAY be lost
    // end-to-end — that is EXPECTED, not a call failure (RFC 3261 §13.2.2.4: the
    // dialog/ACK rides the 2xx). So a TIMEOUT waiting for it is tolerated (the 18x
    // was lost → proceed to the answer) and only COUNTED; the driver gates the
    // cross-call 18x delivery rate (>99%) instead of failing the call. A genuinely
    // wrong message (not a timeout) is still an error. We keep the strict ordering
    // — the UAS answers only AFTER this resolves — so a late/reordered 180 can
    // never strand in the caller's inbox (which would break the later BYE).
    let saw_ringing = match call.try_expect(180).await {
        Ok(_) => true,
        Err(StepError::Timeout { .. }) => false,
        Err(e) => return Err(e),
    };
    ctx.mark_ringing(saw_ringing);
    // Realistic ring: dwell the early dialog before answering. Alice is parked
    // (not awaiting a receive) for the duration, so this just ages the early
    // dialog on the SUT; it stays well inside Timer C (180 s).
    if !env.ring_delay.is_zero() {
        tokio::time::sleep(env.ring_delay).await;
    }
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.try_expect(200).await?;
    ctx.checkpoint("time_to_200");

    let dialog = call.ack().await;
    scope.set_confirmed(dialog.clone());
    env.bob.try_receive("ACK").await?;
    ctx.phase("connected");
    Ok(dialog)
}

/// Shared BYE/200 teardown — the fallible analogue of `callflow::hangup`. On
/// success marks the scope terminated (so the driver's teardown is a no-op) and
/// records `time_to_bye_200`.
pub async fn hangup(
    env: &CallEnv<'_>,
    scope: &CallScope,
    dialog: &mut Dialog,
    ctx: &CallCtx,
) -> Result<(), StepError> {
    let mut bye = dialog.bye().await;
    scope.set_confirmed(dialog.clone()); // refresh CSeq so any teardown BYE stays valid
    env.bob.try_receive("BYE").await?.respond(200, "OK").await;
    bye.try_expect(200).await?;
    scope.mark_terminated();
    ctx.checkpoint("time_to_bye_200");
    ctx.phase("bye_200");
    Ok(())
}

/// Wraps any scenario as an EMERGENCY call: identical flow, but it stamps
/// `Resource-Priority: esnet.0` (so the SUT force-admits it under overload) and
/// reports under a distinct id, so the report cleanly splits the force-admitted
/// emergency traffic from the sheddable non-emergency traffic.
pub struct AsEmergency {
    id: ScenarioId,
    inner: Arc<dyn RealCallScenario>,
}

impl AsEmergency {
    pub fn wrap(id: ScenarioId, inner: Arc<dyn RealCallScenario>) -> Arc<dyn RealCallScenario> {
        Arc::new(Self { id, inner })
    }
}

#[async_trait]
impl RealCallScenario for AsEmergency {
    fn id(&self) -> ScenarioId {
        self.id
    }
    fn needs_charlie(&self) -> bool {
        self.inner.needs_charlie()
    }
    fn emergency(&self) -> bool {
        true
    }
    async fn run(&self, env: &CallEnv<'_>, scope: &CallScope, ctx: &CallCtx) -> Result<(), StepError> {
        self.inner.run(env, scope, ctx).await
    }
}
