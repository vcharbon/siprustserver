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
                    Goal::new(
                        Barrier::pred("ringing", ringing),
                        GoalStep::Cancel { stated: Vec::new() },
                    )
                    .after(cancel_after),
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
                disposition: Disposition::RingThenAnswer { ring },
                media: MediaState::answer(ANSWER_SDP),
                // The teardown rides BOB (branch-conditional): the guard
                // holds once bob has RESOLVED (Confirmed, or the monotone
                // Terminated after a 487); ByeIfConfirmed BYEs the winning
                // dialog or no-ops on the cancelled one. Alice keeps only
                // [Invite, Cancel] so a 487 never trips her incidental-
                // failure WrongStatus path (it stays a clean terminal that
                // the race's accepted endings read).
                goals: vec![Goal::new(
                    Barrier::pred("bob_resolved", |s| s.leg_at_least("bob", LegPhase::Confirmed)),
                    GoalStep::ByeIfConfirmed,
                )],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),

                cseq: None,
                delayed: vec![],
                claim: None,
            },
        ],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

/// C2/E5 branch — CANCEL WINS: alice CANCELs well before bob's answer timer
/// fires (cancel_after < ring − 2·transit), so bob 487s the held INVITE. The
/// call reaches a clean terminal (both legs terminated, alice ACKs the 487)
/// and its accepted endings read it as `abandoned`.
#[tokio::test(start_paused = true)]
async fn cancel_answer_crossing_cancel_wins() {
    let h = Harness::new("actor-cancel-wins").describe(
        "C2/E5: alice CANCELs at 2ms, bob answers at 20ms — the CANCEL wins, \
         bob 487s the INVITE; the branch oracle maps it to the abandoned \
         terminal",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call =
        cancel_crossing_plan(&alice, &bob, Duration::from_millis(20), Duration::from_millis(2));
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the CANCEL-wins call must reach a clean terminal, got {verdict:?}");
    // The observed 487 → the `abandoned` branch: pinned by
    // `either_branch_of_the_race_is_its_accepted_ending`.
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

    let call =
        cancel_crossing_plan(&alice, &bob, Duration::from_millis(5), Duration::from_millis(20));
    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the 200-wins call must confirm and tear down OK, got {verdict:?}");

    h.finish().await;
}

/// A CANCEL delivered to the callee in the same instant its answer timer fires
/// (a 5 ms hop, the CANCEL sent exactly one ring after the INVITE): the callee
/// reacts to the CANCEL first, 200s it and 487s the INVITE, and never sends a
/// 2xx for the cancelled INVITE (RFC 3261 §9.2). Repeated, because task order
/// within one instant must not decide it. This pins the in-process ordering
/// only: over a real network a CANCEL not yet received when the timer fires
/// makes a genuine crossing, which the caller's `answered_despite_cancel`
/// branch handles.
#[tokio::test(start_paused = true)]
async fn a_cancel_landing_with_the_answer_timer_is_processed_first() {
    for i in 0..16 {
        let h = Harness::with_transit_delay(format!("actor-cancel-with-timer-{i}"), 5);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let ring = Duration::from_millis(20);
        let obs = ObservedState::new();
        let ctx = crate::realcall::CallCtx::new();
        let call = cancel_crossing_plan(&alice, &bob, ring, ring);
        let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
        assert!(verdict.is_ok(), "run {i}: {verdict:?}");
        assert!(
            obs.with_snapshot(|s| s.leg_ref("alice").is_some_and(|l| l.saw_final(487))),
            "run {i}: the CANCEL must win over the answer timer"
        );
        h.finish().await;
    }
}

/// The same race on a multi-threaded runtime and the real clock, where the
/// schedule varies from run to run: whichever side wins, the call settles,
/// and the callee never answers 2xx after a CANCEL it received (the RFC audit
/// at `finish` gates on it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "slow lane: real clock >= 1 s"]
async fn a_cancel_racing_the_answer_timer_across_threads_stays_rfc_clean() {
    for i in 0..6 {
        let h = Harness::with_transit_delay(format!("actor-cancel-timer-mt-{i}"), 2);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let ring = Duration::from_millis(10);
        let call = cancel_crossing_plan(&alice, &bob, ring, ring);
        let verdict = run_call(call, Duration::from_secs(5)).await;
        assert!(verdict.is_ok(), "run {i}: {verdict:?}");
        h.finish().await;
    }
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
                    Goal::new(
                        Barrier::pred("ringing", ringing),
                        GoalStep::Cancel { stated: Vec::new() },
                    )
                    .after(Duration::from_millis(500)),
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
                disposition: Disposition::RingThenSilent,
                media: MediaState::none(),
                goals: vec![],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),

                cseq: None,
                delayed: vec![],
                claim: None,
            },
        ],
        plan: vec![phase("ringing", ringing)],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(
        verdict.is_ok(),
        "the CANCELled silent ring must settle cleanly (487 + hop-ACK), got {verdict:?}"
    );
    h.finish().await;
}

