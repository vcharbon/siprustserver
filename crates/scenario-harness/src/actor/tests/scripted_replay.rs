use sip_message::EmitOpts;
use std::time::Duration;

use crate::actor::*;
use super::testkit::*;
use crate::{Harness, ANSWER_SDP};

/// Scripted end-to-end template replay: a templated INVITE, a scripted 180
/// (provisional, binding NOT consumed) then 200 via `RespondTemplate` on
/// the ONE parked server transaction, the reactor's auto-ACK, and a BYE
/// teardown — verdict Ok, settle clean, RFC hard gate green.
#[tokio::test(start_paused = true)]
async fn scripted_template_replay_end_to_end() {
    let h = Harness::new("actor-scripted-template-replay").describe(
        "InviteTemplate → scripted RespondTemplate 180 (non-consuming) + 200 \
         on the parked INVITE txn → auto-ACK → BYE; settles clean",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::InviteTemplate {
                            callee: "bob",
                            plan: None,
                            template: invite_template(&[("X-Replay", "cap-1")]),
                            opts: EmitOpts::default(),
                        },
                    ),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                ],
            ),
            scripted_spec(
                "bob",
                &bob,
                vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::RespondTemplate {
                            template: response_template(180, "Ringing", false),
                            opts: EmitOpts::default(),
                            early: None,
                        },
                    ),
                    Goal::new(
                        Barrier::None,
                        GoalStep::RespondTemplate {
                            template: response_template(200, "OK", true),
                            opts: EmitOpts::default(),
                            early: None,
                        },
                    ),
                ],
            ),
        ],
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the scripted template replay must settle OK, got {verdict:?}");
    h.finish().await;
}

/// ObserveFinal divergence: the plan expected a 400 but the peer answers
/// 200 — the verdict stays Ok, the replay record disagrees, and the
/// follow-up ACK rides the OBSERVED 2xx (the RFC gate proves the ACK).
#[tokio::test(start_paused = true)]
async fn observe_final_records_divergence_and_acks_observed_2xx() {
    let h = Harness::new("actor-observe-final-divergence").describe(
        "ObserveFinal{expected 400} observes a 200: verdict Ok, \
         RecordedFinal disagrees, the ACK follows the observed 2xx",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::None, GoalStep::ObserveFinal { key: 7, expected: Some(400) }),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                ],
            ),
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
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
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict =
        run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "divergence is data, never a failure — got {verdict:?}");
    let replay = obs.replay_record();
    assert!(
        replay.contains(&ReplayEntry::Final(RecordedFinal {
            key: 7,
            expected: Some(400),
            observed: 200,
        })),
        "the replay record carries the disagreement: {replay:?}",
    );
    h.finish().await;
}

/// Truncated flow: scripted goals up to the anchor, `ExpectFinal` with a
/// class assert, then policy `Respond` + BYE completion — the whole
/// truncated-variant lowering pattern on the engine's verbs.
#[tokio::test(start_paused = true)]
async fn truncated_flow_completes_after_class_assert() {
    let h = Harness::new("actor-truncated-completes").describe(
        "scripted Respond 180+200, ExpectFinal{Class(2)} at the anchor, \
         then standard BYE completion — full post-call verification",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::None, GoalStep::ExpectFinal { assert: FinalAssert::Class(2) }),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                ],
            ),
            scripted_spec(
                "bob",
                &bob,
                vec![
                    Goal::new(Barrier::None, GoalStep::Respond { status: 180 }),
                    Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                ],
            ),
        ],
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the truncated completion must settle OK, got {verdict:?}");
    h.finish().await;
}

/// The truncated anchor's NEGATIVE: the fixed final never arrives (a 486
/// where a 2xx-class was asserted) — the goal fails fast with the assert's
/// own expectation (`expected: 200`), not the reactor's incidental shed
/// (`expected: 180`), and never by barrier timeout.
#[tokio::test(start_paused = true)]
async fn truncated_class_assert_fails_fast_on_wrong_class() {
    let h = Harness::new("actor-truncated-assert-fails").describe(
        "ExpectFinal{Class(2)} observes a scripted 486: fail-fast \
         WrongStatus{expected 200, got 486} owned by the goal",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::None, GoalStep::ExpectFinal { assert: FinalAssert::Class(2) }),
                ],
            ),
            scripted_spec(
                "bob",
                &bob,
                vec![Goal::new(Barrier::None, GoalStep::Respond { status: 486 })],
            ),
        ],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    match verdict {
        CallVerdict::Failed(StepError::WrongStatus { who, expected, got, .. }) => {
            assert_eq!(
                (who.as_str(), expected, got),
                ("alice", 200, 486),
                "the assert (not the shed) owns the failure",
            );
        }
        other => panic!("expected the class-assert WrongStatus, got {other:?}"),
    }
    h.finish().await;
}

/// Incidental-failure suppression: a non-2xx final on the establishing
/// INVITE with a RECEPTION goal next is the goal's to judge — verdict Ok,
/// never the reactor's `WrongStatus{expected: 180}` shed.
#[tokio::test(start_paused = true)]
async fn reception_goal_suppresses_incidental_shed() {
    let h = Harness::new("actor-reception-suppresses-shed").describe(
        "bob rejects 486 while alice's next goal is ObserveFinal: the goal \
         owns the final (recorded), no incidental WrongStatus shed",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::None, GoalStep::ObserveFinal { key: 1, expected: Some(486) }),
                ],
            ),
            ActorSpec {
                role: "bob",
                agent: bob.clone(),
                disposition: Disposition::Reject(486),
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
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict =
        run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the reception goal owns the 486, got {verdict:?}");
    assert!(
        obs.replay_record().contains(&ReplayEntry::Final(RecordedFinal {
            key: 1,
            expected: Some(486),
            observed: 486,
        })),
        "the observed final is recorded: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}
