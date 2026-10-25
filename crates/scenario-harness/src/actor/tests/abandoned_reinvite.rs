//! A caller's re-INVITE still pending when a BYE ends the dialog is given up
//! to its endpoint: the non-2xx final it draws afterwards is ACKed hop by hop
//! (RFC 3261 §17.1.1.3), whether or not the caller's actor still runs.

use std::time::Duration;

use crate::actor::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

#[tokio::test(start_paused = true)]
async fn a_reinvite_pending_at_the_bye_still_acks_its_final() {
    let h = Harness::new("actor-reinvite-abandoned-at-bye").describe(
        "alice re-INVITEs; bob BYEs the dialog under it, then rejects the re-INVITE 487; \
         alice's endpoint ACKs the 487 though the dialog ended",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let bob_task = {
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut invite = bob.receive("INVITE").await;
            invite.respond(200, "OK").with_sdp(ANSWER_SDP).await;
            let mut dialog = invite.dialog();
            bob.receive("ACK").await;
            let mut reinvite = bob.receive("INVITE").await;
            reinvite.respond(100, "Trying").await;
            let mut bye = dialog.bye().await;
            bye.expect(200).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            reinvite.respond(487, "Request Terminated").await;
            reinvite.expect_ack().await;
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
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = alice.take_queued_inbound().await;
    bob_task.await.expect("bob saw the 487's ACK");
    h.finish().await;
}
