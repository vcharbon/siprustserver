//! A request a leg sent and still awaits an answer to is retransmitted over
//! the unreliable transport: a PRACK on its Timer E until its final (RFC 3261
//! §17.1.2.2), a re-INVITE on its Timer A until a response (§17.1.1.2).

use std::time::Duration;

use crate::actor::*;
use crate::realcall::InvitePlan;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// The datagrams `agent` received whose start line is a `method` request.
fn received(agent: &crate::Agent, method: &str) -> usize {
    let start = format!("{method} ");
    agent.wire_view().iter().filter(|e| e.raw.starts_with(start.as_bytes())).count()
}

#[tokio::test(start_paused = true)]
async fn a_prack_left_unanswered_is_retransmitted_until_its_final() {
    let h = Harness::new("actor-prack-timer-e").describe(
        "alice PRACKs bob's reliable 183; bob answers the PRACK only after 1.2 s, so \
         alice's Timer E retransmits it once at T1; the call then completes",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER_SDP).await;
            let mut prack = bob.receive("PRACK").await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            prack.respond(200, "OK").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            let mut bye = bob.receive("BYE").await;
            bye.respond(200, "OK").await;
        })
    };

    let plan = InvitePlan {
        via: bob.addr,
        from: None,
        to: None,
        ruri: None,
        headers: vec![("Supported".into(), "100rel".into())],
        rewrite: Default::default(),
    };
    let call = CallPlan {
        actors: vec![ActorSpec {
            role: "alice",
            agent: alice.clone(),
            disposition: Disposition::Caller,
            media: MediaState::offer(OFFER_SDP),
            goals: vec![
                Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: Some(plan) }),
                Goal::new(Barrier::AllConfirmed(&["alice"]), GoalStep::Bye),
            ],
            invite_targets: vec![("bob", bob.clone())],
            via: None,
            feed: CtxFeed::default(),
            cseq: None,
            delayed: vec![],
            claim: None,
        }],
        plan: vec![phase("established", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the call completes, got {verdict:?}");
    bob_task.await.expect("bob's side completes");
    assert_eq!(received(&bob, "PRACK"), 2, "the PRACK and its one Timer E retransmission");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_reinvite_left_unanswered_is_retransmitted_until_a_response() {
    let h = Harness::new("actor-reinvite-timer-a").describe(
        "alice re-INVITEs the established call; bob answers it only after 1.2 s, so \
         alice's Timer A retransmits it once at T1; the call then completes",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            let mut reinvite = bob.receive("INVITE").await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            reinvite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            let mut bye = bob.receive("BYE").await;
            bye.respond(200, "OK").await;
        })
    };

    let call = CallPlan {
        actors: vec![ActorSpec {
            role: "alice",
            agent: alice.clone(),
            disposition: Disposition::Caller,
            media: MediaState::offer(OFFER_SDP),
            goals: vec![
                Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                Goal::new(Barrier::AllConfirmed(&["alice"]), GoalStep::Reinvite),
                Goal::new(
                    Barrier::pred("reneg_done", |s| s.leg("alice").reneg_count() >= 1),
                    GoalStep::Bye,
                ),
            ],
            invite_targets: vec![("bob", bob.clone())],
            via: None,
            feed: CtxFeed::default(),
            cseq: None,
            delayed: vec![],
            claim: None,
        }],
        plan: vec![phase("established", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the call completes, got {verdict:?}");
    bob_task.await.expect("bob's side completes");
    assert_eq!(
        received(&bob, "INVITE"),
        3,
        "the INVITE, the re-INVITE and the re-INVITE's one Timer A retransmission"
    );
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_2xx_left_unacked_is_retransmitted_until_its_ack() {
    let h = Harness::new("actor-2xx-until-ack").describe(
        "bob answers alice's INVITE 200; alice ACKs only after 1.2 s, so bob's 2xx \
         goes again once at T1 (RFC 3261 §13.3.1.4); the call then completes",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let alice_task = {
        let alice = alice.clone();
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).send().await;
            call.expect(200).await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
            let mut dialog = call.ack().await;
            tokio::time::sleep(Duration::from_millis(1000)).await;
            let mut bye = dialog.bye().await;
            bye.expect(200).await;
        })
    };

    let call = CallPlan {
        actors: vec![ActorSpec {
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
            claim: None,
        }],
        plan: vec![phase("established", |s| s.leg_at_least("bob", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the call completes, got {verdict:?}");
    alice_task.await.expect("alice's side completes");
    let answers = alice.wire_view().iter().filter(|e| e.raw.starts_with(b"SIP/2.0 200")).count();
    assert_eq!(answers, 3, "the 2xx, its one repeat before the ACK, and the BYE's 200");
    h.finish().await;
}
