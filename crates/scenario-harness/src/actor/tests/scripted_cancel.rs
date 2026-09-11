use sip_message::{EmitOpts, MessageTemplate, TemplateHeader};
use std::time::Duration;

use super::testkit::*;
use crate::actor::*;
use crate::Harness;

/// Parked-CANCEL fail-fast: the CANCEL automatic consumes the parked
/// initial INVITE (200 + 487); the later `RespondTemplate` bound to it
/// fails immediately with the bounded StepError naming the automatic —
/// never a 32 s goal timeout.
#[tokio::test(start_paused = true)]
async fn cancel_consumed_parked_invite_fails_respond_fast() {
    let h = Harness::new("actor-parked-cancel-fail-fast").describe(
        "alice CANCELs the parked INVITE (CANCEL→200+487 automatic); bob's \
         later RespondTemplate fails fast with the bounded tombstone error",
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
                    Goal::new(Barrier::None, GoalStep::Cancel { stated: Vec::new() })
                        .after(Duration::from_millis(200)),
                ],
            ),
            scripted_spec(
                "bob",
                &bob,
                // Gated PAST the cancellation, so the automatic has already
                // consumed the parked INVITE when the script reaches it.
                vec![Goal::new(
                    Barrier::pred("caller_gone", |s| s.leg_at_least("alice", LegPhase::Terminated)),
                    GoalStep::RespondTemplate {
                        template: response_template(200, "OK", true),
                        opts: EmitOpts::default(),
                        early: None,
                    },
                )],
            ),
        ],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        // The §5 automatic: the parked INVITE draws an immediate 100, so
        // the CANCEL follows a provisional (RFC 3261 §9.1).
        automatics: Automatics { answer_100_trying: true, ..Default::default() },
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    match verdict {
        CallVerdict::Failed(StepError::UnexpectedKind { who, detail }) => {
            assert_eq!(who, "bob");
            assert!(
                detail.contains("consumed by an automatic"),
                "the bounded error names the automatic: {detail}",
            );
        }
        other => panic!("expected the bounded tombstone StepError, got {other:?}"),
    }
    h.finish().await;
}

/// The scripted 487 response template for the CANC01-style abort — a
/// frozen `X-Cap-487` pins verbatim-template fidelity end to end (bob
/// emits it, alice's matcher verifies it).
fn cap487_template() -> MessageTemplate {
    MessageTemplate::response(
        487,
        "Request Terminated",
        vec![TemplateHeader::frozen("X-Cap-487", "cap-1")],
        Vec::new(),
    )
}

/// Scripted CANCEL reception (053, CANC01 shape): bob claims the INVITE,
/// rings 180 on the bound transaction, `ExpectRequest{Cancel}` parks the
/// caller's CANCEL (its 200 stays automatic), and the scripted
/// `RespondTemplate 487` — frozen headers verbatim — rides the BOUND
/// INVITE transaction. Alice's script mirrors the captured A side:
/// expect 180, CANCEL, expect 200, expect 487 (matcher pins the frozen
/// header). Clean terminal, settle green, RFC hard gate green.
#[tokio::test(start_paused = true)]
async fn scripted_cancel_reception_487_rides_bound_invite() {
    let h = Harness::new("actor-scripted-cancel-reception").describe(
        "CANC01: INVITE→180 on bound txn→ExpectRequest{Cancel} parks the \
         CANCEL (auto-200)→RespondTemplate 487 verbatim on the BOUND INVITE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let expect_resp = |status: u16, matcher: Option<MessageTemplate>| GoalStep::ExpectResponse {
        status,
        cseq_method: None,
        body: BodyExpect::Any,
        early: None,
        ack_body: None,
        matcher,
    };
    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::None, expect_resp(180, None)),
                    Goal::new(Barrier::None, GoalStep::Cancel { stated: Vec::new() }),
                    // The CANCEL hop's 200, then the 487 with the frozen
                    // header — verbatim fidelity pinned by the matcher.
                    Goal::new(Barrier::None, expect_resp(200, None)),
                    Goal::new(Barrier::None, expect_resp(487, Some(cap487_template()))),
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
                            rank: None,
                        },
                    ),
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
                        GoalStep::ExpectRequest {
                            kind: RequestKind::Cancel,
                            body: BodyExpect::Any,
                            matcher: None,
                            rank: None,
                        },
                    ),
                    Goal::new(
                        Barrier::None,
                        GoalStep::RespondTemplate {
                            template: cap487_template(),
                            opts: EmitOpts::default(),
                            early: None,
                        },
                    ),
                ],
            ),
        ],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(
        verdict.is_ok(),
        "the scripted CANCEL abort must reach a clean terminal, got {verdict:?}"
    );
    h.finish().await;
}

