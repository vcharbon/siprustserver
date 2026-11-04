//! A deliberate dwell (a talk time, a ring) is never charged to the
//! controller's 64·T1 wait bound: a call held three minutes ends on its BYE,
//! while a message that never comes still fails the call 64·T1 after the
//! dwell that preceded it.

use std::time::Duration;

use tokio::time::Instant;

use super::testkit::{caller_spec, scripted_spec};
use crate::actor::scenarios::BasicCall;
use crate::actor::*;
use crate::agent::Inbound;
use crate::realcall::{CallCtx, CallEnv};
use crate::{Harness, StepError, ANSWER_SDP, OFFER_SDP};
use sip_message::Method;

const TALK: Duration = Duration::from_secs(180);
const RING: Duration = Duration::from_secs(1);

/// A basic call (INVITE/180/200/ACK, talk, BYE) between two parties with no
/// system in between, the caller holding the answered call [`TALK`].
#[tokio::test(start_paused = true)]
async fn a_call_held_three_minutes_ends_on_its_bye() {
    let h = Harness::new("actor-long-talk");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let mut env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Corr", "t");
    env.ring_delay = RING;
    env.talk_time = TALK;
    let start = Instant::now();
    let verdict = run_actor_scenario(&BasicCall, &env, &CallCtx::new()).await;
    let took = start.elapsed();
    assert_eq!(verdict.branch(), Some("answered"), "{verdict:?}");
    assert!(took >= RING + TALK, "the BYE came before the talk time ended: {took:?}");
    assert!(took < RING + TALK + Duration::from_secs(2), "the call outlived its BYE: {took:?}");
    h.finish().await;
}

/// A callee that rings and never answers: no party scheduled the wait for the
/// answer, so the call fails 64·T1 after the ring, however long a talk the
/// caller plans once answered.
#[tokio::test(start_paused = true)]
async fn an_answer_that_never_comes_fails_the_call_64_t1_after_the_ring() {
    let h = Harness::new("actor-long-talk-unanswered");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let established = |s: &StateInner| s.leg_at_least("alice", LegPhase::Confirmed);
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::pred("established", established), GoalStep::Bye).after(TALK),
    ];
    let mut silent = scripted_spec("bob", &bob, vec![]);
    silent.disposition = Disposition::RingThenSilent;
    let plan = CallPlan {
        actors: vec![caller_spec("alice", &alice, ("bob", bob.clone()), goals), silent],
        plan: vec![phase("established", established)],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };
    let start = Instant::now();
    let run = run_call_with(plan, ObservedState::new(), &CallCtx::new(), STEP_TIMEOUT, None).await;
    let took = start.elapsed();
    assert!(matches!(run, CallVerdict::Failed(..)), "{run:?}");
    assert!(
        took >= STEP_TIMEOUT && took < STEP_TIMEOUT + Duration::from_secs(2),
        "the unanswered call failed after {took:?}"
    );
    h.finish().await;
}

/// Bob, hand-scripted: answers alice's INVITE, then takes her BYE and never
/// answers it — the missing final this test is about.
fn bob_ignores_the_bye(bob: &crate::Agent) -> tokio::task::JoinHandle<()> {
    let bob = bob.clone();
    tokio::spawn(async move {
        let mut invite = bob.receive("INVITE").await;
        invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        bob.receive("ACK").await;
        loop {
            match bob.recv_any().await {
                Ok(Inbound::Request(r)) if r.request().method() == Method::Bye => return,
                Ok(_) | Err(StepError::Timeout { .. }) => {}
                Err(e) => panic!("bob: {e:?}"),
            }
        }
    })
}

