//! OPTIONS-keepalive long hold: establish, then keep the dialog alive with
//! periodic in-dialog OPTIONS for `options_hold`, then BYE (the SIPp
//! OPTIONS-hold replacement, mirrors `uac-long-options.xml`). Exercises
//! concurrency ceilings and the keepalive path.

use async_trait::async_trait;
// `tokio::time::Instant` (NOT `std::time::Instant`) so the hold loop is
// clock-portable: under a `#[tokio::test(start_paused)]` functional run the
// `tokio::time::sleep`s auto-advance virtual time, and only the tokio clock
// follows them — `std::time::Instant` would stay frozen and the loop would never
// reach `options_hold`, spinning until the SUT keepalive collided. Identical to
// `std::time::Instant` under the real clock the load fleet runs on (and matches
// the load driver, which already times on `tokio::time::Instant`).
use tokio::time::Instant;
use sip_message::generators::InDialogMethod;

use crate::realcall::{establish, hangup, CallCtx, CallEnv, CallScope, RealCallScenario, ScenarioId};
use crate::StepError;

pub struct OptionsHold;

#[async_trait]
impl RealCallScenario for OptionsHold {
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
