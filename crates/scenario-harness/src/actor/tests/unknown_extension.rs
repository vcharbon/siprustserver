//! A requirement no party implements (RFC 3261 §8.2.2.3): the requester takes
//! its 420 as the request's answer, and an actor refuses it 420 naming it.

use std::time::Duration;

use super::testkit::{caller_spec, scripted_spec};
use crate::actor::*;
use crate::{Harness, ANSWER_SDP};

/// Alice calls bob, sends an INFO requiring `x-nonexistent` once established,
/// and hangs up once it is refused 420; `bob_actor` makes bob an actor too.
fn plan(alice: &crate::Agent, bob: &crate::Agent, bob_actor: bool) -> CallPlan {
    let info = GoalStep::InDialog {
        method: sip_message::generators::InDialogMethod::Info,
        content_type: None,
        body: None,
        headers: vec![("Require".to_string(), "x-nonexistent".to_string())],
    };
    let refused = |s: &StateInner| {
        s.leg("alice").responses().iter().any(|r| r.cseq_method == "INFO" && r.status == 420)
    };
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::AllConfirmed(&["alice"]), info),
        Goal::new(Barrier::pred("refused", refused), GoalStep::Bye),
    ];
    let mut actors = vec![caller_spec("alice", alice, ("bob", bob.clone()), goals)];
    if bob_actor {
        actors.push(ActorSpec {
            disposition: Disposition::Answer,
            ..scripted_spec("bob", bob, vec![])
        });
    }
    CallPlan {
        actors,
        plan: vec![phase("established", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

#[tokio::test(start_paused = true)]
async fn an_info_requiring_an_unknown_extension_refused_420_completes_the_call() {
    let h = Harness::new("actor-info-require-420").describe(
        "alice's INFO requires an extension bob does not implement; he refuses it 420 naming \
         it, and the call ends with her BYE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            let mut info = bob.receive("INFO").await;
            info.respond(420, "Bad Extension").with_header("Unsupported", "x-nonexistent").await;
            bob.receive("BYE").await.respond(200, "OK").await;
        })
    };
    let verdict = run_call(plan(&alice, &bob, false), Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the 420 is the answer the requirement draws, got {verdict:?}");
    bob_task.await.expect("bob's side completes");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn an_actor_refuses_an_info_requiring_an_unknown_extension_420() {
    let h = Harness::new("actor-refuses-require-420").describe(
        "alice's INFO requires an extension bob's actor does not implement: he refuses it 420 \
         with Unsupported naming it, and the call ends with her BYE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let verdict = run_call(plan(&alice, &bob, true), Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the call completes, got {verdict:?}");
    h.finish().await;
}
