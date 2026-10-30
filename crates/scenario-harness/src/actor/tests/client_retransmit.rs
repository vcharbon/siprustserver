//! A request a leg sent and still awaits an answer to is retransmitted over
//! the unreliable transport: any non-INVITE request on its Timer E until its
//! final (RFC 3261 §17.1.2.2), an INVITE on its Timer A until a response
//! (§17.1.1.2); a final to an INVITE goes again until its ACK (§13.3.1.4,
//! §17.2.1). A leg playing a peer that never retransmits opts out.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sip_net::{Arrival, PreIngressAction};

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
        call_id: None,
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

/// Alice's call to a hand-scripted bob who answers her BYE only after 1.2 s.
async fn bye_answered_late(h: &Harness, silenced: bool) -> crate::Agent {
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    if silenced {
        alice.silence_retransmissions();
    }
    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            bob.receive("ACK").await;
            let mut bye = bob.receive("BYE").await;
            tokio::time::sleep(Duration::from_millis(1200)).await;
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
    bob.sight_queued().await;
    bob
}

#[tokio::test(start_paused = true)]
async fn a_bye_left_unanswered_is_retransmitted_until_its_final() {
    let h = Harness::new("actor-bye-timer-e").describe(
        "alice hangs up; bob answers the BYE only after 1.2 s, so alice's Timer E \
         retransmits it once at T1",
    );
    let bob = bye_answered_late(&h, false).await;
    assert_eq!(received(&bob, "BYE"), 2, "the BYE and its one Timer E retransmission");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_party_playing_a_peer_that_never_retransmits_sends_its_bye_once() {
    let h = Harness::new("actor-bye-silenced").describe(
        "alice, who plays a peer that never retransmits, hangs up; bob answers the BYE \
         only after 1.2 s: the BYE left once",
    );
    let bob = bye_answered_late(&h, true).await;
    assert_eq!(received(&bob, "BYE"), 1, "no retransmission");
    h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_non_2xx_final_left_unacked_is_retransmitted_until_its_ack() {
    let h = Harness::new("actor-timer-g").describe(
        "bob rejects alice's INVITE 486; the first 486 is lost before alice, so no ACK \
         comes and bob's Timer G sends the 486 again at T1; alice ACKs that one",
    );
    let copies = Arc::new(AtomicUsize::new(0));
    let counter = copies.clone();
    let alice = h
        .agent_with_pre_ingress(
            "alice",
            "127.0.0.1:5060",
            Arc::new(move |arrival: &mut Arrival, _src, _depth| {
                if arrival.raw().starts_with(b"SIP/2.0 486")
                    && counter.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return PreIngressAction::Drop;
                }
                PreIngressAction::Accept
            }),
        )
        .await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let alice_task = {
        let alice = alice.clone();
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).send().await;
            call.expect(486).await;
        })
    };
    let call = CallPlan {
        actors: vec![ActorSpec {
            role: "bob",
            agent: bob.clone(),
            disposition: Disposition::Reject(486),
            media: MediaState::answer(ANSWER_SDP),
            goals: vec![],
            invite_targets: vec![],
            via: None,
            feed: CtxFeed::default(),
            cseq: None,
            delayed: vec![],
            claim: None,
        }],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the call completes, got {verdict:?}");
    alice_task.await.expect("alice's side completes");
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(copies.load(Ordering::SeqCst), 2, "the lost 486 and its one Timer G repeat");
    assert_eq!(received(&bob, "ACK"), 1, "alice's ACK of the repeat");
    h.finish().await;
}