/// Gap 2 (053): a CANCEL hits a script-BOUND INVITE (claimed by
/// `ExpectRequest{Initial}`, 180 already sent on the bound transaction)
/// with NO remaining `ExpectRequest{Cancel}` — the automatic mirrors the
/// parked path: 200 the CANCEL, 487 the BOUND INVITE, terminate the leg
/// (recorded as a serviced stray) instead of wedging the peer in
/// retransmits.
#[tokio::test(start_paused = true)]
async fn cancel_automatic_487s_script_bound_invite() {
    let h = Harness::new("actor-cancel-automatic-bound").describe(
        "CANCEL on a script-bound INVITE with no scripted claim: the \
         automatic answers 200 + 487 on the BOUND transaction and the \
         call tears down cleanly",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let ringing = |s: &StateInner| s.leg_at_least("alice", LegPhase::Early);
    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(
                        Barrier::pred("ringing", ringing),
                        GoalStep::Cancel { stated: Vec::new() },
                    )
                    .after(Duration::from_millis(200)),
                ],
            ),
            // The script claims the INVITE and rings, then ends — no
            // CANCEL expectation, no scripted final.
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
                            rank: None,
                        },
                    ),
                    Goal::new(
                        Barrier::None,
                        GoalStep::RespondTemplate {
                            template: response_template(180, "Ringing", false),
                            opts: EmitOpts::default(),
                            early: None,
                        },
                    ),
                ],
            ),
        ],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the automatic must 487 the bound INVITE and settle, got {verdict:?}");
    assert!(
        obs.replay_record().contains(&ReplayEntry::ServicedStray {
            leg: "bob",
            method: "CANCEL".to_string(),
            action: "200 + 487 on the bound INVITE",
        }),
        "the bound-INVITE automatic is recorded, never silent: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}

/// Precedence (053): the CANCEL lands while the script's
/// `ExpectRequest{Cancel}` is still DWELLING — the expectation wins: the
/// CANCEL parks (auto-200 only, no automatic 487), waits out the dwell,
/// and the scripted policy `Respond 487` rides the bound INVITE. The
/// automatic never fires (no serviced-stray CANCEL entry).
#[tokio::test(start_paused = true)]
async fn parked_cancel_waits_for_dwelling_cancel_expectation() {
    let h = Harness::new("actor-cancel-precedence-dwell").describe(
        "CANCEL arrives mid-dwell of ExpectRequest{Cancel}: parked, not \
         auto-487'd; the scripted Respond 487 answers the bound INVITE",
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
                            status: 180,
                            cseq_method: None,
                            body: BodyExpect::Any,
                            early: None,
                            ack_body: None,
                            matcher: None,
                        },
                    ),
                    Goal::new(Barrier::None, GoalStep::Cancel { stated: Vec::new() }),
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
                            rank: None,
                        },
                    ),
                    Goal::new(
                        Barrier::None,
                        GoalStep::RespondTemplate {
                            template: response_template(180, "Ringing", false),
                            opts: EmitOpts::default(),
                            early: None,
                        },
                    ),
                    // The dwell holds the cursor here while the CANCEL
                    // arrives — the parked CANCEL must WAIT, not trip the
                    // automatic.
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::Cancel,
                            body: BodyExpect::Any,
                            matcher: None,
                            rank: None,
                        },
                    )
                    .after(Duration::from_millis(300)),
                    Goal::new(Barrier::None, GoalStep::Respond { status: 487 }),
                ],
            ),
        ],
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(
        verdict.is_ok(),
        "the parked CANCEL must wait for the dwelling expectation, got {verdict:?}"
    );
    assert!(
        obs.replay_record()
            .iter()
            .all(|e| !matches!(e, ReplayEntry::ServicedStray { method, .. } if method == "CANCEL")),
        "the automatic must never fire when the script claims the CANCEL: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}
