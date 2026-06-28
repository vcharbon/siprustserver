//! Voluntarily-FAILING scenarios — calls that deliberately do not reach a clean
//! BYE, one per distinct teardown path, so the post-call cleanup matrix is
//! covered without an endurance run. Each ends in a different
//! [`CallScope`](crate::scope::CallScope) state so the driver's teardown
//! exercises every reclamation branch (CANCEL an early dialog, BYE a confirmed
//! one, no-op a final-rejected one) and the SUT must still fully reap:
//!
//! | scenario              | fails at            | scope at exit | teardown |
//! |-----------------------|---------------------|---------------|----------|
//! | [`InviteReject`]      | callee 486s INVITE  | Terminated    | none (final) |
//! | [`AbandonRinging`]    | caller quits on 180 | Early         | CANCEL   |
//! | [`ReferCharlieReject`]| transfer target 603 | Confirmed     | BYE A↔B  |
//!
//! ([`FailMidCall`](crate::scenarios) — establish then drop — covers the plain
//! Confirmed→BYE path in the smoke suite.)

use async_trait::async_trait;
use scenario_harness::{StepError, ANSWER_SDP, OFFER_SDP};
use sip_message::generators::InDialogMethod;

use super::{LoadScenario, ScenarioId};
use crate::ctx::{CallCtx, CallEnv};
use crate::scope::CallScope;

/// The callee rejects the INVITE with a `486 Busy Here`. The final response
/// completes the INVITE transaction (the stack auto-ACKs the non-2xx), so there
/// is nothing to CANCEL/BYE — the scope is marked terminated and teardown is a
/// no-op. Reported as `status_486` (the NOK final-reject case).
pub struct InviteReject;

#[async_trait]
impl LoadScenario for InviteReject {
    fn id(&self) -> ScenarioId {
        "invite_reject"
    }

    async fn run(&self, env: &CallEnv<'_>, scope: &CallScope, _ctx: &CallCtx) -> Result<(), StepError> {
        let inv = env.alice.invite(env.bob).with_sdp(OFFER_SDP).through(env.via);
        let mut call = env.prepare_invite(inv).send().await;
        scope.set_early(call.cancel_handle());

        let mut uas = env.bob.try_receive("INVITE").await?;
        uas.respond(486, "Busy Here").await;

        // Expect the (never-arriving) 200; the relayed 486 surfaces as
        // `WrongStatus { got: 486 }`. A real FINAL (≥ 200) ended the transaction,
        // so mark the scope terminated — CANCELing an already-rejected INVITE just
        // churns the SUT (mirrors `establish`'s shed handling). A non-180
        // provisional would NOT qualify, leaving the scope Early to CANCEL.
        let r = call.try_expect(200).await;
        if matches!(&r, Err(StepError::WrongStatus { got, .. }) if *got >= 200) {
            scope.mark_terminated();
        }
        r.map(|_| ())
    }
}

/// The caller abandons after ringing: it sends the INVITE, sees `180`, then
/// gives up before answer. The scope is left Early, so the driver's teardown
/// CANCELs the pending INVITE (RFC 3261 §9.1) — the path a real caller hang-up
/// before pickup takes. Reported as `timeout` (the NOK abandoned-early case).
pub struct AbandonRinging;

#[async_trait]
impl LoadScenario for AbandonRinging {
    fn id(&self) -> ScenarioId {
        "abandon_ringing"
    }

