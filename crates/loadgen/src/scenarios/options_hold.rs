//! OPTIONS-keepalive long hold: establish, then keep the dialog alive with
//! periodic in-dialog OPTIONS for `options_hold`, then BYE (the SIPp
//! OPTIONS-hold replacement, mirrors `uac-long-options.xml`). Exercises
//! concurrency ceilings and the keepalive path.

use std::time::Instant;

use async_trait::async_trait;
use scenario_harness::StepError;
use sip_message::generators::InDialogMethod;

use super::{establish, hangup, LoadScenario, ScenarioId};
use crate::ctx::{CallCtx, CallEnv};
use crate::scope::CallScope;

pub struct OptionsHold;

#[async_trait]
impl LoadScenario for OptionsHold {
    fn id(&self) -> ScenarioId {
        "options_hold"
    }

    async fn run(
        &self,
        env: &CallEnv<'_>,
        scope: &CallScope,
        ctx: &CallCtx,
    ) -> Result<(), StepError> {
        let mut dialog = establish(env, scope, ctx).await?;

        let start = Instant::now();
        let mut first = true;
        while start.elapsed() < env.options_hold {
            tokio::time::sleep(env.options_cadence).await;
            let mut opt = dialog.request(InDialogMethod::Options, None).await;
            scope.set_confirmed(dialog.clone());
            // The B2BUA relays the in-dialog OPTIONS to bob, who answers 200.
            env.bob.try_receive("OPTIONS").await?.respond(200, "OK").await;
            opt.try_expect(200).await?;
            if first {
                ctx.checkpoint("time_to_options_200");
                first = false;
            }
        }

        hangup(env, scope, &mut dialog, ctx).await
    }
}
