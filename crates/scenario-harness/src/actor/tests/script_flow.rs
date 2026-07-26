use sip_message::{EmitOpts, MessageTemplate, Method, TemplateHeader};
use std::time::Duration;

use crate::actor::*;
use super::testkit::*;
use crate::{Harness, OFFER_SDP};

/// Requeue-on-advance: two INFOs park while one `ExpectRequest{Info}`
/// remains; consuming the first advances the cursor, and the second — now
/// matching no remaining goal — is auto-reacted (200) as a recorded stray
/// instead of starving behind the script.
#[tokio::test(start_paused = true)]
async fn requeue_on_advance_auto_reacts_passed_parked_request() {
    use sip_message::generators::InDialogMethod;

    let h = Harness::new("actor-requeue-on-advance").describe(
        "two parked INFOs, one ExpectRequest{Info}: the consume advances \
         the cursor and the second INFO is auto-reacted as a stray",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let info = |body: &[u8]| GoalStep::InDialog {
        method: InDialogMethod::Info,
        content_type: Some("application/x-cap-test".to_string()),
        body: Some(body.to_vec()),
        headers: vec![],
    };
    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), info(b"one")),
                    Goal::new(Barrier::None, info(b"two")),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye)
                        .after(Duration::from_millis(400)),
                ],
            ),
            scripted_spec(
                "bob",
                &bob,
                vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::Initial,
                            body: BodyExpect::SdpPresent,
                            matcher: None,
                        },
                    ),
                    Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    // Dwell long enough for BOTH INFOs to park before the
                    // first is consumed — the requeue setup.
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::InDialog(InDialogMethod::Info),
                            body: BodyExpect::Present,
                            matcher: None,
                        },
                    )
                    .after(Duration::from_millis(100)),
                    Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                ],
            ),
        ],
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
    };

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict =
        run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "both INFOs must be answered, got {verdict:?}");
    let replay = obs.replay_record();
    assert!(
        replay.iter().any(|e| matches!(
            e,
            ReplayEntry::ServicedStray { leg: "bob", method, action }
                if method == "INFO" && action.contains("advance")
        )),
        "the requeued INFO is a recorded stray: {replay:?}",
    );
    h.finish().await;
}

/// ExpectResponse provisional strictness: expecting a 183 but the FINAL
/// (a 486) arrives first — fail-fast with the goal's own expectation.
#[tokio::test(start_paused = true)]
async fn expect_response_fails_fast_when_final_precedes_provisional() {
    let h = Harness::new("actor-expect-provisional-strict").describe(
        "ExpectResponse{183} sees the 486 final first: fail-fast \
         WrongStatus{expected 183, got 486}, owned by the goal",
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
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectResponse {
                            status: 183,
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
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    match verdict {
        CallVerdict::Failed(StepError::WrongStatus { who, expected, got, .. }) => {
            assert_eq!(
                (who.as_str(), expected, got),
                ("alice", 183, 486),
                "the goal's expectation (183), never the shed's (180)",
            );
        }
        other => panic!("expected the strict provisional WrongStatus, got {other:?}"),
    }
    h.finish().await;
}

/// Forked RespondTemplate early ids: two distinct fork ids emit
/// distinct-tag 180s on the ONE parked INVITE transaction; the final's id
/// names the winner (its tag becomes the dialog tag) and the loser fork
/// settles with no final — the caller's reception goals bind per fork.
#[tokio::test(start_paused = true)]
async fn forked_respond_template_early_ids_name_winner() {
    let h = Harness::new("actor-scripted-forked-early-ids").describe(
        "RespondTemplate early f1/f2 → 180(f1)+180(f2) on one txn, 200 \
         under winner f2; alice's ExpectResponse binds each fork by id",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let expect_180 = |id: EarlyId| GoalStep::ExpectResponse {
        status: 180,
        body: BodyExpect::Any,
        early: Some(id),
        ack_body: None,
        matcher: None,
    };
    let respond_180 = |id: EarlyId| GoalStep::RespondTemplate {
        template: response_template(180, "Ringing", false),
        opts: EmitOpts::default(),
        early: Some(id),
    };
    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::None, expect_180("f1")),
                    Goal::new(Barrier::None, expect_180("f2")),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                ],
            ),
            scripted_spec(
                "bob",
                &bob,
                vec![
                    Goal::new(Barrier::None, respond_180("f1")),
                    Goal::new(Barrier::None, respond_180("f2")),
                    Goal::new(
                        Barrier::None,
                        GoalStep::RespondTemplate {
                            template: response_template(200, "OK", true),
                            opts: EmitOpts::default(),
                            early: Some("f2"),
                        },
                    ),
                ],
            ),
        ],
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the forked scripted answer must settle OK, got {verdict:?}");
    h.finish().await;
}

