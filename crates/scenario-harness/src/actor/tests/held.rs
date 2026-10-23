//! The held callee ([`Disposition::Held`]): goals ring, send early-dialog
//! requests, and answer; a CANCEL ends it with no goal left owed.

use std::time::Duration;

use sip_message::generators::InDialogMethod;

use crate::actor::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

fn held_bob(bob: &crate::Agent, goals: Vec<Goal>) -> ActorSpec {
    ActorSpec {
        role: "bob",
        agent: bob.clone(),
        disposition: Disposition::Held,
        media: MediaState::answer(ANSWER_SDP),
        goals,
        invite_targets: vec![],
        via: None,
        feed: CtxFeed::default(),

        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

fn plan(actors: Vec<ActorSpec>, phases: Vec<BarrierPhase>) -> CallPlan {
    CallPlan {
        actors,
        plan: phases,
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

/// Two 2xx to `method` requests `role` sent.
fn answered(role: &'static str, method: &'static str, n: usize) -> Barrier {
    Barrier::pred("early_exchanged", move |s| {
        s.leg(role)
            .responses()
            .iter()
            .filter(|f| (200..300).contains(&f.status) && f.cseq_method == method)
            .count()
            >= n
    })
}

/// A held callee rings reliably, waits for the PRACK, sends two UPDATEs on
/// its early dialog, answers, then re-INVITEs and hangs up: its requests
/// number 1, 2, 3, 4 across the confirmation (RFC 3261 §12.2.1.1), which the
/// caller's dialog table judges on every one.
#[tokio::test(start_paused = true)]
async fn a_held_callee_numbers_its_requests_on_across_the_answer() {
    let h = Harness::new("actor-held-early-updates");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let alice_task = {
        let alice = alice.clone();
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut call = alice
                .invite(&bob)
                .with_sdp(OFFER_SDP)
                .with_header("Supported", "100rel")
                .send()
                .await;
            let p183 = call.expect(183).await;
            let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the 183");
            prack.expect(200).await;
            let mut seen = Vec::new();
            for _ in 0..2 {
                let mut update = alice.receive("UPDATE").await;
                seen.push(update.request().cseq().seq());
                update.respond(200, "OK").with_sdp(OFFER_SDP).await;
            }
            call.expect(200).await;
            let _dialog = call.ack().await;
            let mut reinvite = alice.receive("INVITE").await;
            seen.push(reinvite.request().cseq().seq());
            reinvite.respond(200, "OK").with_sdp(OFFER_SDP).await;
            alice.receive("ACK").await;
            let mut bye = alice.receive("BYE").await;
            seen.push(bye.request().cseq().seq());
            bye.respond(200, "OK").await;
            seen
        })
    };

    let confirmed = Barrier::AllConfirmed(&["bob"]);
    let reinvited = Barrier::pred("reinvited", |s| s.leg("bob").reneg_count() >= 3);
    let call = plan(
        vec![held_bob(
            &bob,
            vec![
                Goal::new(Barrier::None, GoalStep::Ring { forks: &[], reliable: true }),
                Goal::new(
                    Barrier::None,
                    GoalStep::EarlyRequest { method: InDialogMethod::Update, fork: None },
                ),
                Goal::new(
                    answered("bob", "UPDATE", 1),
                    GoalStep::EarlyRequest { method: InDialogMethod::Update, fork: None },
                ),
                Goal::new(answered("bob", "UPDATE", 2), GoalStep::Answer { winner: None }),
                Goal::new(confirmed, GoalStep::Reinvite),
                Goal::new(reinvited, GoalStep::Bye),
            ],
        )],
        vec![phase("established", |s| s.leg_at_least("bob", LegPhase::Confirmed))],
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the held call settles, got {verdict:?}");
    let seen = alice_task.await.expect("alice's side");
    let m = crate::Party::Callee.first_cseq();
    assert_eq!(seen, vec![m, m + 1, m + 2, m + 3], "one CSeq sequence across the answer");

    h.finish().await;
}

/// A held callee rings reliably and rejects only once its 183 is PRACKed: a
/// caller that PRACKs late gets the PRACK's 200 first, then the 486, which it
/// ACKs, and the call settles.
#[tokio::test(start_paused = true)]
async fn a_held_callee_rejects_after_its_reliable_provisional_is_pracked() {
    let h = Harness::new("actor-held-reject");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let alice_task = {
        let alice = alice.clone();
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut call = alice
                .invite(&bob)
                .with_sdp(OFFER_SDP)
                .with_header("Supported", "100rel")
                .send()
                .await;
            let p183 = call.expect(183).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the 183");
            prack.expect(200).await;
            call.expect(486).await;
        })
    };

    let call = plan(
        vec![held_bob(
            &bob,
            vec![
                Goal::new(Barrier::None, GoalStep::Ring { forks: &[], reliable: true }),
                Goal::new(Barrier::None, GoalStep::Reject { status: 486 }),
            ],
        )],
        vec![],
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the rejected held call settles, got {verdict:?}");
    alice_task.await.expect("alice's side");

    h.finish().await;
}

/// A PRACK that reaches a callee after its own non-2xx final names an early
/// dialog the final ended: it draws 481.
#[tokio::test(start_paused = true)]
async fn a_prack_after_the_callee_s_reject_draws_481() {
    let h = Harness::new("actor-held-reject-late-prack");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let alice_task = {
        let alice = alice.clone();
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut call = alice
                .invite(&bob)
                .with_sdp(OFFER_SDP)
                .with_header("Supported", "100rel")
                .send()
                .await;
            let p183 = call.expect(183).await;
            let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the 183");
            prack.expect(200).await;
            call.expect(486).await;
            let mut late = call.try_prack(&p183).await.expect("alice PRACKs again");
            late.expect(481).await;
        })
    };

    let call = plan(
        vec![held_bob(
            &bob,
            vec![
                Goal::new(Barrier::None, GoalStep::Ring { forks: &[], reliable: true }),
                Goal::new(Barrier::None, GoalStep::Reject { status: 486 }),
            ],
        )],
        vec![],
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the rejected held call settles, got {verdict:?}");
    alice_task.await.expect("alice's side");

    h.finish().await;
}

/// A CANCEL that reaches a held callee before it rang 487s the INVITE and
/// drops the ring and answer goals: the call ends abandoned and settles.
#[tokio::test(start_paused = true)]
async fn a_cancel_before_the_ring_drops_the_held_goals() {
    let h = Harness::new("actor-held-cancelled");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let alice_task = {
        let alice = alice.clone();
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).send().await;
            call.expect(100).await;
            let mut cancel = call.cancel().await;
            cancel.expect(200).await;
            call.expect(487).await;
        })
    };

    let call = plan(
        vec![held_bob(
            &bob,
            vec![
                Goal::new(Barrier::None, GoalStep::Ring { forks: &[], reliable: false })
                    .after(Duration::from_secs(1)),
                Goal::new(Barrier::None, GoalStep::Answer { winner: None })
                    .after(Duration::from_secs(1)),
            ],
        )],
        vec![],
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the cancelled held call settles, got {verdict:?}");
    alice_task.await.expect("alice's side");

    h.finish().await;
}
