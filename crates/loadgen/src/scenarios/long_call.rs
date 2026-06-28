//! Long recorded call: establish, send exactly ONE in-dialog OPTIONS keepalive
//! ping (the marker that lets the recorder capture a representative long flow),
//! then hold the dialog open for `long_hold` — answering the SUT's own in-dialog
//! keepalive OPTIONS on BOTH legs so the call is not torn down — then BYE.
//!
//! This is the small "long tail" of endurance traffic: a few percent of calls
//! that live for many minutes. Distinct from `options_hold` (which loops its own
//! OPTIONS): here alice pings once and then the call simply *survives*, which is
//! what exercises the worker's long-lived dialog state, its periodic keepalive
//! toward both peers (`B2BUA_KEEPALIVE_SEC`), and the recorder's long-flow path.

use async_trait::async_trait;
use scenario_harness::StepError;
use sip_message::generators::InDialogMethod;

use std::time::Duration;

use super::{establish, LoadScenario, ScenarioId};
use crate::ctx::{CallCtx, CallEnv};
use crate::scope::CallScope;

pub struct LongCall;

#[async_trait]
impl LoadScenario for LongCall {
    fn id(&self) -> ScenarioId {
        "long_call"
    }

    async fn run(
        &self,
        env: &CallEnv<'_>,
        scope: &CallScope,
        ctx: &CallCtx,
    ) -> Result<(), StepError> {
        let mut dialog = establish(env, scope, ctx).await?;

        // Exactly one in-dialog OPTIONS ping, right after connect — before any
        // SUT keepalive can fire (so it never races a relayed keepalive on bob's
        // socket). The relayed ping reaches bob, who 200s it.
        let mut opt = dialog.request(InDialogMethod::Options, None).await;
        scope.set_confirmed(dialog.clone());
        env.bob.try_receive("OPTIONS").await?.respond(200, "OK").await;
        opt.try_expect(200).await?;
        ctx.checkpoint("time_to_options_200");

        // Hold the call open for the full duration, answering the SUT's own
        // in-dialog keepalive OPTIONS (and any in-dialog request) on BOTH legs
        // CONCURRENTLY — `quiesce` blocks for the whole window, 200-ing every
        // inbound request. A long hold (minutes) outlives `B2BUA_KEEPALIVE_SEC`,
        // so a leg that went unanswered past `B2BUA_KEEPALIVE_TIMEOUT_SEC` would
        // be BYE'd out from under us; pumping both legs keeps the dialog alive.
        if !env.long_hold.is_zero() {
            tokio::join!(env.alice.quiesce(env.long_hold), env.bob.quiesce(env.long_hold));
        }

        // Tolerant teardown. The hold is deliberately DE-ALIGNED from the SUT's
        // keepalive interval (the endurance run holds ~6 min against a 5-min
        // keepalive, so the BYE lands well clear of a keepalive fire) — but ring
        // jitter can still drift one keepalive OPTIONS onto the teardown boundary.
        // So drain bob's leg with `quiesce`, which 200-OKs the relayed BYE AND any
        // straggler OPTIONS, instead of asserting the next request IS the BYE (which
        // misreported a benign keepalive as `wrong_method` when the hold length was a
        // multiple of the keepalive interval).
        let mut bye = dialog.bye().await;
        scope.set_confirmed(dialog.clone());
        env.bob.quiesce(Duration::from_millis(500)).await;
        bye.try_expect(200).await?;
        scope.mark_terminated();
        ctx.checkpoint("time_to_bye_200");
        Ok(())
    }
}
