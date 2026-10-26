//! A party meets the refusals RFC 3261 gives an in-dialog request: a 500
//! stating Retry-After to her re-INVITE is retried after that delay (§14.1),
//! and a 481 to her request ends her dialog (§12.2.1.2).

use std::time::Duration;

use super::testkit::caller_spec;
use crate::actor::*;
use crate::{Harness, ANSWER_SDP};

/// Alice calls bob, then sends `step` once established, then hangs up once
/// `done` holds.
fn plan(
    alice: &crate::Agent,
    bob: &crate::Agent,
    step: GoalStep,
    done: fn(&StateInner) -> bool,
) -> CallPlan {
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::AllConfirmed(&["alice"]), step),
        Goal::new(Barrier::pred("done", done), GoalStep::Bye),
    ];
    CallPlan {
        actors: vec![caller_spec("alice", alice, ("bob", bob.clone()), goals)],
        plan: vec![phase("established", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_refused_500_with_retry_after_is_retried_after_it() {
    let h = Harness::new("actor-reinvite-500-retry-after").describe(
        "bob refuses alice's re-INVITE 500 with Retry-After: 2; she sends it again 2 s \
         later, bob answers, and the call ends",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            let mut first = bob.receive("INVITE").await;
            first.respond(500, "Server Internal Error").with_header("Retry-After", "2").await;
            let refused_at = tokio::time::Instant::now();
            let mut again = bob.receive("INVITE").await;
            assert!(refused_at.elapsed() >= Duration::from_secs(2), "after the Retry-After");
            again.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            bob.receive("BYE").await.respond(200, "OK").await;
        })
    };
    let renegotiated = |s: &StateInner| s.leg("alice").reneg_count() >= 1;
    let call = plan(&alice, &bob, GoalStep::Reinvite, renegotiated);
    let verdict = run_call(call, Duration::from_secs(10)).await;
    assert!(verdict.is_ok(), "the call completes, got {verdict:?}");
    bob_task.await.expect("bob's side completes");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_request_refused_481_ends_the_dialog() {
    let h = Harness::new("actor-info-481").describe(
        "bob no longer knows the dialog and answers alice's INFO 481: her dialog ends, her \
         remaining goals with it",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            bob.receive("INFO").await.respond(481, "Call/Transaction Does Not Exist").await;
        })
    };
    let info = GoalStep::InDialog {
        method: sip_message::generators::InDialogMethod::Info,
        content_type: None,
        body: None,
        headers: vec![],
    };
    let informed = |s: &StateInner| {
        s.leg("alice").responses().iter().any(|r| r.cseq_method == "INFO" && r.status == 200)
    };
    let verdict = run_call(plan(&alice, &bob, info, informed), Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "her dialog ended and the call tore down, got {verdict:?}");
    bob_task.await.expect("bob's side completes");
    h.finish().await;
}
