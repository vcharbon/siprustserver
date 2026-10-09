//! The reactive actor answers what its UA's dialog table refuses on its own
//! (RFC 3261 §12.2.2), and the verdict still fails the run unless opted in.

use std::time::Duration;

use sip_message::generators::InDialogMethod;

use crate::actor::*;
use crate::dialogs::DIALOG_UNKNOWN;
use crate::{Harness, WaiverScope, ANSWER_SDP, OFFER_SDP};

/// bob, an actor that answers at once and stays up until the caller's BYE.
fn answering_bob(bob: &crate::Agent) -> CallPlan {
    CallPlan {
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
    }
}

/// alice calls the actor bob, sends an OPTIONS under a To-tag bob never
/// minted, and expects bob's own 481; then she hangs up. The forged request
/// spends nothing of her dialog's CSeq space (§12.2.1.1); the audit's charge
/// to alice for it is waived, being the stimulus.
async fn forged_options_drawn_481(h: &Harness) {
    h.waive(
        WaiverScope::rule("mid-dialog-tags", "alice forges the To-tag on purpose")
            .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let alice_task = {
        let (alice, bob) = (alice.clone(), bob.clone());
        tokio::spawn(async move {
            let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).send().await;
            call.expect(200).await;
            let mut dialog = call.ack().await;
            let before = dialog.local_cseq();
            let mut forged =
                dialog.send_request(InDialogMethod::Options).with_to_tag("bob-forged").send().await;
            dialog.set_local_cseq(before);
            forged.expect(481).await;
            let mut bye = dialog.bye().await;
            bye.expect(200).await;
        })
    };
    let verdict = run_call(answering_bob(&bob), Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the call itself completes, got {verdict:?}");
    alice_task.await.unwrap();
}

/// Opted in, the run passes: the 481 came from the actor itself.
#[tokio::test(start_paused = true)]
async fn the_actor_refuses_a_request_on_no_dialog_with_481() {
    let h = Harness::new("actor-dialog-481")
        .describe("the actor bob answers 481 to an OPTIONS under a To-tag he never minted");
    h.waive(
        WaiverScope::rule(DIALOG_UNKNOWN, "alice forges the To-tag on purpose").on_party("bob"),
    );
    forged_options_drawn_481(&h).await;
    h.finish().await;
}

/// Not opted in, the verdict fails the run at `finish`, naming the UA, the
/// request and the tag it holds.
#[tokio::test(start_paused = true)]
#[should_panic(expected = "bob: OPTIONS To-tag bob-forged names no dialog (holds bob-tag-")]
async fn the_actors_verdict_fails_the_run_unless_opted_in() {
    let h = Harness::new("actor-dialog-gate");
    forged_options_drawn_481(&h).await;
    h.finish().await;
}