/// The BYE that ends a three-minute talk draws no final: the call fails
/// 64·T1 after the BYE, not a talk time later.
#[tokio::test(start_paused = true)]
async fn a_bye_that_draws_no_answer_fails_the_call_64_t1_after_it() {
    let h = Harness::new("actor-long-talk-bye-unanswered")
        .describe("RFC 3261 §17.1.2.2: the BYE draws no final; the caller gives up at 64·T1");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob_task = bob_ignores_the_bye(&bob);
    let established = |s: &StateInner| s.leg_at_least("alice", LegPhase::Confirmed);
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
        Goal::new(Barrier::pred("established", established), GoalStep::Bye).after(TALK),
    ];
    let plan = CallPlan {
        actors: vec![caller_spec("alice", &alice, ("bob", bob.clone()), goals)],
        plan: vec![phase("established", established)],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };
    let start = Instant::now();
    let run = run_call_with(plan, ObservedState::new(), &CallCtx::new(), STEP_TIMEOUT, None).await;
    let took = start.elapsed();
    bob_task.await.expect("bob's side completes");
    assert!(matches!(run, CallVerdict::Failed(..)), "{run:?}");
    assert!(
        took >= TALK + STEP_TIMEOUT && took < TALK + STEP_TIMEOUT + Duration::from_secs(3),
        "the unanswered BYE failed the call after {took:?}: {run:?}"
    );
    h.finish().await;
}

/// The system in the middle, hand-scripted: it answers alice's INVITE once
/// bob answers its own, BYEs alice 10 s into her talk, and never releases
/// bob's leg — the leak this test is about.
fn middle_ends_alice_and_leaks_bob(
    middle: &crate::Agent,
    bob: &crate::Agent,
) -> tokio::task::JoinHandle<()> {
    let (middle, bob) = (middle.clone(), bob.clone());
    tokio::spawn(async move {
        let mut invite = middle.receive("INVITE").await;
        let mut onward = middle.invite(&bob).with_sdp(OFFER_SDP).send().await;
        onward.expect(200).await;
        let _leaked = onward.ack().await;
        invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        let mut dialog = invite.dialog();
        middle.receive("ACK").await;
        tokio::time::sleep(EARLY_BYE).await;
        dialog.bye().await.expect(200).await;
    })
}

/// When the middle ends alice's call.
const EARLY_BYE: Duration = Duration::from_secs(10);

/// Alice's talk ends early on the middle's BYE while bob's leg is never
/// released: the talk alice will no longer finish extends nothing, so the
/// call fails 64·T1 after the BYE, not after the talk.
#[tokio::test(start_paused = true)]
async fn an_abandoned_talk_extends_no_wait() {
    let h = Harness::new("actor-long-talk-abandoned")
        .describe("RFC 3261 §15: the middle BYEs the caller early and leaks the callee leg");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let middle = h.agent("middle", "127.0.0.1:5080").await;
    let middle_task = middle_ends_alice_and_leaks_bob(&middle, &bob);
    let established = |s: &StateInner| s.leg_at_least("alice", LegPhase::Confirmed);
    let goals = vec![
        Goal::new(Barrier::None, GoalStep::Invite { callee: "middle", plan: None }),
        Goal::new(Barrier::pred("established", established), GoalStep::Bye).after(TALK),
    ];
    let mut answering = scripted_spec("bob", &bob, vec![]);
    answering.disposition = Disposition::Answer;
    let plan = CallPlan {
        actors: vec![caller_spec("alice", &alice, ("middle", middle.clone()), goals), answering],
        plan: vec![phase("established", established)],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };
    let ctx = CallCtx::new();
    ctx.accept_clean_ends();
    let start = Instant::now();
    let run = run_call_with(plan, ObservedState::new(), &ctx, STEP_TIMEOUT, None).await;
    let took = start.elapsed();
    middle_task.await.expect("the middle's side completes");
    assert!(matches!(run, CallVerdict::Failed(..)), "{run:?}");
    assert!(
        took >= EARLY_BYE + STEP_TIMEOUT
            && took < EARLY_BYE + STEP_TIMEOUT + Duration::from_secs(3),
        "the leaked leg failed the call after {took:?}: {run:?}"
    );
    h.finish().await;
}
