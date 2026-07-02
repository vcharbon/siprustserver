//! PRACK + UPDATE: RFC 3262 reliable-provisional establishment followed by an
//! RFC 3311 in-dialog UPDATE renegotiation, then BYE.
//!
//! ```text
//!   INVITE(offer, Supported:100rel) → 183(Require:100rel, RSeq, answer)
//!     → PRACK(RAck) → 200(PRACK) → 200(INVITE) → ACK
//!     → UPDATE(re-offer) → 200(UPDATE, re-answer) → BYE → 200
//! ```
//!
//! The SUT relays the reliable 183 (with `Require: 100rel`/`RSeq` intact), the
//! PRACK, its 200 and the UPDATE end-to-end (`relay-prack` / `relay-update` /
//! `relay-non-invite-200` — the same paths `b2bua-harness/tests/prack.rs` and
//! `prack_update_forking.rs` assert strictly); the RSeq/RAck bookkeeping is
//! UA-to-UA. Establishment rides the shared [`establish_100rel`] choreography;
//! the UPDATE rides the generic fallible any-method surface.

use async_trait::async_trait;
use sip_message::generators::InDialogMethod;

use crate::realcall::{
    establish_100rel, hangup, CallCtx, CallEnv, CallScope, RealCallScenario, ScenarioId,
};
use crate::StepError;

/// SDP alice re-offers in the confirmed-dialog UPDATE (a media tweak).
const UPDATE_OFFER: &str =
    "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
/// SDP bob answers the UPDATE with.
const UPDATE_ANSWER: &str =
    "v=0\r\no=bob 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

pub struct PrackUpdate;

#[async_trait]
impl RealCallScenario for PrackUpdate {
    fn id(&self) -> ScenarioId {
        "prack_update"
    }

    async fn run(
        &self,
        env: &CallEnv<'_>,
        scope: &CallScope,
        ctx: &CallCtx,
    ) -> Result<(), StepError> {
        let mut dialog = establish_100rel(env, scope, ctx).await?;

        // Realistic spacing: dwell the confirmed dialog before renegotiating.
        if !env.reinvite_gap.is_zero() {
            tokio::time::sleep(env.reinvite_gap).await;
        }

        // In-dialog UPDATE (RFC 3311): alice re-offers, bob answers in the 200.
        // Unlike a re-INVITE there is no ACK — the 200 completes the exchange.
        let mut update = dialog
            .send_request(InDialogMethod::Update)
            .with_sdp(UPDATE_OFFER)
            .try_send()
            .await?;
        scope.set_confirmed(dialog.clone()); // refresh CSeq so a teardown BYE stays valid
        let mut update_uas = env.bob.try_receive("UPDATE").await?;
        update_uas.respond(200, "OK").with_sdp(UPDATE_ANSWER).try_send().await?;
        update.try_expect(200).await?;
        ctx.checkpoint("time_to_update_200");
        ctx.phase("updated");

        // Post-renegotiation dwell before teardown.
        if !env.reinvite_gap.is_zero() {
            tokio::time::sleep(env.reinvite_gap).await;
        }

        hangup(env, scope, &mut dialog, ctx).await
    }
}
