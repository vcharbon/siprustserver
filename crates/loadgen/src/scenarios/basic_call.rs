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
        hangup(env, scope, &mut dialog, ctx).await
    }
}
