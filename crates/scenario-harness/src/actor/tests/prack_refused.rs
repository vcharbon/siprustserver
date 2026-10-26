//! A PRACK a callee answers 481 — its provisional already acknowledged —
//! still arrived: its CSeq is no hole in the dialog's sequence, and a later
//! request settles the call.

use std::time::Duration;

use crate::actor::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

#[tokio::test(start_paused = true)]
async fn a_prack_refused_481_leaves_no_hole_in_the_sequence() {
    let h = Harness::new("actor-prack-481-no-hole").describe(
        "bob answers alice's reliable call after her PRACK; she PRACKs the 183 again \
         (481), then hangs up: the call settles",
    );
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
            call.expect(200).await;
            let mut late = call.try_prack(&p183).await.expect("alice PRACKs again");
            late.expect(481).await;
            let mut dialog = call.ack().await;
            dialog.bye().await.expect(200).await;
        })
    };

    let call = CallPlan {
        actors: vec![ActorSpec {
            role: "bob",
            agent: bob.clone(),
            disposition: Disposition::ReliableAnswer,
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
    assert!(verdict.is_ok(), "the call settles, got {verdict:?}");
    alice_task.await.expect("alice's side");
    h.finish().await;
}
