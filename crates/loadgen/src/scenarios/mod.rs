//! The load scenarios — fallible reuse of the functional choreography.
//!
//! Each scenario drives a full call through the SUT using the `try_*` agent
//! methods (so an expected failure is a [`StepError`] the driver counts, not a
//! panic) and registers its dialog state in the [`CallScope`] so a failure mid-
//! flow still tears the call down. The sends (`respond`/`ack`) stay infallible;
//! a genuine transport error there is caught by the driver's `catch_unwind`.
//!
//! v1 set: [`basic_call`], [`reinvite`], [`refer`] (blind transfer), and
//! [`options_hold`] (OPTIONS-keepalive long hold).

use std::sync::Arc;

use async_trait::async_trait;
use scenario_harness::{Dialog, StepError, ANSWER_SDP, OFFER_SDP};

use crate::ctx::{CallCtx, CallEnv};
use crate::scope::CallScope;

pub mod basic_call;
pub mod failures;
pub mod options_hold;
pub mod refer;
pub mod reinvite;

/// Stable scenario name — the Prometheus `scenario` label and report dir name.
pub type ScenarioId = &'static str;

/// A load scenario: one full call through the SUT. Implementations are
/// `Send + Sync` and stateless (all per-call state is in the args).
#[async_trait]
pub trait LoadScenario: Send + Sync {
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
    /// expected failure; the driver tears the call down regardless.
    async fn run(
        &self,
        env: &CallEnv<'_>,
        scope: &CallScope,
        ctx: &CallCtx,
    ) -> Result<(), StepError>;
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
    call.try_expect(180).await?;
    uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
    call.try_expect(200).await?;
    ctx.checkpoint("time_to_200");

    let dialog = call.ack().await;
    scope.set_confirmed(dialog.clone());
    env.bob.try_receive("ACK").await?;
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
    Ok(())
}

/// Wraps any scenario as an EMERGENCY call: identical flow, but it stamps
/// `Resource-Priority: esnet.0` (so the SUT force-admits it under overload) and
/// reports under a distinct id, so the report cleanly splits the force-admitted
/// emergency traffic from the sheddable non-emergency traffic.
pub struct AsEmergency {
    id: ScenarioId,
    inner: Arc<dyn LoadScenario>,
}

impl AsEmergency {
    pub fn new(id: ScenarioId, inner: Arc<dyn LoadScenario>) -> Arc<dyn LoadScenario> {
        Arc::new(Self { id, inner })
    }
}

#[async_trait]
impl LoadScenario for AsEmergency {
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

/// All v1 scenarios with default weights (basic-heavy, like real traffic).
pub fn default_scenarios() -> Vec<(Arc<dyn LoadScenario>, f64)> {
    vec![
        (Arc::new(basic_call::BasicCall), 4.0),
        (Arc::new(reinvite::Reinvite), 2.0),
        (Arc::new(refer::Refer), 1.0),
        (Arc::new(options_hold::OptionsHold), 1.0),
    ]
}

/// Resolve a scenario by id (for CLI `--scenario name=weight`). The `*_em`
/// variants are emergency (Resource-Priority `esnet.0`) calls of the same flow.
pub fn by_id(id: &str) -> Option<Arc<dyn LoadScenario>> {
    match id {
        "basic_call" => Some(Arc::new(basic_call::BasicCall)),
        "reinvite" => Some(Arc::new(reinvite::Reinvite)),
        "refer" => Some(Arc::new(refer::Refer)),
        "options_hold" => Some(Arc::new(options_hold::OptionsHold)),
        "basic_call_em" => Some(AsEmergency::new("basic_call_em", Arc::new(basic_call::BasicCall))),
        "reinvite_em" => Some(AsEmergency::new("reinvite_em", Arc::new(reinvite::Reinvite))),
        "invite_reject" => Some(Arc::new(failures::InviteReject)),
        "abandon_ringing" => Some(Arc::new(failures::AbandonRinging)),
        "refer_charlie_reject" => Some(Arc::new(failures::ReferCharlieReject)),
        _ => None,
    }
}

/// The voluntarily-failing scenarios (one per post-call-cleanup teardown path),
/// for a no-leak cleanup-coverage test without an endurance run.
pub fn failure_scenarios() -> Vec<(Arc<dyn LoadScenario>, f64)> {
    vec![
        (Arc::new(failures::InviteReject), 1.0),
        (Arc::new(failures::AbandonRinging), 1.0),
        (Arc::new(failures::ReferCharlieReject), 1.0),
    ]
}