    async fn run(&self, env: &CallEnv<'_>, scope: &CallScope, ctx: &CallCtx) -> Result<(), StepError> {
        let inv = env.alice.invite(env.bob).with_sdp(OFFER_SDP).through(env.via);
        let mut call = env.prepare_invite(inv).send().await;
        scope.set_early(call.cancel_handle()); // safety net until the handshake completes

        let mut bob_uas = env.bob.try_receive("INVITE").await?;
        bob_uas.respond(180, "Ringing").await;
        call.try_expect(180).await?;
        ctx.checkpoint("time_to_180");

        // Caller hangs up before answer. Drive the FULL CANCEL handshake so the
        // SUT fully reaps BOTH legs: alice CANCELs; the B2BUA relays the CANCEL to
        // bob, who answers 200 (CANCEL) + 487 (its INVITE). The B2BUA auto-ACKs
        // the non-2xx on both legs and terminates the call. (A bare fire-and-
        // forget CANCEL leaves the b-leg INVITE unfinalized → the call lingers to
        // Timer C; completing bob's 487 is what makes the reap immediate.)
        let _cxl = call.cancel().await;
        let mut bob_cancel = env.bob.try_receive("CANCEL").await?;
        bob_cancel.respond(200, "OK").await;
        bob_uas.respond(487, "Request Terminated").await;
        // The B2BUA removes the call once the b-leg terminates (bob's 487, which
        // it auto-ACKs); alice's own `200 (CANCEL)` + `487 (INVITE)` settle at the
        // transaction layer and need not be read here (and reading them races the
        // two interleaved finals on one inbox). So we do NOT read alice's side.
        scope.mark_terminated(); // both legs torn down — teardown is a no-op

        // The abandoned-before-answer call is the NOK outcome.
        Err(StepError::Timeout { who: "alice-abandoned-after-ringing".to_string() })
    }
}

/// A blind transfer whose target DECLINES: A↔B establish, B REFERs to C, but C
/// rejects the transfer INVITE with `603 Decline`. The transfer fails (the
/// B2BUA NOTIFYs the failure and keeps A↔B up); the scenario surfaces the failed
/// transfer and leaves the scope Confirmed, so the driver's teardown BYEs the
/// still-live A↔B — exercising cleanup of a call whose transfer leg was born and
/// rejected. Reported as `unexpected` (the NOK declined-transfer case).
pub struct ReferCharlieReject;

#[async_trait]
impl LoadScenario for ReferCharlieReject {
    fn id(&self) -> ScenarioId {
        "refer_charlie_reject"
    }

    fn needs_charlie(&self) -> bool {
        true
    }

    async fn run(&self, env: &CallEnv<'_>, scope: &CallScope, ctx: &CallCtx) -> Result<(), StepError> {
        let charlie = env.charlie.ok_or_else(|| StepError::UnexpectedKind {
            who: "refer_charlie_reject".to_string(),
            detail: "bound without a charlie leg".to_string(),
        })?;

        // A↔B established.
        let inv = env.alice.invite(env.bob).with_sdp(OFFER_SDP).through(env.via);
        let mut call = env.prepare_invite(inv).send().await;
        scope.set_early(call.cancel_handle());
        let mut bob_uas = env.bob.try_receive("INVITE").await?;
        bob_uas.respond(180, "Ringing").await;
        call.try_expect(180).await?;
        bob_uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        call.try_expect(200).await?;
        ctx.checkpoint("time_to_200");
        let alice_dialog = call.ack().await;
        scope.set_confirmed(alice_dialog.clone());
        env.bob.try_receive("ACK").await?;
        let mut bob_dialog = bob_uas.dialog();

        // REFER → 202 (tolerate a NOTIFY racing the 202).
        let refer_to = env.refer_to().ok_or_else(|| StepError::UnexpectedKind {
            who: "refer_charlie_reject".to_string(),
            detail: "no charlie for Refer-To".to_string(),
        })?;
        let mut refer = bob_dialog.send_request(InDialogMethod::Refer).with_header("Refer-To", &refer_to);
        if let Some(api) = env.refer_api_call() {
            refer = refer.with_header("X-Api-Call", &api);
        }
        let mut refer = refer.send().await;
        refer.try_expect_tolerating(202, &["NOTIFY"]).await?;

        // The transfer target DECLINES the INVITE.
        let mut charlie_uas = charlie.try_receive("INVITE").await?;
        charlie_uas.respond(603, "Decline").await;

        // Absorb the failure NOTIFY(s) + any realign on all legs; A↔B stays up.
        let settle = std::time::Duration::from_millis(120);
        env.alice.quiesce(settle).await;
        env.bob.quiesce(settle).await;
        charlie.quiesce(settle).await;

        // The transfer FAILED — surface it as the NOK outcome and leave the scope
        // Confirmed so the driver's teardown BYEs the still-live A↔B (the case
        // under test: cleanup of a call whose transfer leg was rejected).
        Err(StepError::UnexpectedKind {
            who: "refer_charlie_reject".to_string(),
            detail: "transfer declined by charlie (603)".to_string(),
        })
    }
}
