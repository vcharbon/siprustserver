use std::time::Duration;

use crate::actor::*;
use super::testkit::*;
use crate::{Harness, ANSWER_SDP};

/// The reject plan for the response-side confrontation: alice's script
/// strictly expects a 486 final; bob rejects with 603.
fn reject_drift_plan(
    alice: &crate::Agent,
    bob: &crate::Agent,
    delta_policy: Option<AcceptedDeltaPolicy>,
) -> CallPlan {
    CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectResponse {
                            status: 486,
                            cseq_method: None,
                            body: BodyExpect::Any,
                            early: None,
                            ack_body: None,
                            matcher: None,
                        },
                    ),
                ],
            ),
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition: Disposition::Reject(603),
                media: MediaState::answer(ANSWER_SDP),
                goals: vec![],
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
        delta_policy,
    }
}

/// Response-side accepted delta: the due `ExpectResponse{486}` meets
/// a 603 — the policy blesses the status drift (`DeltaReaction::Default`:
/// the reactive follow-up already ACKed the observed final) and the run
/// completes with the acceptance recorded.
#[tokio::test(start_paused = true)]
async fn accepted_delta_response_status_drift_recorded() {
    let h = Harness::new("actor-delta-response-drift").describe(
        "response side: ExpectResponse{486} confronted with a 603 — the \
         policy accepts, the fact is consumed, AcceptedDelta recorded",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let drift: AcceptedDeltaPolicy = Arc::new(|ctx: &DeltaContext<'_>| {
        if matches!(
            ctx.expected,
            ExpectedStimulus::Response { status: 486 }
        ) && matches!(ctx.observed, ObservedStimulus::Response { status: 603, .. })
        {
            DeltaDecision::Accepted(AcceptedDelta {
                rule: "reject-code-drift",
                satisfies_steps: 1,
                reaction: DeltaReaction::Default,
            })
        } else {
            DeltaDecision::NotAccepted
        }
    });
    let call = reject_drift_plan(&alice, &bob, Some(drift));
    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the accepted status drift must complete, got {verdict:?}");
    assert!(
        obs.replay_record().contains(&ReplayEntry::AcceptedDelta {
            leg: "alice",
            step: 1,
            expected: "486".to_string(),
            observed: "603".to_string(),
            rule: "reject-code-drift",
        }),
        "the response acceptance is never silent: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}

/// Response-side `NotAccepted`: the declined status drift fails
/// exactly as today — the pinned `WrongStatus{expected: 486, got: 603}`
/// fail-fast, and no acceptance is recorded.
#[tokio::test(start_paused = true)]
async fn not_accepted_response_delta_pins_wrong_status() {
    let h = Harness::new("actor-delta-response-declined").describe(
        "response-side decline: ExpectResponse{486} vs 603 keeps the \
         fail-fast WrongStatus verdict",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let decline: AcceptedDeltaPolicy = Arc::new(|_| DeltaDecision::NotAccepted);
    let call = reject_drift_plan(&alice, &bob, Some(decline));
    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    match &verdict {
        CallVerdict::Failed(StepError::WrongStatus { who, expected, got, reason }) => {
            assert_eq!(who, "alice");
            assert_eq!(*expected, 486);
            assert_eq!(*got, 603);
            assert_eq!(reason, "Decline");
        }
        other => panic!("expected the pinned WrongStatus fail-fast, got {other:?}"),
    }
    assert!(
        obs.accepted_deltas().is_empty(),
        "a declined substitution must record no acceptance: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}

/// A `satisfies_steps` overrun (a policy typo: 20 with one goal left) is
/// bounded fail-fast — the shared check both hook points run — never a
/// silent script exhaustion, and no acceptance is recorded. Pinned on the
/// response-side hook, where the wire is already complete when the policy
/// bug trips (the 603 was ACKed), so the flow stays terminal.
#[tokio::test(start_paused = true)]
async fn satisfies_steps_overrun_fails_fast_bounded() {
    let h = Harness::new("actor-delta-overrun").describe(
        "ADR-0024 §6: satisfies_steps beyond the remaining goals is a \
         bounded StepError, never a silent pass",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let typo: AcceptedDeltaPolicy = Arc::new(|_| {
        DeltaDecision::Accepted(AcceptedDelta {
            rule: "overrun-typo",
            satisfies_steps: 20,
            reaction: DeltaReaction::Default,
        })
    });
    let call = reject_drift_plan(&alice, &bob, Some(typo));
    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    match &verdict {
        CallVerdict::Failed(StepError::UnexpectedKind { who, detail }) => {
            assert_eq!(who, "alice");
            assert!(
                detail.contains("must satisfy 1..=1") && detail.contains("got 20"),
                "the bounded error names the valid range and the typo: {detail}",
            );
        }
        other => panic!("expected the bounded overrun StepError, got {other:?}"),
    }
    assert!(
        obs.accepted_deltas().is_empty(),
        "an out-of-bounds acceptance must record nothing: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}

/// A response substitution declaring `TerminatePendingInitial` is a
/// bounded fail-fast (its RFC follow-up already keyed on the observed
/// status — there is nothing to terminate), pinning the reaction
/// validation.
#[tokio::test(start_paused = true)]
async fn response_delta_rejects_terminate_reaction() {
    let h = Harness::new("actor-delta-response-bad-reaction").describe(
        "ADR-0024 §6: a response substitution must take DeltaReaction::\
         Default — TerminatePendingInitial fails fast",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let bad: AcceptedDeltaPolicy = Arc::new(|_| {
        DeltaDecision::Accepted(AcceptedDelta {
            rule: "bad-reaction",
            satisfies_steps: 1,
            reaction: DeltaReaction::TerminatePendingInitial,
        })
    });
    let call = reject_drift_plan(&alice, &bob, Some(bad));
    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    match &verdict {
        CallVerdict::Failed(StepError::UnexpectedKind { who, detail }) => {
            assert_eq!(who, "alice");
            assert!(
                detail.contains("DeltaReaction::Default"),
                "the bounded error names the required reaction: {detail}",
            );
        }
        other => panic!("expected the bounded reaction StepError, got {other:?}"),
    }
    h.finish().await;
}
