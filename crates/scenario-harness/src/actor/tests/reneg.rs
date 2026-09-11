use std::time::Duration;

use crate::actor::*;
use crate::{Harness, ANSWER_SDP, OFFER_SDP};

/// C4/S5 re-INVITE glare (RFC 3261 §14.1): alice and bob BOTH originate a
/// re-INVITE at the same paused-clock instant (gated on `AllConfirmed`), so
/// each arrives while the peer's own re-INVITE is outstanding — BOTH get
/// `491 Request Pending`. Each end hop-ACKs the 491, closes its obligation,
/// and retries after the §14.1 back-off (owner alice: 2.5s; non-owner bob:
/// 1.0s), so bob's retry lands first (alice's re-INVITE no longer pending →
/// 200), then alice's. Both rounds complete; the RFC hard gate confirms both
/// 491s were ACKed and no obligation leaked.
#[tokio::test(start_paused = true)]
async fn reinvite_glare_491_both_ways_then_retry_resolves() {
    let h = Harness::new("actor-reinvite-glare").describe(
        "C4/S5: alice+bob re-INVITE at once → 491 both ways → §14.1 owner/\
         non-owner back-off retries (bob 1s, alice 2.5s) → both rounds \
         complete → BYE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let both_confirmed = Barrier::AllConfirmed(&["alice", "bob"]);
    let both_reneged = Barrier::pred("glare_resolved", |s| {
        s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
    });
    let call = CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                // Alice answers bob's realign re-INVITE with SDP (full media).
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                disposition: Disposition::Caller,
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(both_confirmed.clone(), GoalStep::Reinvite),
                    Goal::new(both_reneged.clone(), GoalStep::Bye),
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
                disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                media: MediaState::full(ANSWER_SDP, ANSWER_SDP),
                // Bob ALSO re-INVITEs on the same gate — the glare.
                goals: vec![Goal::new(both_confirmed.clone(), GoalStep::Reinvite)],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),

                cseq: None,
                delayed: vec![],
                claim: None,
            },
        ],
        plan: vec![
            phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            }),
            phase("glare_resolved", |s| {
                s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
            }),
        ],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(10)).await;
    assert!(verdict.is_ok(), "the glare must resolve via §14.1 retry, got {verdict:?}");

    h.finish().await;
}

/// C4/S6 UPDATE-vs-re-INVITE collision (RFC 3311 §5.2): alice sends a
/// re-INVITE and bob an UPDATE at the same instant, both carrying offers.
/// Each has an outstanding offer when the peer's offer-bearing request
/// arrives, so BOTH are rejected 491 (the re-INVITE 491 is hop-ACKed, the
/// UPDATE 491 takes no ACK). Each retries after the back-off (owner alice
/// 2.5s, non-owner bob 1.0s): bob's UPDATE retry lands first (alice has no
/// pending offer → 200), then alice's re-INVITE (bob's UPDATE done → 200).
/// Both renegotiations complete; the RFC hard gate confirms the wire.
#[tokio::test(start_paused = true)]
async fn update_vs_reinvite_collision_491_then_retry_resolves() {
    let h = Harness::new("actor-update-reinvite-glare").describe(
        "C4/S6: alice re-INVITE × bob UPDATE at once → 491 both ways → \
         back-off retries (bob UPDATE 1s, alice re-INVITE 2.5s) → both \
         offers complete → BYE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let both_confirmed = Barrier::AllConfirmed(&["alice", "bob"]);
    let both_reneged = Barrier::pred("collision_resolved", |s| {
        s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
    });
    let call = CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                disposition: Disposition::Caller,
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(both_confirmed.clone(), GoalStep::Reinvite),
                    Goal::new(both_reneged.clone(), GoalStep::Bye),
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
                disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(100) },
                media: MediaState::full(ANSWER_SDP, ANSWER_SDP),
                // Bob collides with an UPDATE on the same gate.
                goals: vec![Goal::new(both_confirmed.clone(), GoalStep::Update)],
                invite_targets: vec![],
                via: None,
                feed: CtxFeed::default(),

                cseq: None,
                delayed: vec![],
                claim: None,
            },
        ],
        plan: vec![
            phase("established", |s| {
                s.leg_at_least("alice", LegPhase::Confirmed)
                    && s.leg_at_least("bob", LegPhase::Confirmed)
            }),
            phase("collision_resolved", |s| {
                s.leg("alice").reneg_count() >= 1 && s.leg("bob").reneg_count() >= 1
            }),
        ],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(10)).await;
    assert!(verdict.is_ok(), "the UPDATE×re-INVITE collision must resolve, got {verdict:?}");

    h.finish().await;
}

