use sip_message::{MessageTemplate, Method, TemplateHeader};
use std::time::Duration;

use crate::actor::*;
use super::testkit::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// A scripted 2xx answer to a RECEIVED re-INVITE (`ExpectRequest{InDialog
/// (Invite)}` + `Respond{200}`): opens the answered-awaiting-ACK
/// obligation — observed OPEN mid-call by a barrier probe — which the
/// peer's ACK then closes, so settle held exactly until the ACK.
#[tokio::test(start_paused = true)]
async fn scripted_reinvite_answer_holds_settle_until_ack() {
    use sip_message::generators::InDialogMethod;
    use std::sync::atomic::{AtomicBool, Ordering};

    let h = Harness::new("actor-scripted-reinvite-answer").describe(
        "scripted 200 to a received re-INVITE: the realign obligation is \
         observed open until the peer's ACK closes it; settle clean",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let saw_open = Arc::new(AtomicBool::new(false));
    let probe = saw_open.clone();
    let call = CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Reinvite),
                    // The guard polls on every observed-state tick while
                    // pending: it witnesses the scripted answer's
                    // obligation OPEN (before alice's ACK closes it).
                    Goal::new(
                        Barrier::pred("reneg_done", move |s| {
                            if s.describe_open()
                                .iter()
                                .any(|o| o.contains("bob:re-INVITE") && o.contains("realign"))
                            {
                                probe.store(true, Ordering::SeqCst);
                            }
                            s.leg("alice").reneg_count() >= 1
                        }),
                        GoalStep::Bye,
                    ),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
            
                cseq: None,
                delayed: None,
            },
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
                    // The delayed-offer re-INVITE is bodyless.
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::InDialog(InDialogMethod::Invite),
                            body: BodyExpect::Any,
                            matcher: None,
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
    assert!(verdict.is_ok(), "the scripted realign must settle OK, got {verdict:?}");
    assert!(
        saw_open.load(Ordering::SeqCst),
        "the answered-awaiting-ACK obligation was observed OPEN mid-call — \
         it held settle until the peer's ACK closed it",
    );
    h.finish().await;
}

