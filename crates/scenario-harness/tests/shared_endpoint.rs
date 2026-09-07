//! Several actors on ONE endpoint: a peer socket that both ORIGINATES a call
//! and RECEIVES one under a different Call-ID (the application-server loopback
//! shape — the far end re-originates the call back over the same socket pair).
//!
//! Both endpoints of these calls are shared: the peer hosts `puac` (the
//! originator) and `puas` (the receiver), the application server hosts `asuas`
//! (receiver) and `asuac` (re-originator). Each endpoint's ONE receive pump
//! demultiplexes in-dialog traffic by dialog identity and an inbound initial
//! INVITE by the actors' `ClaimRule`s. There is no SUT — the two UA stacks talk
//! to each other over the recording-wrapped simulated fabric, so the RFC hard
//! gate at `finish()` judges the whole exchange.

use std::time::Duration;

use scenario_harness::actor::{
    await_pred, phase, run_call_with, ActorSpec, Automatics, Barrier, CallPlan, CallVerdict,
    CtxFeed, Disposition, Goal, GoalStep, LegPhase, MediaState, ObservedState, ReplayEntry,
    SettleBarrier,
};
use scenario_harness::realcall::CallCtx;
use scenario_harness::{Agent, ClaimRule, Harness, StepError, ANSWER_SDP, OFFER_SDP};

/// The per-barrier wait bound for these SUT-less calls.
const CEILING: Duration = Duration::from_secs(30);

/// Every leg of the loopback confirmed — the establishment gate.
fn all_up(s: &scenario_harness::actor::StateInner) -> bool {
    ["puac", "asuas", "asuac", "puas"]
        .iter()
        .all(|r| s.leg_at_least(r, LegPhase::Confirmed))
}

/// The four actors of the loopback: two per endpoint, opposite roles, distinct
/// dialogs. `teardown` gates BOTH originators' BYEs, so a test can hold the call
/// up until whatever it wants to observe has landed.
fn loopback_actors(
    peer: &Agent,
    as_ua: &Agent,
    teardown: impl Fn() -> Barrier,
) -> Vec<ActorSpec> {
    vec![
        // The peer's originating half: places the call, hangs it up.
        ActorSpec {
            role: "puac",
            agent: peer.clone(),
            disposition: Disposition::Caller,
            media: MediaState::offer(OFFER_SDP),
            goals: vec![
                Goal::new(Barrier::None, GoalStep::Invite { callee: "asuas", plan: None }),
                Goal::new(teardown(), GoalStep::Bye),
            ],
            invite_targets: vec![("asuas", as_ua.clone())],
            via: None,
            feed: CtxFeed::default(),
            cseq: None,
            delayed: vec![],
            claim: None,
        },
        // The AS's receiving half: owns the leg addressed to the AS user-part.
        ActorSpec {
            role: "asuas",
            agent: as_ua.clone(),
            disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(500) },
            media: MediaState::answer(ANSWER_SDP),
            goals: vec![],
            invite_targets: vec![],
            via: None,
            feed: CtxFeed::default(),
            cseq: None,
            delayed: vec![],
            claim: Some(ClaimRule::RuriUser("as".into())),
        },
        // The AS's re-originating half: once the inbound leg rings, it places
        // the loopback call back over the SAME socket pair.
        ActorSpec {
            role: "asuac",
            agent: as_ua.clone(),
            disposition: Disposition::Caller,
            media: MediaState::offer(OFFER_SDP),
            goals: vec![
                Goal::new(
                    Barrier::pred("inbound_leg_early", |s| {
                        s.leg_at_least("asuas", LegPhase::Early)
                    }),
                    GoalStep::Invite { callee: "puas", plan: None },
                ),
                Goal::new(teardown(), GoalStep::Bye),
            ],
            invite_targets: vec![("puas", peer.clone())],
            via: None,
            feed: CtxFeed::default(),
            cseq: None,
            delayed: vec![],
            claim: None,
        },
        // The peer's receiving half: owns the loopback leg by its R-URI user.
        ActorSpec {
            role: "puas",
            agent: peer.clone(),
            disposition: Disposition::RingThenAnswer { ring: Duration::from_millis(500) },
            media: MediaState::answer(ANSWER_SDP),
            goals: vec![],
            invite_targets: vec![],
            via: None,
            feed: CtxFeed::default(),
            cseq: None,
            delayed: vec![],
            claim: Some(ClaimRule::RuriUser("peer".into())),
        },
    ]
}

fn plan(actors: Vec<ActorSpec>) -> CallPlan {
    CallPlan {
        actors,
        plan: vec![phase("loopback_established", all_up)],
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    }
}

