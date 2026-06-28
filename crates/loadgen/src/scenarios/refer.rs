//! REFER blind transfer: establish A↔B, then B issues a REFER to C that the
//! cluster authorizes (`X-Api-Call` `refer_key`); the B2BUA builds the C leg and
//! drives INVITE/200/ACK with interleaved NOTIFY progress, then A BYEs (tearing
//! down B and C). The complex flow SIPp can't easily express — the motivation
//! for this tool. Port of `b2bua-harness/tests/refer_allow.rs::refer_allow_happy`,
//! made fallible and lenient (NOTIFY bodies are answered, not deep-asserted).

use async_trait::async_trait;
use scenario_harness::{StepError, ANSWER_SDP, OFFER_SDP};
use sip_message::generators::InDialogMethod;

use super::{LoadScenario, ScenarioId};
use crate::ctx::{CallCtx, CallEnv};
use crate::scope::CallScope;

pub struct Refer;

#[async_trait]
impl LoadScenario for Refer {
    fn id(&self) -> ScenarioId {
        "refer"
    }

    fn needs_charlie(&self) -> bool {
        true
    }

    async fn run(
        &self,
        env: &CallEnv<'_>,
        scope: &CallScope,
        ctx: &CallCtx,
    ) -> Result<(), StepError> {
        let charlie = env.charlie.ok_or_else(|| StepError::UnexpectedKind {
            who: "refer".to_string(),
            detail: "REFER scenario bound without a charlie leg".to_string(),
        })?;

        // A↔B established (capture bob's UAS dialog to originate the REFER).
        let inv = env.alice.invite(env.bob).with_sdp(OFFER_SDP).through(env.via);
        let mut call = env.prepare_invite(inv).send().await;
        scope.set_early(call.cancel_handle());

        let mut bob_uas = env.bob.try_receive("INVITE").await?;
        bob_uas.respond(180, "Ringing").await;
        call.try_expect(180).await?;
        bob_uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        call.try_expect(200).await?;
        ctx.checkpoint("time_to_200");
        let mut alice_dialog = call.ack().await;
        scope.set_confirmed(alice_dialog.clone());
        env.bob.try_receive("ACK").await?;
        let mut bob_dialog = bob_uas.dialog();

        // REFER → 202. Refer-To carries charlie's correlation token in the
        // user-part; the optional X-Api-Call pins the transfer to the static
        // `refer` endpoint (our-b2bua adapter).
        let refer_to = env.refer_to().ok_or_else(|| StepError::UnexpectedKind {
            who: "refer".to_string(),
            detail: "no charlie for Refer-To".to_string(),
        })?;
        let mut refer = bob_dialog.send_request(InDialogMethod::Refer).with_header("Refer-To", &refer_to);
        if let Some(api) = env.refer_api_call() {
            refer = refer.with_header("X-Api-Call", &api);
        }
        let mut refer = refer.send().await;
        // The 202 and the first NOTIFY race on bob's socket; tolerate a NOTIFY
        // arriving first (UDP reordering — and the fake fabric's equal-transit
        // race). 200-OK it and keep waiting for the 202.
        refer.try_expect_tolerating(202, &["NOTIFY"]).await?;
        ctx.checkpoint("time_to_202");

        // Charlie answers the transfer INVITE (held SDP).
        let mut charlie_uas = charlie.try_receive("INVITE").await?;
        charlie_uas.respond(180, "Ringing").await;
        charlie_uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        ctx.checkpoint("time_to_charlie_200");
        let _charlie_dialog = charlie_uas.dialog();

        // The B2BUA now ACKs charlie and emits the remaining NOTIFYs (180,
        // terminated) to bob and a c-realign re-INVITE to charlie — all racing on
        // two sockets in any order. Absorb them with a short settle (200-OK every
        // straggler) rather than asserting an exact sequence: a load tool must be
        // robust to interleaving, not a strict conformance oracle (that is
        // `refer_allow.rs`'s job).
        // Absorb every post-transfer straggler on ALL legs (200-OK each): the
        // remaining NOTIFYs + c-realign on B/C, AND the **a-leg realign re-INVITE**
        // the B2BUA sends to alice after the transfer (which would otherwise queue
        // and break alice's BYE response read).
        let settle = std::time::Duration::from_millis(120);
        env.alice.quiesce(settle).await;
        env.bob.quiesce(settle).await;
        charlie.quiesce(settle).await;

        // A↔B still bridged; tear down via A BYE → the B2BUA BYEs every leg.
        let mut alice_bye = alice_dialog.bye().await;
        scope.set_confirmed(alice_dialog.clone());
        // The relayed b-leg BYEs to B and C race the 200; absorb them, then read
        // alice's own 200 (tolerating a late realign INVITE / NOTIFY).
        env.bob.quiesce(settle).await;
        charlie.quiesce(settle).await;
        alice_bye.try_expect_tolerating(200, &["BYE", "NOTIFY", "INVITE"]).await?;
        scope.mark_terminated();
        Ok(())
    }
}
