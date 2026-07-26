use sip_message::EmitOpts;
use std::time::Duration;

use crate::actor::*;
use super::testkit::*;
use crate::Harness;

/// The bye-for-cancel confrontation plan: alice rings bob then abandons with a CANCEL,
/// while bob's script — lowered from a capture whose caller sent a
/// pre-answer BYE — expects a BYE and scripts its 200. Only an installed
/// accepted-delta policy can bridge the two.
fn bye_expecting_cancel_plan(
    alice: &crate::Agent,
    bob: &crate::Agent,
    delta_policy: Option<AcceptedDeltaPolicy>,
) -> CallPlan {
    use sip_message::generators::InDialogMethod;
    let expect_resp = |status: u16| GoalStep::ExpectResponse {
        status, cseq_method: None,
        body: BodyExpect::Any,
        early: None,
        ack_body: None,
        matcher: None,
    };
    CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::None, expect_resp(180)),
                    Goal::new(Barrier::None, GoalStep::Cancel),
                    Goal::new(Barrier::None, expect_resp(200)),
                    Goal::new(Barrier::None, expect_resp(487)),
                ],
            ),
            scripted_spec(
                "bob",
                bob,
                vec![
                    Goal::new(
                        Barrier::None,
                        GoalStep::ExpectRequest {
                            kind: RequestKind::Initial,
                            body: BodyExpect::SdpPresent,
                            matcher: None,
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
                    // The captured choreography: a pre-answer BYE, 200'd
                    // by script — what the live peer never sends.
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
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy,
    }
}

/// The caller-side BYE ≈ CANCEL rule, installed FROM TEST CODE (the crate
/// ships only the hook): blessed ONLY with exactly one early dialog
/// (RFC 3261 §15.1.2 vs §9 — under forking the two are not equivalent);
/// satisfies the BYE expectation AND its scripted 200 with the given
/// mechanical reaction.
fn bye_for_cancel_policy_with(reaction: DeltaReaction) -> AcceptedDeltaPolicy {
    Arc::new(move |ctx: &DeltaContext<'_>| {
        let expects_bye = matches!(
            &ctx.expected,
            ExpectedStimulus::Request(RequestKind::InDialog(m)) if m.as_str() == "BYE"
        );
        let observed_cancel = matches!(
            &ctx.observed,
            ObservedStimulus::Request(r) if r.method.as_str() == "CANCEL"
        );
        if expects_bye && observed_cancel && ctx.dialog.early_dialog_count == 1 {
            DeltaDecision::Accepted(AcceptedDelta {
                rule: "bye-approx-cancel-single-early-dialog",
                satisfies_steps: 2,
                reaction,
            })
        } else {
            DeltaDecision::NotAccepted
        }
    })
}

/// The BYE ≈ CANCEL rule with the CANCEL-automatic mechanics (200 + 487
/// on the bound INVITE).
fn bye_for_cancel_policy() -> AcceptedDeltaPolicy {
    bye_for_cancel_policy_with(DeltaReaction::TerminatePendingInitial)
}

/// Accepted delta, the concrete BYE → CANCEL case: bob's script
/// expects a BYE (+ scripted 200) on a ringing call; the peer CANCELs
/// instead; the installed policy (checking `early_dialog_count == 1` from
/// the provided context) accepts — the stack 200s the CANCEL, 487s the
/// BOUND INVITE via the CANCEL-automatic mechanics, the scripted BYE steps
/// are satisfied, and the run completes with the `AcceptedDelta`
/// observation (rule name included) in the replay record. NEVER as a
/// serviced stray — a blessed substitution is not divergence.
#[tokio::test(start_paused = true)]
async fn accepted_delta_bye_for_cancel_completes_script() {
    let h = Harness::new("actor-accepted-delta-bye-cancel").describe(
        "ADR-0024 §6: script expects BYE, peer sends CANCEL; the plan's accepted-delta \
         policy blesses it (one early dialog) → 200 + 487 on the bound INVITE, \
         BYE steps satisfied, AcceptedDelta recorded",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = bye_expecting_cancel_plan(&alice, &bob, Some(bye_for_cancel_policy()));
    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the accepted delta must complete the run, got {verdict:?}");
    assert!(
        obs.replay_record().contains(&ReplayEntry::AcceptedDelta {
            leg: "bob",
            step: 2,
            expected: "BYE".to_string(),
            observed: "CANCEL".to_string(),
            rule: "bye-approx-cancel-single-early-dialog",
        }),
        "the acceptance is never silent — the AcceptedDelta entry must carry \
         the rule: {:?}",
        obs.replay_record(),
    );
    assert!(
        obs.replay_record()
            .iter()
            .all(|e| !matches!(e, ReplayEntry::ServicedStray { method, .. } if method == "CANCEL")),
        "a blessed substitution must not double-book as a serviced stray: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}

/// `NotAccepted`: the SAME plan with a policy that declines —
/// behavior is byte-identical to today's mismatch path (and to the
/// no-policy run below): the CANCEL automatic services the abandon as a
/// recorded stray, no `AcceptedDelta` appears, and the scripted BYE steps
/// stay unsatisfied (the consumed-target tombstone guards them).
#[tokio::test(start_paused = true)]
async fn not_accepted_delta_keeps_todays_mismatch_path() {
    let h = Harness::new("actor-delta-not-accepted").describe(
        "ADR-0024 §6: the policy declines the BYE→CANCEL substitution — the CANCEL \
         automatic (stray) fires exactly as with no policy installed",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let decline: AcceptedDeltaPolicy = Arc::new(|_| DeltaDecision::NotAccepted);
    let call = bye_expecting_cancel_plan(&alice, &bob, Some(decline));
    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the automatic still tears the call down, got {verdict:?}");
    assert!(
        obs.accepted_deltas().is_empty(),
        "NotAccepted must record no acceptance: {:?}",
        obs.replay_record(),
    );
    assert!(
        obs.replay_record().contains(&ReplayEntry::ServicedStray {
            leg: "bob",
            method: "CANCEL".to_string(),
            action: "200 + 487 on the bound INVITE",
        }),
        "the mismatch stays the automatic's recorded stray: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}

/// No policy installed: the hook is absent — zero behavior change
/// from today, and NO `AcceptedDelta` observation exists (the never-silent
/// contract's converse).
#[tokio::test(start_paused = true)]
async fn no_policy_bye_for_cancel_unchanged_and_silent_free() {
    let h = Harness::new("actor-delta-no-policy").describe(
        "ADR-0024 §6: no accepted-delta policy — the BYE-expecting script meets a \
         CANCEL exactly as today (automatic + stray), no AcceptedDelta entry",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = bye_expecting_cancel_plan(&alice, &bob, None);
    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the automatic still tears the call down, got {verdict:?}");
    assert!(
        obs.accepted_deltas().is_empty(),
        "no policy → no acceptance may appear: {:?}",
        obs.replay_record(),
    );
    assert!(
        obs.replay_record().contains(&ReplayEntry::ServicedStray {
            leg: "bob",
            method: "CANCEL".to_string(),
            action: "200 + 487 on the bound INVITE",
        }),
        "the mismatch stays the automatic's recorded stray: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}

/// `DeltaReaction::Default` on an observed CANCEL: the standard handling
/// IS the CANCEL automatic (200 + 487 on the bound INVITE), run with the
/// stray record suppressed — the acceptance records ONLY the
/// `AcceptedDelta` entry, never a double-booked `ServicedStray`.
#[tokio::test(start_paused = true)]
async fn accepted_delta_default_reaction_on_cancel_never_strays() {
    let h = Harness::new("actor-delta-default-cancel").describe(
        "ADR-0024 §6: a Default-reaction acceptance of an observed CANCEL \
         runs the automatic's mechanics without recording a serviced stray",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let call = bye_expecting_cancel_plan(
        &alice,
        &bob,
        Some(bye_for_cancel_policy_with(DeltaReaction::Default)),
    );
    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the Default-reaction acceptance must complete, got {verdict:?}");
    assert!(
        obs.replay_record().iter().any(|e| matches!(
            e,
            ReplayEntry::AcceptedDelta { rule: "bye-approx-cancel-single-early-dialog", .. }
        )),
        "the acceptance is recorded: {:?}",
        obs.replay_record(),
    );
    assert!(
        obs.replay_record()
            .iter()
            .all(|e| !matches!(e, ReplayEntry::ServicedStray { method, .. } if method == "CANCEL")),
        "a Default-reaction acceptance must not double-book as a stray: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}

/// The RFC scoping under FORKING (the reason the blessing is conditional):
/// bob's script rings TWO early dialogs (the transaction's default tag +
/// an explicit fork tag), so `early_dialog_count == 2` — the `== 1` policy
/// declines and today's behavior stands (the CANCEL automatic as a
/// recorded stray, no acceptance). Pins the per-tag counting.
#[tokio::test(start_paused = true)]
async fn forked_early_dialogs_decline_bye_for_cancel() {
    use sip_message::generators::InDialogMethod;
    let h = Harness::new("actor-delta-forked-decline").describe(
        "ADR-0024 §6: two early dialogs (forked 180s) → the single-early-\
         dialog BYE≈CANCEL rule declines; the CANCEL automatic services it",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let expect_resp = |status: u16| GoalStep::ExpectResponse {
        status, cseq_method: None,
        body: BodyExpect::Any,
        early: None,
        ack_body: None,
        matcher: None,
    };
    let ring = |early: Option<EarlyId>| GoalStep::RespondTemplate {
        template: response_template(180, "Ringing", false),
        opts: EmitOpts::default(),
        early,
    };
    let call = CallPlan {
        actors: vec![
            caller_spec(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::None, expect_resp(180)),
                    Goal::new(Barrier::None, expect_resp(180)),
                    Goal::new(Barrier::None, GoalStep::Cancel),
                    Goal::new(Barrier::None, expect_resp(200)),
                    Goal::new(Barrier::None, expect_resp(487)),
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
                    // Two early dialogs on the ONE transaction (§12.1.2).
                    Goal::new(Barrier::None, ring(None)),
                    Goal::new(Barrier::None, ring(Some("fork-b"))),
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
        plan: vec![],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: Some(bye_for_cancel_policy()),
    };
    let ctx = CallCtx::new();
    let obs = ObservedState::new();
    let verdict = run_call_with(call, obs.clone(), &ctx, Duration::from_secs(5), None).await;
    assert!(verdict.is_ok(), "the automatic still tears the call down, got {verdict:?}");
    assert!(
        obs.accepted_deltas().is_empty(),
        "two early dialogs must decline the == 1 rule: {:?}",
        obs.replay_record(),
    );
    assert!(
        obs.replay_record().contains(&ReplayEntry::ServicedStray {
            leg: "bob",
            method: "CANCEL".to_string(),
            action: "200 + 487 on the bound INVITE",
        }),
        "the declined mismatch stays the automatic's recorded stray: {:?}",
        obs.replay_record(),
    );
    h.finish().await;
}
