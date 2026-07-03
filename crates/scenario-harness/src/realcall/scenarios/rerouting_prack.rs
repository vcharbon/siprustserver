//! Rerouting + a RELIABLE provisional on the winning leg — the **load body** of
//! the dual-body `rerouting_prack` shape (the functional body lives in
//! `e2e-core::shapes::rerouting_prack`):
//!
//! ```text
//!   INVITE(offer, Supported:100rel, candidates [bob, bob2])
//!     → bob 486 (the SUT fails over to the next candidate)
//!     → bob2 183(Require:100rel, RSeq, answer) → PRACK → 200(PRACK)
//!     → 200(INVITE) → ACK → BYE
//! ```
//!
//! The ordered candidate list rides the egress seam
//! ([`CallEnv::outgoing_invite`] with `["bob", "bob2"]`): a pinned layout
//! ([`EgressPolicy::ApiCallPin`](crate::egress::EgressPolicy)) realizes it as an
//! `X-Api-Call` `routes` failover plan the SUT walks on the b-leg rejection
//! (each route's `new_ruri` names its callee, which is also how the driver's
//! R-URI-user leg picker demuxes the two receivers sharing one socket); a
//! transparent layout's own engine owns the same failover. Either way bob's
//! 486 drives the reroute to bob2, whose reliable 183/PRACK dance rides the
//! shared [`complete_100rel`] choreography.

use async_trait::async_trait;

use crate::realcall::{
    admitted_uas, complete_100rel, hangup_on, CallCtx, CallEnv, CallScope, RealCallScenario,
    ScenarioId,
};
use crate::{StepError, OFFER_SDP};

pub struct ReroutingPrack;

#[async_trait]
impl RealCallScenario for ReroutingPrack {
    fn id(&self) -> ScenarioId {
        "rerouting_prack"
    }

    async fn run(
        &self,
        env: &CallEnv<'_>,
        scope: &CallScope,
        ctx: &CallCtx,
    ) -> Result<(), StepError> {
        let bob2 = env.bob2.ok_or_else(|| StepError::UnexpectedKind {
            who: "rerouting_prack".to_string(),
            detail: "bound without a bob2 leg".to_string(),
        })?;

        // A UAC that intends to PRACK advertises 100rel support on the INVITE
        // (RFC 3262 §3). The layout realizes the [bob, bob2] candidate list on
        // its wire (see the module docs).
        let inv = env
            .alice
            .invite(env.bob)
            .with_sdp(OFFER_SDP)
            .with_header("Supported", "100rel");
        let mut call = env.outgoing_invite(&["bob", "bob2"], inv).send().await;
        scope.set_early(call.cancel_handle());

        // The primary callee gets the first b-leg (racing the overload-shed
        // final, like every establishment) and REJECTS it, triggering the SUT's
        // failover to the next candidate.
        let mut uas1 = admitted_uas(env, scope, &mut call, 183).await?;
        uas1.respond(486, "Busy Here").await;
        // Drain the SUT's ACK for the 486 (RFC 3261 §17.1.1.3 — the failed
        // b-leg INVITE client txn ACKs its non-2xx) so bob's leg closes clean.
        env.bob.try_receive("ACK").await?;

        // The SUT fails over: bob2 gets the rerouted b-leg…
        let uas2 = bob2.try_receive("INVITE").await?;
        ctx.phase("rerouted");

        // …and answers RELIABLY; the shared 183/PRACK/200/ACK dance completes
        // the call on the WINNING leg.
        let mut dialog = complete_100rel(env, scope, ctx, call, uas2, bob2).await?;

        // Realistic talk time before teardown (free under a paused clock).
        if !env.talk_time.is_zero() {
            tokio::time::sleep(env.talk_time).await;
        }

        hangup_on(env, scope, &mut dialog, ctx, bob2).await
    }
}
