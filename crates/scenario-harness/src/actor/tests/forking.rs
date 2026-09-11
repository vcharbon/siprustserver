use std::time::Duration;

use crate::actor::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// A bob-only [`CallPlan`] with the given forking disposition — the C1(a)
/// machinery rig: alice is hand-rolled (the fork dance on the caller side is
/// the C1(b) capability; here the CALLEE's emission is the subject).
fn forking_bob_plan(bob: &crate::Agent, disposition: Disposition) -> CallPlan {
    CallPlan {
        actors: vec![ActorSpec {
            role: "bob",
            agent: bob.clone(),
            disposition,
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

/// C1(a): a forking callee emits DISTINCT-tag 18x on the ONE INVITE server
/// transaction and answers 200 under the WINNING tag. The hand-rolled caller
/// sees two 180s with distinct To-tags (two early dialogs, RFC 3261
/// §12.1.2), the 200 carries the declared winner's tag, and the confirmed
/// dialog (ACK + BYE) rides that tag. The RFC hard gate at `finish()`
/// confirms the forked wire stayed compliant.
#[tokio::test(start_paused = true)]
async fn forking_ring_emits_distinct_tag_18x_and_answers_winner() {
    let h = Harness::new("actor-forking-ring").describe(
        "C1(a): bob (ForkingRing) emits 180(f1) + 180(f2) — distinct explicit \
         To-tags on one INVITE server txn — then 200 under the winner f2; the \
         hand-rolled alice confirms and BYEs the winning dialog",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let alice_task = {
        let alice = alice.clone();
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).send().await;
            let p1 = call.expect(180).await;
            let t1 = p1.to().tag().expect("fork1 tag");
            let p2 = call.expect(180).await;
            let t2 = p2.to().tag().expect("fork2 tag");
            assert_ne!(t1, t2, "each fork's 18x carries a DISTINCT To-tag");
            assert_eq!(t1, "f1");
            assert_eq!(t2, "f2");
            let ok = call.expect(200).await;
            assert_eq!(ok.to().tag(), Some("f2"), "the 200 is under the winner tag");
            let mut dialog = call.ack().await;
            let mut bye = dialog.bye().await;
            bye.expect(200).await;
        })
    };

    let call = forking_bob_plan(
        &bob,
        Disposition::ForkingRing {
            tags: &["f1", "f2"],
            winner: "f2",
            ring: Duration::from_millis(200),
            reliable: false,
            loser_late_200: None,
        },
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the forked call must settle OK, got {verdict:?}");

    alice_task.await.unwrap();
    h.finish().await;
}

/// C1(a) loser-late-200 variant: after the winner's 200 (tag f2) the callee
/// emits a LATE 200 under the losing tag f1. The caller ACKs the loser's 200
/// on ITS OWN fork dialog and BYEs it (RFC 3261 §13.2.2.4) — the callee's
/// reactor 200s that BYE WITHOUT terminating its leg (the winning dialog
/// lives on and is BYE'd normally afterwards).
#[tokio::test(start_paused = true)]
async fn forking_ring_loser_late_200_is_acked_and_byed() {
    let h = Harness::new("actor-forking-loser-200").describe(
        "C1(a): bob answers 200 under winner f2 THEN emits a late 200 under \
         loser f1; alice ACKs+BYEs the loser fork (bob's leg survives), then \
         tears down the winning dialog",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let alice_task = {
        let alice = alice.clone();
        let bob = bob.clone();
        tokio::spawn(async move {
            let mut call = alice.invite(&bob).with_sdp(OFFER_SDP).send().await;
            call.expect(180).await;
            call.expect(180).await;
            // The winner's 200 (f2): ACK, keep the confirmed dialog.
            let ok = call.expect(200).await;
            assert_eq!(ok.to().tag(), Some("f2"));
            let mut winner = call.ack().await;
            // The loser's LATE 200 (f1): §13.2.2.4 — ACK it on its own fork
            // dialog, then BYE that fork. (`expect(200)` re-points the
            // ClientInvite's dialog at the latest 2xx's tag, so `ack()` here
            // addresses the LOSER fork.)
            let late = call.expect(200).await;
            assert_eq!(late.to().tag(), Some("f1"), "the late 200 is the loser's");
            let mut loser = call.ack().await;
            let mut loser_bye = loser.bye().await;
            loser_bye.expect(200).await;
            // The winning dialog is unaffected — tear it down normally.
            let mut bye = winner.bye().await;
            bye.expect(200).await;
        })
    };

    let call = forking_bob_plan(
        &bob,
        Disposition::ForkingRing {
            tags: &["f1", "f2"],
            winner: "f2",
            ring: Duration::from_millis(200),
            reliable: false,
            loser_late_200: Some("f1"),
        },
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(
        verdict.is_ok(),
        "the loser-late-200 call must settle OK (bob's leg must survive the \
         loser-fork BYE), got {verdict:?}",
    );

    alice_task.await.unwrap();
    h.finish().await;
}

/// C1(a) reliable variant: each fork's 18x is a reliable 183 (Require:100rel,
/// RSeq:1, SDP) and the 200 waits for the WINNER fork's PRACK (MUST-014). A
/// losing fork's PRACK is 200'd but does NOT release the answer.
#[tokio::test(start_paused = true)]
async fn forking_ring_reliable_answers_on_winner_prack_only() {
    use sip_message::generators::InDialogMethod;

    let h = Harness::new("actor-forking-reliable").describe(
        "C1(a): bob (ForkingRing reliable) emits reliable 183(f1)+183(f2); \
         alice PRACKs each fork on its own early dialog; only the winner \
         (f2) fork's PRACK releases the 200",
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
            let p1 = call.expect(183).await;
            assert_eq!(p1.to().tag(), Some("f1"));
            let p2 = call.expect(183).await;
            assert_eq!(p2.to().tag(), Some("f2"));
            // PRACK the LOSER fork first — the answer must NOT be released.
            let mut prack1 = call
                .send_request(InDialogMethod::Prack)
                .with_to_tag("f1")
                .with_rack("1 1 INVITE")
                .send()
                .await;
            prack1.expect(200).await;
            // PRACK the WINNER fork — this releases the 200 (under f2).
            let mut prack2 = call
                .send_request(InDialogMethod::Prack)
                .with_to_tag("f2")
                .with_rack("1 1 INVITE")
                .send()
                .await;
            prack2.expect(200).await;
            let ok = call.expect(200).await;
            assert_eq!(ok.to().tag(), Some("f2"), "answered under the winner tag");
            let mut dialog = call.ack().await;
            let mut bye = dialog.bye().await;
            bye.expect(200).await;
        })
    };

    let call = forking_bob_plan(
        &bob,
        Disposition::ForkingRing {
            tags: &["f1", "f2"],
            winner: "f2",
            ring: Duration::ZERO,
            reliable: true,
            loser_late_200: None,
        },
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the reliable forked call must settle OK, got {verdict:?}");

    alice_task.await.unwrap();
    h.finish().await;
}

/// A two-actor plan pairing the ACTOR caller with a forking callee — the
/// C1(b) rig: alice is the reactive actor (the fork dance on the caller
/// side is the subject), bob the C1(a) `ForkingRing` UAS.
fn forked_pair_plan(
    alice: &crate::Agent,
    bob: &crate::Agent,
    disposition: Disposition,
    plan: Option<crate::realcall::InvitePlan>,
) -> CallPlan {
    CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan }),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),

                cseq: None,
                delayed: vec![],
                claim: None,
            },
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition,
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),

                cseq: None,
                delayed: vec![],
                claim: None,
            },
        ],
        plan: vec![phase("established", |s| {
            s.leg_at_least("alice", LegPhase::Confirmed)
                && s.leg_at_least("bob", LegPhase::Confirmed)
        })],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

