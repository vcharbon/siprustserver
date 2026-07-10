//! REFER blind transfer: establish A↔B, then B issues a REFER to C that the
//! cluster authorizes (`X-Api-Call` `refer_key`); the B2BUA builds the C leg and
//! drives INVITE/200/ACK with interleaved NOTIFY progress, then A BYEs (tearing
//! down B and C). The complex flow SIPp can't easily express — the motivation
//! for this tool. Port of `b2bua-harness/tests/refer_allow.rs::refer_allow_happy`,
//! made fallible and lenient (NOTIFY bodies are answered, not deep-asserted).

use async_trait::async_trait;
use sip_message::generators::InDialogMethod;

use crate::realcall::{CallCtx, CallEnv, CallScope, RealCallScenario, ScenarioId};
use crate::{StepError, ANSWER_SDP, OFFER_SDP};

pub struct Refer {
    /// The `X-Api-Call.refer_key` the SUT's REFER backend authorizes — per-run
    /// SUT auth data fed in at construction (the CLI's `--refer-key`), NOT
    /// topology (the transfer *target* resolves through the env's egress seam).
    pub refer_key: String,
}

impl Refer {
    pub fn new(refer_key: impl Into<String>) -> Self {
        Self { refer_key: refer_key.into() }
    }
}

#[async_trait]
impl RealCallScenario for Refer {
    fn id(&self) -> ScenarioId {
        "refer"
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
        let inv = env.alice.invite(env.bob).with_sdp(OFFER_SDP);
        let mut call = env.outgoing_invite(&["bob"], inv).send().await;
        scope.set_early(call.cancel_handle());

        let mut bob_uas = env.bob.try_receive("INVITE").await?;
        ctx.anchor(env.bob, "initialInvite", bob_uas.request());
        bob_uas.respond(180, "Ringing").await;
        let ring = call.try_expect(180).await?;
        ctx.anchor(env.alice, "firstProvisional", &ring);
        // Realistic ring before answer (consistent with the other scenarios' 5 s).
        if !env.ring_delay.is_zero() {
            tokio::time::sleep(env.ring_delay).await;
        }
        bob_uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        let answer = call.try_expect(200).await?;
        ctx.anchor(env.alice, "answer", &answer);
        ctx.checkpoint("time_to_200");
        let mut alice_dialog = call.ack().await;
        scope.set_confirmed(alice_dialog.clone());
        let ack = env.bob.try_receive("ACK").await?;
        ctx.anchor(env.bob, "ack", ack.request());
        let mut bob_dialog = bob_uas.dialog();

        // Talk a few seconds in the established A↔B call before Bob transfers (a
        // real attendant speaks first, and it keeps the REFER off the connect
        // boundary). Uses the re-INVITE spacing knob (the endurance run sets 5 s).
        if !env.reinvite_gap.is_zero() {
            tokio::time::sleep(env.reinvite_gap).await;
        }

        // REFER → 202. The transfer target resolves through the SAME egress seam
        // as any callee (`env.refer_to()` = charlie via `callee("charlie")`, with
        // the correlation token as the user-part); the X-Api-Call authorizes the
        // transfer under this run's `refer_key` (mirrors the e2e
        // `transfer-refer-media` shape's `ApiCall::refer`).
        let refer_to = env.refer_to().ok_or_else(|| StepError::UnexpectedKind {
            who: "refer".to_string(),
            detail: "no charlie for Refer-To".to_string(),
        })?;
        let mut refer = bob_dialog.send_request(InDialogMethod::Refer).with_header("Refer-To", &refer_to);
        if let Some(api) = env.refer_authorization(&self.refer_key) {
            refer = refer.with_header("X-Api-Call", &api);
        }
        // The REFER's only receiver is the SUT itself (it builds the C leg), so
        // it is anchored as a SENT message on bob's lane.
        let (mut refer, refer_req) = refer.try_send_with_request().await?;
        ctx.anchor_sent(env.bob, "refer", &refer_req);
        // The 202 and the first NOTIFY race on bob's socket; tolerate a NOTIFY
        // arriving first (UDP reordering — and the fake fabric's equal-transit
        // race). 200-OK it and keep waiting for the 202.
        refer.try_expect_tolerating(202, &["NOTIFY"]).await?;
        ctx.checkpoint("time_to_202");
        ctx.phase("referred");

        // Charlie answers the transfer INVITE (held SDP).
        let mut charlie_uas = charlie.try_receive("INVITE").await?;
        ctx.anchor(charlie, "initialInvite", charlie_uas.request());
        charlie_uas.respond(180, "Ringing").await;
        charlie_uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        ctx.checkpoint("time_to_charlie_200");
        ctx.phase("transferred");
        let _charlie_dialog = charlie_uas.dialog();

        // The B2BUA MERGES the transfer media before teardown, in ORDER: it
        // ACKs charlie, re-INVITEs CHARLIE (the c-realign, carrying alice's
        // SDP), and only once charlie answers does it re-INVITE ALICE (the
        // a-realign). Each step used to be a blind `quiesce` drain — which
        // could not assert the realigns actually happened AND answered the
        // offer-carrying realign re-INVITE with a bodyless 200 (an RFC 3264
        // §5 / §13.3.1.1 violation on our own UA). The blocking tolerant
        // receive (newkahneed-033 ask C) makes each step assertable and
        // answers the realign with a real SDP answer; a missing realign is
        // now a Timeout, not silent success.
        //
        // Why alice's realign must complete BEFORE her BYE: if alice BYEs
        // while the a-realign 2xx is in flight, that in-dialog 2xx is left
        // un-ACKed on the closing dialog — a benign race, but it legitimately
        // trips the §13.3.1.4 "answered 2xx never ACKed/BYE'd" audit.
        let (mut c_realign, _) = charlie.try_receive_tolerating_blocking("INVITE", &[]).await?;
        c_realign.respond(200, "OK").with_sdp(ANSWER_SDP).try_send().await?;
        let (_c_realign_ack, _) = charlie.try_receive_tolerating_blocking("ACK", &[]).await?;
        let (mut a_realign, _) = env.alice.try_receive_tolerating_blocking("INVITE", &[]).await?;
        a_realign.respond(200, "OK").with_sdp(ANSWER_SDP).try_send().await?;
        let (_a_realign_ack, _) = env.alice.try_receive_tolerating_blocking("ACK", &[]).await?;
        // Bob's completion NOTIFY(s) have no terminating sentinel pre-BYE (the
        // Trying NOTIFY may already have been absorbed by the 202 wait), so a
        // short drain remains the right tool for HIS socket only.
        env.bob.quiesce(std::time::Duration::from_millis(150)).await;

        // A↔B↔C merged; tear down via A BYE → the B2BUA BYEs every leg.
        let mut alice_bye = alice_dialog.bye().await;
        scope.set_confirmed(alice_dialog.clone());
        // The relayed b-leg BYEs to B and C race alice's 200. ASSERT both
        // arrive (a lost teardown is a failure, not silence), answering any
        // interleaved NOTIFY, then read alice's own 200.
        let (mut bob_bye, _) = env.bob.try_receive_tolerating_blocking("BYE", &["NOTIFY"]).await?;
        bob_bye.respond(200, "OK").try_send().await?;
        let (mut charlie_bye, _) =
            charlie.try_receive_tolerating_blocking("BYE", &["NOTIFY"]).await?;
        charlie_bye.respond(200, "OK").try_send().await?;
        alice_bye.try_expect_tolerating(200, &["BYE", "NOTIFY", "INVITE"]).await?;
        scope.mark_terminated();
        Ok(())
    }
}
