//! A clean end by the peer while the call accepts one: the party the peer
//! BYEs, or whose establishing INVITE draws a non-2xx, drops her remaining
//! goals and the call ends cleanly; before the call accepts clean ends, the
//! same peer fails it.

use std::time::Duration;

use super::testkit::caller_spec;
use crate::actor::*;
use crate::realcall::CallCtx;
use crate::{Harness, ANSWER_SDP};

/// Alice calls bob and, once established, waits 2 s to send an INFO and
/// then hang up; bob answers, then BYEs her after 1 s.
fn call_bobbed(alice: &crate::Agent, bob: &crate::Agent) -> CallPlan {
    let established = |s: &StateInner| s.leg_at_least("alice", LegPhase::Confirmed);
    let info = GoalStep::InDialog {
        method: sip_message::generators::InDialogMethod::Info,
        content_type: None,
        body: None,
        headers: vec![],
    };
    let informed = |s: &StateInner| {
        s.leg("alice").responses().iter().any(|r| r.cseq_method == "INFO" && r.status == 200)
    };
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::pred("established", established), info).after(Duration::from_secs(2)),
        Goal::new(Barrier::pred("informed", informed), GoalStep::Bye),
    ];
    CallPlan {
        actors: vec![caller_spec("alice", alice, ("bob", bob.clone()), goals)],
        plan: vec![phase("informed", informed)],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

/// Bob, hand-scripted: answers alice's INVITE, then BYEs her after 1 s.
fn bob_hangs_up(bob: &crate::Agent) -> tokio::task::JoinHandle<()> {
    let bob = bob.clone();
    tokio::spawn(async move {
        let mut invite = bob.receive("INVITE").await;
        invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        let mut dialog = invite.dialog();
        bob.receive("ACK").await;
        tokio::time::sleep(Duration::from_secs(1)).await;
        dialog.bye().await.expect(200).await;
    })
}

/// The verdict of alice's call bob BYEs, checked by `check` before the
/// harness judges the wire.
async fn bobbed(name: &str, accept: bool, check: impl FnOnce(&ScenarioVerdict)) {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = bob_hangs_up(&bob);
    let ctx = CallCtx::new();
    if accept {
        ctx.accept_clean_ends();
    }
    let obs = ObservedState::new();
    let run = run_call_with(call_bobbed(&alice, &bob), obs.clone(), &ctx, STEP_TIMEOUT, None).await;
    bob_task.await.expect("bob's side completes");
    check(&verdict(Expect::answered(), run, &obs, "alice"));
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_party_the_peer_byes_ends_the_call_cleanly_once_clean_ends_are_accepted() {
    bobbed("actor-clean-end-bye", true, |verdict| {
        assert_eq!(verdict.branch(), Some(ScenarioVerdict::ENDED_CLEANLY), "{verdict:?}");
    })
    .await;
}

/// Unaccepted, alice goes on with her INFO into the dialog bob ended: the
/// call fails, and so does the wire.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "dialog rule dialog-ended")]
async fn a_party_the_peer_byes_fails_the_call_unless_clean_ends_are_accepted() {
    bobbed("actor-clean-end-bye-strict", false, |verdict| {
        assert!(!verdict.is_success(), "{verdict:?}");
    })
    .await;
}

async fn refused(name: &str, accept: bool) -> ScenarioVerdict {
    let h = Harness::new(name);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(503, "Service Unavailable").await;
        })
    };
    let ctx = CallCtx::new();
    if accept {
        ctx.accept_clean_ends();
    }
    let obs = ObservedState::new();
    let run = run_call_with(call_bobbed(&alice, &bob), obs.clone(), &ctx, STEP_TIMEOUT, None).await;
    bob_task.await.expect("bob's side completes");
    let verdict = verdict(Expect::answered(), run, &obs, "alice");
    h.finish().await;
    verdict
}

#[tokio::test(start_paused = true)]
async fn a_caller_refused_ends_the_call_cleanly_once_clean_ends_are_accepted() {
    let verdict = refused("actor-clean-end-503", true).await;
    assert_eq!(verdict.branch(), Some(ScenarioVerdict::ENDED_CLEANLY), "{verdict:?}");
}

#[tokio::test(start_paused = true)]
async fn a_caller_refused_fails_the_call_unless_clean_ends_are_accepted() {
    let verdict = refused("actor-clean-end-503-strict", false).await;
    assert!(!verdict.is_success(), "{verdict:?}");
}
