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
        let inv = env.alice.invite(env.bob).with_sdp(OFFER_SDP);
        let mut call = env.outgoing_invite(&["bob"], inv).send().await;
        scope.set_early(call.cancel_handle());

        let mut bob_uas = env.bob.try_receive("INVITE").await?;
        bob_uas.respond(180, "Ringing").await;
        call.try_expect(180).await?;
        // Realistic ring before answer (consistent with the other scenarios' 5 s).
        if !env.ring_delay.is_zero() {
            tokio::time::sleep(env.ring_delay).await;
        }
        bob_uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        call.try_expect(200).await?;
        ctx.checkpoint("time_to_200");
        let mut alice_dialog = call.ack().await;
        scope.set_confirmed(alice_dialog.clone());
        env.bob.try_receive("ACK").await?;
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
        let mut refer = refer.send().await;
        // The 202 and the first NOTIFY race on bob's socket; tolerate a NOTIFY
        // arriving first (UDP reordering — and the fake fabric's equal-transit
        // race). 200-OK it and keep waiting for the 202.
        refer.try_expect_tolerating(202, &["NOTIFY"]).await?;
        ctx.checkpoint("time_to_202");
        ctx.phase("referred");

        // Charlie answers the transfer INVITE (held SDP).
        let mut charlie_uas = charlie.try_receive("INVITE").await?;
        charlie_uas.respond(180, "Ringing").await;
        charlie_uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        ctx.checkpoint("time_to_charlie_200");
        ctx.phase("transferred");
        let _charlie_dialog = charlie_uas.dialog();

        // Let the B2BUA MERGE the transfer media before tearing down. The merge
        // is ORDERED: the B2BUA first re-INVITEs CHARLIE (the c-realign, carrying
        // alice's SDP); only once charlie answers 200 does it re-INVITE ALICE (the
        // a-realign). So drain charlie/bob FIRST to settle the c-realign + the
        // remaining NOTIFYs, THEN drain ALICE last and longer so her a-realign
        // re-INVITE arrives and is 200'd + ACKed BEFORE we BYE.
        //
        // Why the ordering matters (and the old "drain alice first" was wrong):
        // alice's a-realign only fires AFTER the c-realign, i.e. well after an
        // early alice drain has closed. If alice BYEs before the a-realign lands,
        // that in-dialog 2xx is left un-ACKed on the closing dialog — a benign
        // race, but it legitimately trips the §13.3.1.4 "answered 2xx never
        // ACKed/BYE'd" audit. Completing the a-realign in order keeps the audit
        // clean WITHOUT weakening it.
        let settle = std::time::Duration::from_millis(150);
        env.bob.quiesce(settle).await;
        charlie.quiesce(settle).await;
        // Alice last: a generous window that outlasts the c-realign → a-realign
        // chain so her realign 200 (and the B2BUA's ACK of it) complete pre-BYE.
        env.alice.quiesce(std::time::Duration::from_millis(600)).await;

        // A↔B↔C merged; tear down via A BYE → the B2BUA BYEs every leg.
        let mut alice_bye = alice_dialog.bye().await;
        scope.set_confirmed(alice_dialog.clone());
        // The relayed b-leg BYEs to B and C race the 200; absorb them, then read
        // alice's own 200 (tolerating a stray late NOTIFY / re-INVITE).
        env.bob.quiesce(settle).await;
        charlie.quiesce(settle).await;
        alice_bye.try_expect_tolerating(200, &["BYE", "NOTIFY", "INVITE"]).await?;
        scope.mark_terminated();
        Ok(())
    }
}
