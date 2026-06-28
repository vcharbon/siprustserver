//! Basic call: INVITE / 180 / 200 / ACK, short hold, BYE. The bread-and-butter
//! load flow (mirrors `uac-endurance-short.xml`).

use async_trait::async_trait;
use scenario_harness::StepError;

use super::{establish, hangup, LoadScenario, ScenarioId};
use crate::ctx::{CallCtx, CallEnv};
use crate::scope::CallScope;

pub struct BasicCall;

#[async_trait]
impl LoadScenario for BasicCall {
    fn id(&self) -> ScenarioId {
        "basic_call"
    }

    async fn run(
        &self,
        env: &CallEnv<'_>,
        scope: &CallScope,
        ctx: &CallCtx,
    ) -> Result<(), StepError> {
        let mut dialog = establish(env, scope, ctx).await?;
        // Realistic post-connect talk time before teardown. The hold is short
        // (well under the SUT's in-dialog keepalive interval), so no leg pumping
        // is needed; the long_call scenario covers the keepalive-answering path.
        if !env.talk_time.is_zero() {
            tokio::time::sleep(env.talk_time).await;
        }
        hangup(env, scope, &mut dialog, ctx).await
    }
}
