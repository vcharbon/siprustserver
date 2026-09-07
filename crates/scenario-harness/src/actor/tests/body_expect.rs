//! What a reception goal's [`BodyExpect`] claims about the body that arrived.
//! `Absent` is an ASSERTION — the message carries no body, the delayed-offer
//! shape of RFC 3264 §5 — and is therefore also a discriminator between two
//! same-method messages on one leg; `Any` is the absence of any claim.

use std::time::Duration;

use sip_message::generators::InDialogMethod;

use super::testkit::*;
use crate::actor::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// End to end: the delayed-offer re-INVITE is bodyless, so the callee's
/// `ExpectRequest{InDialog(Invite), Absent}` consumes it and the call
/// completes — the offer rides the 2xx and the answer the ACK (RFC 3264 §5).
#[tokio::test(start_paused = true)]
async fn absent_consumes_the_bodyless_reinvite() {
    let h = Harness::new("actor-body-expect-absent").describe(
        "ExpectRequest{InDialog(Invite), Absent} consumes the delayed-offer \
         re-INVITE: bodylessness asserted, round completed, clean teardown",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

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
                    Goal::new(
                        Barrier::pred("reneg_done", |s| s.leg("alice").reneg_count() >= 1),
                        GoalStep::Bye,
                    ),
                ],
                invite_targets: vec![("bob", bob.clone())],
                via: None,
                feed: CtxFeed::default(),
                cseq: None,
                delayed: vec![],
                claim: None,
            },
            scripted_spec(
                "bob",
                &bob,
                vec![
                    // The initial INVITE carries alice's offer …
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::Initial,
                            body: BodyExpect::SdpPresent,
                            matcher: None,
                            rank: None,
                        },
                    ),
                    Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
                    // … and the re-INVITE that follows carries none.
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::InDialog(InDialogMethod::Invite),
                            body: BodyExpect::Absent,
                            matcher: None,
                            rank: None,
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
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the bodyless re-INVITE satisfies Absent, got {verdict:?}");
    h.finish().await;
}

/// The claim vocabulary itself: `Absent` holds only for an empty body,
/// `Any` claims nothing either way, and each variant names itself for the
/// divergence record.
#[test]
fn body_expect_variants_assert_what_they_name() {
    assert!(BodyExpect::Absent.satisfied_by(0, false));
    assert!(!BodyExpect::Absent.satisfied_by(42, true));
    assert!(BodyExpect::Any.satisfied_by(0, false));
    assert!(BodyExpect::Any.satisfied_by(42, true));
    assert!(!BodyExpect::Present.satisfied_by(0, false));
    assert!(BodyExpect::Present.satisfied_by(42, false));
    assert!(!BodyExpect::SdpPresent.satisfied_by(0, false));
    assert!(BodyExpect::SdpPresent.satisfied_by(42, true));
    assert_eq!(BodyExpect::Absent.label(), "absent");
    assert_eq!(BodyExpect::SdpPresent.label(), "sdp");
}

/// A [`BodyExpect`] miss on an expect step is a divergence RECORD, never a
/// step failure: the offer-carrying INVITE misses `Absent`, the case still
/// runs to completion (200/ACK/BYE) and the replay record carries the miss —
/// visible, never silently accepted (replay-campaign family A5).
#[tokio::test(start_paused = true)]
async fn a_body_expect_miss_records_a_divergence_and_the_case_completes() {
    let h = Harness::new("actor-body-expect-miss-records").describe(
        "ExpectRequest{Initial, Absent} met by an offer-carrying INVITE: \
         BodyExpectMiss recorded, run completes, clean teardown",
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
                    // The INVITE carries alice's offer, so this claim misses.
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::Initial,
                            body: BodyExpect::Absent,
                            matcher: None,
                            rank: None,
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
        reception_observer: None,
    };

    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "a body miss is data, never a failure — got {verdict:?}");
    let replay = obs.replay_record();
    let miss = replay.iter().find_map(|e| match e {
        ReplayEntry::BodyExpectMiss {
            leg,
            expected,
            body_len,
            body_is_sdp,
            status,
            method,
            initial,
            ..
        } => Some((*leg, *expected, *body_len, *body_is_sdp, *status, method.clone(), *initial)),
        _ => None,
    });
    let (leg, expected, body_len, body_is_sdp, status, method, initial) =
        miss.expect("the replay record carries the miss");
    assert_eq!(leg, "bob");
    assert_eq!(expected, "absent");
    assert!(body_len > 0 && body_is_sdp, "the offer body is what missed the claim");
    assert_eq!(status, None, "a request-side miss carries no status");
    assert_eq!(method, "INVITE");
    assert!(initial, "the initial-INVITE axis rides the record");
    h.finish().await;
}