/// Scripted originator attribution: the caller is the actor whose FIRST
/// goal originates the dialog (`InviteTemplate`), not `Disposition::Caller`
/// and not the `"alice"` fallback — an `Expect::Reject` terminal carries
/// that actor's role.
#[tokio::test]
async fn scripted_originator_attribution_keys_on_first_goal() {
    let h = Harness::new("actor-scripted-attribution").describe(
        "originating_role keys on the first Invite/InviteTemplate goal: a \
         Scripted carol originator is the attributed caller, not alice",
    );
    let carol = h.agent("carol", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;

    let actors = vec![
        ActorSpec {
            role: "carol",
            agent: carol.clone(),
            disposition: Disposition::Scripted,
            media: MediaState::none(),
            goals: vec![Goal::new(
                Barrier::None,
                GoalStep::InviteTemplate {
                    callee: "bob",
                    plan: None,
                    template: invite_template(&[]),
                    opts: EmitOpts::default(),
                },
            )],
            invite_targets: vec![("bob", bob.clone())],
            via: None,
            feed: CtxFeed::default(),
        
            cseq: None,
            delayed: None,
            claim: None,
        },
        scripted_spec("bob", &bob, vec![]),
    ];
    assert_eq!(originating_role(&actors), "carol");

    // The Reject terminal is attributed to carol, never "alice".
    let obs = ObservedState::new();
    obs.record(
        Observation::LegFinal { leg: "carol", status: 486, reason: "Busy Here".into() },
        tokio::time::Instant::now(),
    );
    match into_result(Expect::Reject(486), CallVerdict::Ok, &obs, originating_role(&actors)) {
        Err(StepError::WrongStatus { who, expected, got, .. }) => {
            assert_eq!((who.as_str(), expected, got), ("carol", 200, 486));
        }
        other => panic!("expected the Reject terminal under carol, got {other:?}"),
    }
    h.finish().await;
}

/// One `answer_100_trying` run: alice invites, Scripted bob answers by
/// policy, BYE teardown — returns whether alice observed a `100`.
async fn run_100_trying_case(name: &'static str, on: bool) -> bool {
    let h = Harness::new(name).describe(
        "Automatics{answer_100_trying}: the parked INVITE draws (or not) an \
         immediate 100 Trying; the call settles clean either way",
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
        automatics: Automatics { answer_100_trying: on },
        delta_policy: None,
    };

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict =
        run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "[{name}] must settle clean, got {verdict:?}");
    h.finish().await;
    obs.with_snapshot(|s| s.leg("alice").saw_status(100))
}

/// `Automatics{answer_100_trying}`: ON emits the immediate 100 on the
/// parked INVITE; OFF does not — both settle clean on the fake lane.
#[tokio::test(start_paused = true)]
async fn answer_100_trying_automatic_toggles_emission() {
    assert!(
        run_100_trying_case("actor-100-trying-on", true).await,
        "with the automatic ON, alice observes the 100",
    );
    assert!(
        !run_100_trying_case("actor-100-trying-off", false).await,
        "with the automatic OFF, no 100 is emitted",
    );
}

/// One initial-INVITE matcher run: alice's `InviteTemplate` carries a
/// frozen `X-Cap: v1`; bob's `ExpectRequest{Initial}` matcher pins
/// `X-Cap: expect_value` plus CAPTURE-time tier-1 rows (Call-ID/Via/CSeq —
/// regenerated live, never value-compared). Returns the verdict.
async fn run_initial_matcher_case(name: &'static str, expect_value: &str) -> CallVerdict {
    let h = Harness::new(name).describe(
        "ExpectRequest{matcher} on the initial INVITE: frozen X-Cap \
         compared; captured tier-1 values are structural-only",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let matcher = MessageTemplate::request(
        Method::Invite,
        vec![
            TemplateHeader::classified("Via", "SIP/2.0/UDP 9.9.9.9:9;branch=z9hG4bK-cap"),
            TemplateHeader::classified("Call-ID", "captured@9.9.9.9"),
            TemplateHeader::classified("CSeq", "7 INVITE"),
            TemplateHeader::frozen("X-Cap", expect_value),
            TemplateHeader::frozen("Content-Type", "application/sdp"),
        ],
        OFFER_SDP.as_bytes().to_vec(),
    );
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
                            template: invite_template(&[("X-Cap", "v1")]),
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
                        GoalStep::ExpectRequest {
                            kind: RequestKind::Initial,
                            body: BodyExpect::SdpPresent,
                            matcher: Some(matcher),
                        },
                    ),
                    Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                ],
            ),
        ],
        plan: vec![established_phase()],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    if verdict.is_ok() {
        h.finish().await;
    }
    verdict
}

/// Content matcher POSITIVE, paired with the sibling negative on the SAME
/// goal shape: the identical configuration failing on a drifted value
/// proves the matcher RUNS here, so the passing case demonstrably held
/// (a silently-skipped matcher would make the sibling pass too).
#[tokio::test(start_paused = true)]
async fn content_matcher_holds_on_frozen_header() {
    let ok = run_initial_matcher_case("actor-content-matcher-holds", "v1").await;
    assert!(ok.is_ok(), "the matching value must hold, got {ok:?}");

    let bad = run_initial_matcher_case("actor-content-matcher-holds-neg", "v2").await;
    match bad {
        CallVerdict::Failed(StepError::UnexpectedKind { who, detail }) => {
            assert_eq!(who, "bob");
            assert!(
                detail.contains("x-cap") && detail.contains("v2") && detail.contains("v1"),
                "the same shape fails on a drifted value — the matcher runs: {detail}",
            );
        }
        other => panic!("the sibling negative must fail the matcher, got {other:?}"),
    }
}
