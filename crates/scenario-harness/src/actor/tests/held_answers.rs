//! A party's drawn answers ([`Answers`]): a renegotiation answered after a
//! hold or refused, a refusal stating Retry-After retried a bounded number of
//! times (RFC 3261 §14.1), and a PRACK held, yet sent before the final it
//! would overtake (RFC 3262 §4).

use std::time::Duration;

use tokio::time::Instant;

use super::testkit::caller_spec;
use crate::actor::answers::{Answer, Answers};
use crate::actor::*;
use crate::realcall::InvitePlan;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// Alice calls bob, re-INVITEs once established and hangs up once that
/// renegotiation completed; bob answers as `answers` states.
fn reinvite_plan(alice: &crate::Agent, bob: &crate::Agent, answers: Answers) -> CallPlan {
    let renegotiated = |s: &StateInner| s.leg("alice").reneg_count() >= 1;
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Reinvite),
        Goal::new(Barrier::pred("renegotiated", renegotiated), GoalStep::Bye),
    ];
    let bob_spec = ActorSpec {
        role: "bob",
        agent: bob.clone(),
        disposition: Disposition::Answer,
        media: MediaState::answer(ANSWER_SDP),
        goals: vec![],
        invite_targets: vec![],
        via: None,
        feed: CtxFeed::default(),
        cseq: None,
        delayed: vec![],
        answers,
        claim: None,
    };
    CallPlan {
        actors: vec![caller_spec("alice", alice, ("bob", bob.clone()), goals), bob_spec],
        plan: vec![phase("established", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_held_reinvite_answer_completes_the_renegotiation_after_its_hold() {
    let h = Harness::new("actor-held-reinvite-answer")
        .describe("bob holds his answer to alice's re-INVITE 2 s, then answers it; the call ends");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let hold = Duration::from_secs(2);
    let answers = Answers { reinvite: Answer { hold, refuse: None }, ..Default::default() };
    let started = Instant::now();
    let verdict = run_call(reinvite_plan(&alice, &bob, answers), Duration::from_secs(10)).await;
    assert!(verdict.is_ok(), "the held renegotiation completes, got {verdict:?}");
    assert!(started.elapsed() >= hold, "the answer waited out its hold");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_refused_reinvite_leaves_the_call_up_to_its_bye() {
    let h = Harness::new("actor-refused-reinvite").describe(
        "bob refuses alice's re-INVITE 488, which asks no retry: her renegotiation ends \
         refused and her BYE follows",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let refuse = Answer { hold: Duration::ZERO, refuse: Some(488) };
    let answers = Answers { reinvite: refuse, ..Default::default() };
    let verdict = run_call(reinvite_plan(&alice, &bob, answers), Duration::from_secs(10)).await;
    assert!(verdict.is_ok(), "the refused renegotiation ends the round, got {verdict:?}");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_refused_500_every_time_is_retried_three_times() {
    let h = Harness::new("actor-reinvite-retry-bound").describe(
        "bob refuses every re-INVITE 500 with Retry-After: 1; alice retries three times, then \
         gives up and hangs up",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            for _ in 0..4 {
                let mut again = bob.receive("INVITE").await;
                again.respond(500, "Server Internal Error").with_header("Retry-After", "1").await;
            }
            bob.receive("BYE").await.respond(200, "OK").await;
        })
    };
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::AllConfirmed(&["alice"]), GoalStep::Reinvite),
        Goal::new(Barrier::pred("gave_up", |s| s.leg("alice").reneg_count() >= 1), GoalStep::Bye),
    ];
    let call = CallPlan {
        actors: vec![caller_spec("alice", &alice, ("bob", bob.clone()), goals)],
        plan: vec![phase("established", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };
    let verdict = run_call(call, Duration::from_secs(20)).await;
    assert!(verdict.is_ok(), "the bounded retries end the round, got {verdict:?}");
    bob_task.await.expect("bob saw the first re-INVITE and three retries, then the BYE");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_held_prack_is_sent_before_the_final_it_would_overtake() {
    let h = Harness::new("actor-held-prack-before-final").describe(
        "alice holds her PRACKs 2 s; bob sends a reliable 183 and his 200 at once: her PRACK \
         leaves before she takes the 200, then her ACK",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(183, "Session Progress").reliable(1).await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("PRACK").await.respond(200, "OK").await;
            bob.receive("ACK").await;
            bob.receive("BYE").await.respond(200, "OK").await;
        })
    };
    let plan = InvitePlan {
        via: bob.addr,
        from: None,
        to: None,
        ruri: None,
        call_id: None,
        headers: vec![("Supported".into(), "100rel".into())],
        rewrite: Default::default(),
    };
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: Some(plan) }),
        Goal::new(Barrier::AllConfirmed(&["alice"]), GoalStep::Bye),
    ];
    let mut alice_spec = caller_spec("alice", &alice, ("bob", bob.clone()), goals);
    alice_spec.media = MediaState::offer(OFFER_SDP);
    alice_spec.answers = Answers { prack: Duration::from_secs(2), ..Default::default() };
    let call = CallPlan {
        actors: vec![alice_spec],
        plan: vec![phase("established", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };
    let verdict = run_call(call, Duration::from_secs(10)).await;
    assert!(verdict.is_ok(), "the call completes, got {verdict:?}");
    bob_task.await.expect("bob took the PRACK before the ACK");
    h.finish().await;
}