/// C5 early UPDATE (RFC 3311 §5.1): alice INVITEs with 100rel; bob answers
/// reliably (183) and HOLDS the INVITE. Alice PRACKs, then — while still in
/// the EARLY dialog, before the final 200 — sends an UPDATE renegotiating
/// media. Bob 200s the UPDATE and only THEN answers the INVITE 200. Alice
/// ACKs and the call tears down. The RFC hard gate confirms the pre-answer
/// UPDATE rode the early dialog compliantly.
#[tokio::test(start_paused = true)]
async fn early_update_on_the_reliable_early_dialog() {
    let h = Harness::new("actor-early-update").describe(
        "C5: 100rel INVITE → reliable 183 → PRACK → EARLY UPDATE (200) → \
         final 200 INVITE → ACK → BYE; bob holds the INVITE until the early \
         UPDATE completes (RFC 3311 §5.1)",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // The caller advertises 100rel via a direct-to-bob plan.
    let plan = crate::realcall::InvitePlan {
        via: bob.addr(),
        from: None,
        to: None,
        ruri: None,
        headers: vec![("Supported".to_string(), "100rel".to_string())],
        rewrite: Default::default(),
    };
    let alice_confirmed =
        Barrier::pred("confirmed", |s| s.leg_at_least("alice", LegPhase::Confirmed));
    let call = CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                media: MediaState::full(OFFER_SDP, ANSWER_SDP),
                disposition: Disposition::Caller,
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: Some(plan) }),
                    // The early UPDATE fires once alice has PRACKed the
                    // reliable 183 (SUBFLOW_EARLY — a real post-183 signal,
                    // NOT LegPhase::Early which holds the instant she
                    // originates), before the final 200 (RFC 3311 §5.1).
                    Goal::new(
                        Barrier::pred("early", |s| s.leg("alice").subflow(SUBFLOW_EARLY).is_some()),
                        GoalStep::UpdateEarly,
                    ),
                    Goal::new(alice_confirmed.clone(), GoalStep::Bye),
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
                disposition: Disposition::ReliableAnswerEarlyUpdate,
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
        plan: vec![phase("confirmed", |s| s.leg_at_least("alice", LegPhase::Confirmed))],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the early-UPDATE call must settle OK, got {verdict:?}");

    h.finish().await;
}

/// The generic in-dialog origination primitive (`GoalStep::InDialog`): alice
/// establishes, sends an INFO carrying a typed body + an extra header on the
/// confirmed dialog, and hangs up ONLY once the observed state shows bob
/// received that INFO (`Barrier::received` — the ordering gate that replaces a
/// timed dwell). The INFO opens an `InDialog` obligation the settle barrier
/// holds on until its 2xx closes it; the RFC hard gate at `finish()` confirms
/// the INFO rode the wire compliantly.
#[tokio::test(start_paused = true)]
async fn actor_originates_in_dialog_info() {
    use sip_message::generators::InDialogMethod;

    let h = Harness::new("actor-in-dialog-info").describe(
        "in-dialog primitive: alice originates a plain in-dialog INFO (typed body) on \
         the confirmed dialog; bob 200s it reactively; alice BYEs gated on the \
         observed fact that bob received the INFO (Barrier::received), and the \
         settle barrier holds until the INFO's 2xx closes its InDialog obligation",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = CallPlan {
        actors: vec![
            ActorSpec {
                role: "alice",
                agent: alice.clone(),
                disposition: Disposition::Caller,
                media: MediaState::offer(OFFER_SDP),
                goals: vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    // Send the INFO once the call is up.
                    Goal::new(
                        Barrier::AllConfirmed(&["alice", "bob"]),
                        GoalStep::InDialog {
                            method: InDialogMethod::Info,
                            content_type: Some("application/x-info-test".to_string()),
                            body: Some(b"<info>eof</info>".to_vec()),
                            headers: vec![("X-Info-Kind".to_string(), "eof".to_string())],
                        },
                    ),
                    // Hang up only once bob has OBSERVABLY received the INFO —
                    // the ordering barrier, no timed dwell.
                    Goal::new(Barrier::received("info_seen", "bob", "INFO"), GoalStep::Bye),
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
                disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(200) },
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
    };

    let verdict = run_call(call, Duration::from_secs(5)).await;
    assert!(verdict.is_ok(), "the INFO call must settle OK, got {verdict:?}");

    h.finish().await;
}
