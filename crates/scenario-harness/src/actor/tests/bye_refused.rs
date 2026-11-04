//! A 481 to a party's BYE ends her dialog all the same (RFC 3261 §15.1.1):
//! no peer holds it, and the call tears down.

use std::time::Duration;

use super::testkit::caller_spec;
use crate::actor::*;
use crate::{Harness, ANSWER_SDP};

#[tokio::test(start_paused = true)]
async fn a_bye_answered_481_ends_the_dialog() {
    let h = Harness::new("actor-bye-481").describe(
        "alice hangs up the call bob answered; bob no longer knows the dialog and \
         answers her BYE 481: her dialog ends and the call tears down",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            let mut bye = bob.receive("BYE").await;
            bye.respond(481, "Call/Transaction Does Not Exist").await;
        })
    };
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::AllConfirmed(&["alice"]), GoalStep::Bye),
    ];
    let call = CallPlan {
        actors: vec![caller_spec("alice", &alice, ("bob", bob.clone()), goals)],
        plan: vec![phase("established", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the call tears down, got {verdict:?}");
    bob_task.await.expect("bob's side completes");
    h.finish().await;
}