/// C1(b): the ACTOR caller absorbs a forked establishment — two distinct-tag
/// 180s (two early dialogs, §12.1.2), the 2xx's tag picks the winner
/// (§13.2.2.4), the confirmed dialog rides it, and the teardown BYE
/// addresses the winning fork. Verdict Ok + the RFC hard gate.
#[tokio::test(start_paused = true)]
async fn actor_caller_confirms_forked_winner() {
    let h = Harness::new("actor-caller-forked-winner").describe(
        "C1(b): the actor caller sees 180(f1)+180(f2) then 200(f2); the 2xx \
         tag is the winner — the call confirms, tears down and settles OK",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = forked_pair_plan(
        &alice,
        &bob,
        Disposition::ForkingRing {
            tags: &["f1", "f2"],
            winner: "f2",
            ring: Duration::from_millis(200),
            reliable: false,
            loser_late_200: None,
        },
        None,
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the forked call must settle OK, got {verdict:?}");
    h.finish().await;
}

/// C1(b) loser-late-200: the actor caller confirms the winner (f2), then the
/// LOSING fork's late 200 (f1) arrives — the reactor ACKs it on the loser's
/// OWN fork dialog and BYEs that fork (§13.2.2.4), the fork BYE's 200
/// (recognised by its tag mismatch) closes the `ForkBye` obligation WITHOUT
/// terminating alice's leg, and the winning dialog tears down normally.
/// The fork-aware `unacked-2xx-not-cleared` audit rule gates the wire at
/// `finish()` — an unACKed or unBYEd loser 200 would fail there.
#[tokio::test(start_paused = true)]
async fn actor_caller_acks_and_byes_losing_fork_late_200() {
    let h = Harness::new("actor-caller-loser-late-200").describe(
        "C1(b): a losing fork's late 200 (f1) after the winner's (f2) is \
         ACKed on its own fork dialog then BYE'd; the winning dialog and \
         the verdict are unaffected",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = forked_pair_plan(
        &alice,
        &bob,
        Disposition::ForkingRing {
            tags: &["f1", "f2"],
            winner: "f2",
            ring: Duration::from_millis(200),
            reliable: false,
            loser_late_200: Some("f1"),
        },
        None,
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(
        verdict.is_ok(),
        "the loser-late-200 call must settle OK (fork ACK+BYE + ForkBye close), got {verdict:?}",
    );
    h.finish().await;
}

/// C1(b) reliable forks: each fork's reliable 183 (distinct tag, RSeq:1) is
/// PRACKed on its OWN early dialog — the `(to_tag, rseq)` dedup re-key: an
/// RSeq-only dedup would swallow the second fork's PRACK (both forks start
/// at RSeq 1) and the callee (which answers only on the WINNER's PRACK)
/// would never answer; the `unacked-reliable-provisional` audit rule would also flag
/// the unPRACKed fork at `finish()`.
#[tokio::test(start_paused = true)]
async fn actor_caller_pracks_each_reliable_fork() {
    let h = Harness::new("actor-caller-forked-prack").describe(
        "C1(b): reliable 183(f1)+183(f2), both RSeq:1 — the actor caller \
         PRACKs EACH fork on its own early dialog ((tag,rseq) dedup); the \
         winner fork's PRACK releases the 200 and the call completes",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // The caller must advertise 100rel for the UAS's reliable 183s to be
    // legal (RFC 3262 §3) — a hand-built direct-to-bob plan carries it.
    let plan = crate::realcall::InvitePlan {
        via: bob.addr(),
        from: None,
        to: None,
        ruri: None,
        headers: vec![("Supported".to_string(), "100rel".to_string())],
        rewrite: Default::default(),
    };

    let call = forked_pair_plan(
        &alice,
        &bob,
        Disposition::ForkingRing {
            tags: &["f1", "f2"],
            winner: "f2",
            ring: Duration::ZERO,
            reliable: true,
            loser_late_200: None,
        },
        Some(plan),
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the reliable forked call must settle OK, got {verdict:?}");
    h.finish().await;
}
