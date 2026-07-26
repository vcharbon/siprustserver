use std::time::Duration;

use crate::actor::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// A CANCEL×200 crossing plan (C2/E5): alice INVITEs, CANCELs after
/// `cancel_after` (gated on ringing), and BYEs-if-confirmed once the race
/// resolves; bob rings then answers after `ring`. Varying `cancel_after`
/// vs `ring` by one transit quantum pins each branch deterministically.
fn cancel_crossing_plan(
    alice: &crate::Agent,
    bob: &crate::Agent,
    ring: Duration,
    cancel_after: Duration,
) -> CallPlan {
    let ringing = |s: &StateInner| s.leg_at_least("alice", LegPhase::Early);
    CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::pred("ringing", ringing), GoalStep::Cancel)
                        .after(cancel_after),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition: Disposition::RingThenAnswer { ring },
                media: MediaState::answer(ANSWER_SDP),
                // The teardown rides BOB (branch-conditional): the guard
                // holds once bob has RESOLVED (Confirmed, or the monotone
                // Terminated after a 487); ByeIfConfirmed BYEs the winning
                // dialog or no-ops on the cancelled one. Alice keeps only
                // [Invite, Cancel] so a 487 never trips her incidental-
                // failure WrongStatus path (it stays a clean terminal that
                // the EitherOf oracle maps).
                goals: vec![Goal::new(
                    Barrier::pred("bob_resolved", |s| s.leg_at_least("bob", LegPhase::Confirmed)),
                    GoalStep::ByeIfConfirmed,
                )],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
        ],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
    }
}

/// C2/E5 branch — CANCEL WINS: alice CANCELs well before bob's answer timer
/// fires (cancel_after < ring − 2·transit), so bob 487s the held INVITE. The
/// call reaches a clean terminal (both legs terminated, alice ACKs the 487)
/// and the EitherOf oracle maps it to the abandoned `Timeout`.
#[tokio::test(start_paused = true)]
async fn cancel_answer_crossing_cancel_wins() {
    let h = Harness::new("actor-cancel-wins").describe(
        "C2/E5: alice CANCELs at 2ms, bob answers at 20ms — the CANCEL wins, \
         bob 487s the INVITE; the branch oracle maps it to the abandoned \
         terminal",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = cancel_crossing_plan(
        &alice,
        &bob,
        Duration::from_millis(20),
        Duration::from_millis(2),
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the CANCEL-wins call must reach a clean terminal, got {verdict:?}");
    // The observed 487 → the abandoned branch: pinned in the oracle unit
    // test `either_of_oracle_maps_each_branch`.
    h.finish().await;
}

/// C2/E5 branch — 200 WINS (the true crossing): alice CANCELs just AFTER
/// bob's answer timer fires (cancel_after > ring), so bob has already sent
/// the 200 when the CANCEL arrives. Per §9.2 bob 200s the CANCEL and ignores
/// it (the confirmed dialog survives); alice ACKs the crossed 200, bob BYEs
/// the winning dialog, and the call tears down OK.
#[tokio::test(start_paused = true)]
async fn cancel_answer_crossing_answer_wins() {
    let h = Harness::new("actor-answer-wins").describe(
        "C2/E5: alice CANCELs at 20ms, bob answers at 5ms — the 200 crossed \
         the CANCEL; bob ignores the late CANCEL (§9.2), alice ACKs the 200, \
         the winning dialog tears down OK",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = cancel_crossing_plan(
        &alice,
        &bob,
        Duration::from_millis(5),
        Duration::from_millis(20),
    );
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the 200-wins call must confirm and tear down OK, got {verdict:?}");

    h.finish().await;
}

/// No-answer machinery: a `RingThenSilent` callee emits its 180 then holds the
/// INVITE server txn with NO answer ever scheduled — the peer's CANCEL (in
/// production, the SUT's no-answer timer firing) releases the held txn as a
/// `487`, the leg terminates, and the reject-final obligation closes on the
/// hop-ACK: a clean settle, no leaked server txn. SUT-less pin of the
/// silent primary a no-answer-triggered failover composes with.
#[tokio::test(start_paused = true)]
async fn ring_then_silent_487s_on_cancel_and_settles() {
    let h = Harness::new("actor-ring-then-silent").describe(
        "bob rings then stays silent (held INVITE txn, no answer ever); \
         alice CANCELs — standing in for the SUT's no-answer timer — bob \
         487s the held txn and both legs settle cleanly",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let ringing = |s: &StateInner| s.leg_at_least("alice", LegPhase::Early);
    let call = CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    // The stand-in for the SUT's no-answer timer: CANCEL a
                    // while after the ring (bob would ring forever).
                    Goal::new(Barrier::pred("ringing", ringing), GoalStep::Cancel)
                        .after(Duration::from_millis(500)),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition: Disposition::RingThenSilent,
                media: MediaState::none(),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
                claim: None,
            },
        ],
        plan: vec![phase("ringing", ringing)],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(
        verdict.is_ok(),
        "the CANCELled silent ring must settle cleanly (487 + hop-ACK), got {verdict:?}"
    );
    h.finish().await;
}

/// The `into_result` branch oracle maps each observed race outcome to its
/// bounded downstream class: a caller that saw the 487 → the abandoned
/// `Timeout` (the `timeout` load class); a caller that confirmed (no non-2xx
/// final) → `Ok`.
#[tokio::test]
async fn either_of_oracle_maps_each_branch() {
    use crate::actor::ExpectBranch;
    let branches: &[ExpectBranch] =
        &[ExpectBranch::Answered, ExpectBranch::Cancelled { code: 487 }];

    // Cancel-wins: alice saw a 487 final.
    let obs = ObservedState::new();
    let now = tokio::time::Instant::now();
    obs.record(Observation::LegEarly { leg: "alice" }, now);
    obs.record(
        Observation::LegFinal { leg: "alice", status: 487, reason: "Request Terminated".into() },
        now,
    );
    obs.record(Observation::LegTerminated { leg: "alice" }, now);
    match into_result(Expect::EitherOf(branches), CallVerdict::Ok, &obs, "alice") {
        Err(StepError::Timeout { who }) => {
            assert_eq!(who, "alice-abandoned-after-ringing")
        }
        other => panic!("cancel-wins must map to the abandoned Timeout, got {other:?}"),
    }

    // Answer-wins: alice confirmed, no non-2xx final.
    let obs = ObservedState::new();
    obs.record(Observation::LegConfirmed { leg: "alice" }, now);
    obs.record(Observation::LegTerminated { leg: "alice" }, now);
    assert!(
        into_result(Expect::EitherOf(branches), CallVerdict::Ok, &obs, "alice").is_ok(),
        "answer-wins (no 487) must map to Ok",
    );
}
