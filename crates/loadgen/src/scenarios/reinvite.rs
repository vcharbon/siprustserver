//! Re-INVITE: establish, then an in-dialog (delayed-offer) re-INVITE
//! renegotiation, then BYE (mirrors `uac-reinvite.xml`).

use async_trait::async_trait;
use scenario_harness::StepError;
use sip_message::generators::InDialogMethod;

use super::{establish, hangup, LoadScenario, ScenarioId};
use crate::ctx::{CallCtx, CallEnv};
use crate::scope::CallScope;

/// SDP the callee answers the delayed re-INVITE offer with.
const REOFFER: &str =
    "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";
/// SDP the caller acks the re-INVITE with.
const REANSWER: &str =
    "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

pub struct Reinvite;

#[async_trait]
impl LoadScenario for Reinvite {
    fn id(&self) -> ScenarioId {
        "reinvite"
    }

    async fn run(
        &self,
        env: &CallEnv<'_>,
        scope: &CallScope,
        ctx: &CallCtx,
    ) -> Result<(), StepError> {
        let mut dialog = establish(env, scope, ctx).await?;

        // Delayed-offer re-INVITE: alice sends bodyless, bob answers with SDP.
        let mut reinv = dialog.request(InDialogMethod::Invite, None).await;
        scope.set_confirmed(dialog.clone());
        let mut bob_uas = env.bob.try_receive("INVITE").await?;
        bob_uas.respond(200, "OK").with_sdp(REOFFER).await;
        reinv.try_expect(200).await?;
        ctx.checkpoint("time_to_reinvite_200");

        dialog.ack(Some(REANSWER)).await;
        env.bob.try_receive("ACK").await?;

        hangup(env, scope, &mut dialog, ctx).await
    }
}
