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

    let mut uas = env.bob.try_receive("INVITE").await?;
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

/// All v1 scenarios with default weights (basic-heavy, like real traffic).
pub fn default_scenarios() -> Vec<(Arc<dyn LoadScenario>, f64)> {
    vec![
        (Arc::new(basic_call::BasicCall), 4.0),
        (Arc::new(reinvite::Reinvite), 2.0),
        (Arc::new(refer::Refer), 1.0),
        (Arc::new(options_hold::OptionsHold), 1.0),
    ]
}

/// Resolve a scenario by id (for CLI `--scenario name=weight`).
pub fn by_id(id: &str) -> Option<Arc<dyn LoadScenario>> {
    match id {
        "basic_call" => Some(Arc::new(basic_call::BasicCall)),
        "reinvite" => Some(Arc::new(reinvite::Reinvite)),
        "refer" => Some(Arc::new(refer::Refer)),
        "options_hold" => Some(Arc::new(options_hold::OptionsHold)),
        _ => None,
    }
}