/// The loopback replays end to end: one socket pair carries TWO dialogs in
/// opposite directions, each actor drives only its own, and both tear down.
/// Without the per-endpoint demux the originator's reactor would consume the
/// inbound leg (and vice versa) and no leg would confirm.
#[tokio::test(start_paused = true)]
async fn one_endpoint_originates_and_receives_distinct_dialogs() {
    let h = Harness::new("shared-endpoint-loopback").describe(
        "an application-server loopback: the peer socket originates a call and \
         receives the re-originated one under a different Call-ID; the endpoint's \
         one receive pump routes each dialog to its own actor",
    );
    let peer = h.agent("peer", "127.0.0.1:5060").await;
    let as_ua = h.agent("as", "127.0.0.1:5080").await;

    let obs = ObservedState::new();
    let ctx = CallCtx::new();
    let actors = loopback_actors(&peer, &as_ua, || {
        Barrier::AllConfirmed(&["puac", "asuas", "asuac", "puas"])
    });
    let verdict = run_call_with(plan(actors), obs.clone(), &ctx, CEILING, None).await;
    assert!(
        matches!(verdict, CallVerdict::Ok),
        "the loopback call must settle OK, got {verdict:?}",
    );

    // Each half of each endpoint drove its OWN dialog to a confirmed, then
    // terminated, state — nothing was consumed by the wrong actor.
    obs.with_snapshot(|s| {
        for role in ["puac", "asuas", "asuac", "puas"] {
            assert_eq!(
                s.leg(role).phase(),
                LegPhase::Terminated,
                "{role} must reach its own terminal state",
            );
        }
    });
    assert!(
        obs.unclaimed_inbound().is_empty(),
        "every inbound found its actor: {:?}",
        obs.unclaimed_inbound(),
    );
    h.finish().await;
}

/// An inbound initial INVITE no PENDING claim owns is counted and recorded — it
/// is never handed to the endpoint's originating actor, and never silently
/// dropped. The stray's own client transaction gives up (its INVITE draws no
/// response), so the leg still terminates; the loopback call itself is
/// unaffected and settles clean.
#[tokio::test(start_paused = true)]
async fn an_unclaimed_initial_invite_is_recorded_not_consumed() {
    let h = Harness::new("shared-endpoint-unclaimed").describe(
        "a third party INVITEs the shared peer socket after every claim has \
         fired: the endpoint records the unclaimed leg (never routing it to the \
         originating actor) and the loopback call is undisturbed",
    );
    let peer = h.agent("peer", "127.0.0.1:5060").await;
    let as_ua = h.agent("as", "127.0.0.1:5080").await;
    let stray_ua = h.agent("stray", "127.0.0.1:5090").await;

    let obs = ObservedState::new();
    let ctx = CallCtx::new();
    // Teardown waits for the unclaimed record, so the pump is still running when
    // the stray INVITE lands (and the call is not held up any longer than that).
    let actors = loopback_actors(&peer, &as_ua, || {
        Barrier::pred("stray_recorded", |s| {
            all_up(s)
                && s.replay_record()
                    .iter()
                    .any(|e| matches!(e, ReplayEntry::UnclaimedInbound { .. }))
        })
    });

    let drive = run_call_with(plan(actors), obs.clone(), &ctx, CEILING, None);
    let stray = async {
        // Both claims have fired once the loopback leg is confirmed.
        let deadline = tokio::time::Instant::now() + CEILING;
        await_pred(&obs, "loopback_up", all_up, deadline).await.expect("loopback established");
        let mut inv = stray_ua.invite(&peer).with_sdp(OFFER_SDP).send().await;
        // Nobody owns the leg: the transaction gives up rather than being
        // answered by an actor it does not belong to.
        match inv.try_expect(200).await {
            Err(StepError::Timeout { .. }) => {}
            other => panic!("an unclaimed leg must draw no response, got {other:?}"),
        }
    };
    let (verdict, ()) = tokio::join!(drive, stray);
    assert!(
        matches!(verdict, CallVerdict::Ok),
        "the loopback call settles clean beside the unclaimed leg, got {verdict:?}",
    );

    let unclaimed = obs.unclaimed_inbound();
    assert_eq!(unclaimed.len(), 1, "exactly one unclaimed leg recorded: {unclaimed:?}");
    match &unclaimed[0] {
        ReplayEntry::UnclaimedInbound { endpoint, detail } => {
            assert_eq!(endpoint, "peer", "the record names the endpoint it arrived on");
            assert!(detail.contains("INVITE"), "the record names the message: {detail}");
        }
        other => panic!("unexpected record {other:?}"),
    }
    h.finish().await;
}