/// The caller's leg after a crossing attempt, from its response log in
/// arrival order (`(status, CSeq method)`) and its establishing final, if any.
fn caller_saw(responses: &[(u16, &str)], final_status: Option<u16>) -> ObservedState {
    let obs = ObservedState::new();
    let now = tokio::time::Instant::now();
    obs.record(Observation::LegEarly { leg: "alice" }, now);
    for &(status, method) in responses {
        let fact = ResponseFact {
            status,
            reason: String::new(),
            cseq_method: method.into(),
            body_len: 0,
            body_is_sdp: false,
            early_tag: None,
            typed: None,
        };
        obs.record(Observation::LegResponse { leg: "alice", fact }, now);
    }
    if let Some(status) = final_status {
        obs.record(Observation::LegFinal { leg: "alice", status, reason: String::new() }, now);
    }
    obs.record(Observation::LegTerminated { leg: "alice" }, now);
    obs
}

/// The race's accepted endings read the observed outcome: a caller that saw
/// the 487 ended `abandoned`; a caller whose INVITE drew its 2xx before her
/// CANCEL drew its 200 (a true crossing) ended `answered_despite_cancel`.
#[tokio::test]
async fn either_branch_of_the_race_is_its_accepted_ending() {
    let expect = Expect::of(&[Branch::ANSWERED_DESPITE_CANCEL, Branch::ABANDONED]);
    let won = caller_saw(&[(180, "INVITE"), (200, "CANCEL"), (487, "INVITE")], Some(487));
    let v = verdict(expect, CallVerdict::Ok, &won, "alice");
    assert_eq!(v.branch(), Some("abandoned"), "cancel-wins, got {v:?}");
    let crossed = caller_saw(&[(180, "INVITE"), (200, "INVITE"), (200, "CANCEL")], None);
    let v = verdict(expect, CallVerdict::Ok, &crossed, "alice");
    assert_eq!(v.branch(), Some("answered_despite_cancel"), "answer-wins, got {v:?}");
}

/// The 2xx reached the caller before her CANCEL left: she sent none, was
/// answered, and hung up. That is the `answered_before_cancel` ending; the
/// same log with a CANCEL sent and never answered is no accepted ending.
#[tokio::test]
async fn an_answer_before_the_cancel_left_is_answered_before_cancel() {
    let expect = Expect::of(&[
        Branch::ANSWERED_DESPITE_CANCEL,
        Branch::ANSWERED_BEFORE_CANCEL,
        Branch::ABANDONED,
    ]);
    let answered = caller_saw(&[(180, "INVITE"), (200, "INVITE")], None);
    let v = verdict(expect, CallVerdict::Ok, &answered, "alice");
    assert_eq!(v.branch(), Some("answered_before_cancel"), "got {v:?}");
    let sent = caller_saw(&[(180, "INVITE"), (200, "INVITE")], None);
    sent.record(Observation::CancelSent { leg: "alice" }, tokio::time::Instant::now());
    let v = verdict(expect, CallVerdict::Ok, &sent, "alice");
    assert!(!v.is_success(), "a CANCEL that drew no final, got {v:?}");
}

/// A server that ignores the CANCEL is no crossing: the caller's CANCEL drew
/// no final at all, or drew its 200 and the INVITE still drew a 2xx after it
/// (RFC 3261 §9.2). Neither is an accepted ending.
#[tokio::test]
async fn a_server_ignoring_the_cancel_is_no_accepted_ending() {
    let expect = Expect::of(&[Branch::ANSWERED_DESPITE_CANCEL, Branch::ABANDONED]);
    let dropped = caller_saw(&[(180, "INVITE"), (200, "INVITE")], None);
    dropped.record(Observation::CancelSent { leg: "alice" }, tokio::time::Instant::now());
    let v = verdict(expect, CallVerdict::Ok, &dropped, "alice");
    assert!(!v.is_success(), "a CANCEL that drew no final, got {v:?}");
    let ignored = caller_saw(&[(180, "INVITE"), (200, "CANCEL"), (200, "INVITE")], None);
    let v = verdict(expect, CallVerdict::Ok, &ignored, "alice");
    assert!(!v.is_success(), "a 2xx after the CANCEL's 200, got {v:?}");
}