/// A scripted 200 answer to a RECEIVED BYE (`ExpectRequest{InDialog(Bye)}`
/// + `Respond{200}`): the script — not the reactor — services the
/// teardown, with the leg-termination bookkeeping and no stray entry.
#[tokio::test(start_paused = true)]
async fn scripted_bye_answer_tears_down_cleanly() {
    use sip_message::generators::InDialogMethod;

    let h = Harness::new("actor-scripted-bye-answer").describe(
        "scripted 200 to a received BYE: script-owned teardown, clean \
         settle, no serviced-stray entry for the BYE",
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
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::Initial,
                            body: BodyExpect::SdpPresent,
                            matcher: None,
                        },
                    ),
                    Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::InDialog(InDialogMethod::Bye),
                            body: BodyExpect::Any,
                            matcher: None,
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

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict =
        run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the scripted BYE answer must settle OK, got {verdict:?}");
    assert!(
        !obs.replay_record().iter().any(|e| matches!(
            e,
            ReplayEntry::ServicedStray { method, .. } if method == "BYE"
        )),
        "the SCRIPT serviced the BYE — no stray entry: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}

/// The `ack_body` override end-to-end: the ACK to a delayed-offer
/// re-INVITE 2xx carries the `ExpectResponse{ack_body}` bytes instead of
/// the engine-built answer SDP. (A byte-identical retransmitted 2xx is
/// absorbed below `recv_any` by the fake lane's receive-view dedup, so the
/// re-surfacing idempotence is pinned by the unit test below.)
#[tokio::test(start_paused = true)]
async fn ack_body_override_rides_the_reinvite_ack() {
    const CUSTOM_ACK_SDP: &str = "v=0\r\no=alice 2 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";

    let h = Harness::new("actor-ack-body-override").describe(
        "ExpectResponse{ack_body} on a delayed-offer re-INVITE 2xx: the \
         ACK carries the override bytes, not the engine answer SDP",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // The hand-rolled peer answers, 2xx's the re-INVITE, and asserts the
    // ACK body is the override.
    let bob_srv = bob.clone();
    let server = tokio::spawn(async move {
        let bob = bob_srv;
        let mut inv = bob.try_receive("INVITE").await.unwrap();
        inv.respond(180, "Ringing").try_send().await.unwrap();
        inv.respond(200, "OK").with_sdp(ANSWER_SDP).try_send().await.unwrap();
        bob.try_receive("ACK").await.unwrap();
        let mut re = bob.try_receive("INVITE").await.unwrap();
        re.respond(200, "OK").with_sdp(ANSWER_SDP).try_send().await.unwrap();
        let ack = bob.try_receive("ACK").await.unwrap();
        assert_eq!(
            ack.request().body(),
            CUSTOM_ACK_SDP.as_bytes(),
            "the ACK carries the ack_body override",
        );
        bob.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
    });

    let alice_confirmed =
        Barrier::pred("alice_confirmed", |s| s.leg_at_least("alice", LegPhase::Confirmed));
    let expect = |status: u16, ack_body: Option<Vec<u8>>| GoalStep::ExpectResponse {
        status,
        body: BodyExpect::Any,
        early: None,
        ack_body,
        matcher: None,
    };
    let call = CallPlan {
        actors: vec![caller_spec(
            "alice",
            &alice,
            ("bob", bob.clone()),
            vec![
                Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                // Consume the establishment responses in order so the
                // re-INVITE reception goal aligns on ITS final.
                Goal::new(Barrier::None, expect(180, None)),
                Goal::new(Barrier::None, expect(200, None)),
                Goal::new(alice_confirmed, GoalStep::Reinvite),
                Goal::new(
                    Barrier::None,
                    expect(200, Some(CUSTOM_ACK_SDP.as_bytes().to_vec())),
                ),
                Goal::new(Barrier::None, GoalStep::Bye).after(Duration::from_millis(100)),
            ],
        )],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the override call must settle OK, got {verdict:?}");
    server.await.unwrap();
    h.finish().await;
}

/// The ACK-body resolution is resolved ONCE per CSeq and cached: a 2xx
/// re-surfacing AFTER the goal cursor advanced past the override-carrying
/// goal still draws the identical override bytes (RFC 3261 §13.2.2.4),
/// and a different CSeq resolved later falls to the engine default.
#[test]
fn ack_body_resolution_is_cached_per_cseq() {
    use std::collections::HashMap;

    let mut cache: HashMap<u32, String> = HashMap::new();
    let override_goal = GoalStep::ExpectResponse {
        status: 200,
        body: BodyExpect::Any,
        early: None,
        ack_body: Some(b"custom-answer".to_vec()),
        matcher: None,
    };
    // First resolution: the pending override wins and is cached.
    assert_eq!(
        crate::actor::response::resolve_ack_body(&mut cache, Some(&override_goal), "engine-sdp", 2),
        "custom-answer",
    );
    // The 2xx re-surfaces after the cursor advanced (next goal is Bye):
    // the CACHED bytes are re-emitted, never the engine default.
    assert_eq!(
        crate::actor::response::resolve_ack_body(&mut cache, Some(&GoalStep::Bye), "engine-sdp", 2),
        "custom-answer",
    );
    // A different CSeq with no pending override takes the engine default.
    assert_eq!(
        crate::actor::response::resolve_ack_body(&mut cache, Some(&GoalStep::Bye), "engine-sdp", 3),
        "engine-sdp",
    );
}

/// The per-goal deadline override bounds the guard wait tighter than the
/// 32 s step timeout: a never-holding guard with `.deadline(2s)` fails the
/// actor with the barrier's bounded Timeout well before the ceiling.
#[tokio::test(start_paused = true)]
async fn per_goal_deadline_bounds_the_guard_wait() {
    let h = Harness::new("actor-goal-deadline").describe(
        "a capture-declared tighter bound: .deadline(2s) on a never-holding \
         guard times the goal out long before the 32 s step timeout",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;

    let call = CallPlan {
        actors: vec![ActorSpec {
            role: "alice",
            agent: alice.clone(),
            disposition: Disposition::Caller,
            media: MediaState::none(),
            goals: vec![Goal::new(Barrier::pred("never", |_| false), GoalStep::Bye)
                .deadline(Duration::from_secs(2))],
            invite_targets: vec![],
            via: None,
            feed: CtxFeed::default(),
        
            cseq: None,
            delayed: None,
        }],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
    };

    let started = tokio::time::Instant::now();
    let verdict = run_call(call, Duration::from_secs(32)).await;
    let elapsed = started.elapsed();
    match verdict {
        CallVerdict::Failed(StepError::Timeout { who }) => assert_eq!(who, "never"),
        other => panic!("expected the bounded guard Timeout, got {other:?}"),
    }
    assert!(
        elapsed < Duration::from_secs(10),
        "the 2 s per-goal deadline (not the 32 s ceiling) bounded the wait: {elapsed:?}",
    );
    h.finish().await;
}

/// Content matcher NEGATIVE: a changed frozen-header value fails fast at
/// consume time with the match surface's detailed finding. Staged on an
/// in-dialog INFO so the establishment (and the wire) stays clean.
#[tokio::test(start_paused = true)]
async fn content_matcher_fails_fast_on_changed_value() {
    use sip_message::generators::InDialogMethod;

    let h = Harness::new("actor-content-matcher-fails").describe(
        "ExpectRequest{matcher} on an INFO whose X-Cap drifted: fail-fast \
         with the template-match finding naming header and values",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let matcher = MessageTemplate::request(
        Method::Info,
        vec![TemplateHeader::frozen("X-Cap", "v2")],
        b"payload".to_vec(),
    );
    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(
                        Barrier::AllConfirmed(&["alice", "bob"]),
                        GoalStep::InDialog {
                            method: InDialogMethod::Info,
                            content_type: Some("application/x-cap-test".to_string()),
                            body: Some(b"payload".to_vec()),
                            headers: vec![("X-Cap".to_string(), "v1".to_string())],
                        },
                    ),
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
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::InDialog(InDialogMethod::Info),
                            body: BodyExpect::Present,
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
    match verdict {
        CallVerdict::Failed(StepError::UnexpectedKind { who, detail }) => {
            assert_eq!(who, "bob");
            assert!(
                detail.contains("x-cap") && detail.contains("v2") && detail.contains("v1"),
                "the match surface's finding names header and values: {detail}",
            );
        }
        other => panic!("expected the matcher's fail-fast finding, got {other:?}"),
    }
    h.finish().await;
}
