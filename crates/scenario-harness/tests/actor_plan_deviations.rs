//! ADR-0024 §6: deviations and lane automatics that ride the ACTOR PLAN — the
//! `ActorSpec.cseq` shared counter, the `ActorSpec.delayed` automatic, and the
//! `ActorCall.automatics` carrier plumbed onto the `CallPlan`. Each drives a
//! full SUT-less actor call through `run_built_actor_call` over the recording-
//! wrapped simulated network, then asserts the effect on the recorded wire.

use scenario_harness::actor::{
    phase, run_built_actor_call, run_call_with, ActorCall, ActorSpec, Automatics, Barrier,
    CallPlan, CtxFeed, Disposition, Expect, Goal, GoalStep, LegPhase, MediaState, ObservedState,
    ReplayEntry, SettleBarrier,
};
use scenario_harness::realcall::{CallCtx, CallEnv};
use scenario_harness::{
    Agent, CseqOp, CseqOpAt, CseqPattern, DelayedAutomatic, Harness, WaiverScope, ANSWER_SDP,
    OFFER_SDP,
};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

fn parse(raw: &[u8]) -> SipMessage {
    CustomParser::new().parse(raw).unwrap_or_else(|e| panic!("entry did not parse: {e}"))
}

/// The CSeq number of the first request of `method` on the recorded wire.
fn req_cseq(entries: &[sip_net::RecordedSipEntry], method: &str) -> Option<u32> {
    entries.iter().find_map(|e| match parse(&e.raw) {
        SipMessage::Request(r) if r.method() == method => Some(r.cseq().seq()),
        _ => None,
    })
}

/// The `sent_ms` of the first request of `method`.
fn req_sent_ms(entries: &[sip_net::RecordedSipEntry], method: &str) -> Option<u64> {
    entries.iter().find_map(|e| match parse(&e.raw) {
        SipMessage::Request(r) if r.method() == method => Some(e.sent_ms),
        _ => None,
    })
}

/// The `sent_ms` of the first response of `status` whose CSeq echoes `cseq_method`.
fn status_sent_ms(
    entries: &[sip_net::RecordedSipEntry],
    status: u16,
    cseq_method: &str,
) -> Option<u64> {
    entries.iter().find_map(|e| match parse(&e.raw) {
        SipMessage::Response(r) if r.status() == status && r.cseq().method() == cseq_method => {
            Some(e.sent_ms)
        }
        _ => None,
    })
}

/// Whether any recorded response carries `status`.
fn has_status(entries: &[sip_net::RecordedSipEntry], status: u16) -> bool {
    entries.iter().any(|e| matches!(parse(&e.raw), SipMessage::Response(r) if r.status() == status))
}

fn caller(
    role: &'static str,
    agent: &Agent,
    callee: (&'static str, Agent),
    goals: Vec<Goal>,
) -> ActorSpec {
    ActorSpec {
        role,
        agent: agent.clone(),
        disposition: Disposition::Caller,
        media: MediaState::offer(OFFER_SDP),
        goals,
        invite_targets: vec![callee],
        via: None,
        feed: CtxFeed::default(),
        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

fn answering(
    role: &'static str,
    agent: &Agent,
    disposition: Disposition,
    goals: Vec<Goal>,
) -> ActorSpec {
    ActorSpec {
        role,
        agent: agent.clone(),
        disposition,
        media: MediaState::answer(ANSWER_SDP),
        goals,
        invite_targets: vec![],
        via: None,
        feed: CtxFeed::default(),
        cseq: None,
        delayed: vec![],
        claim: None,
    }
}

fn established() -> Vec<scenario_harness::actor::BarrierPhase> {
    vec![phase("established", |s| {
        s.leg_at_least("alice", LegPhase::Confirmed) && s.leg_at_least("bob", LegPhase::Confirmed)
    })]
}

/// Test 5 (ADR-0024 §6): a caller carrying a CSeq deviation pattern on
/// `ActorSpec.cseq` emits the declared number on the wire, driven by the ONE
/// shared step counter attached at the dialog-formation point. A REUSE (not a
/// jump — a jump would leave a §12.2.1.1 gap the settle ledger can never close,
/// since it cannot tell a deliberate skip from dropped in-dialog requests) at
/// the teardown BYE emits the SAME number as the preceding OPTIONS, and the call
/// still settles cleanly (no gap). The Dialog-level fork proof — that a
/// scope-refresh clone shares the counter — lives in the deviations suite.
#[tokio::test(start_paused = true)]
async fn cseq_pattern_via_actorspec_emits_declared_number() {
    let h = Harness::new("actor-cseq-reuse").describe(
        "ActorSpec.cseq reuse: the teardown BYE reuses the OPTIONS's CSeq via the \
         ONE shared counter attached at dialog formation; the call settles clean",
    );
    // The reuse is the §12.2.1.1 violation the audit flags — waive it on alice
    // (the peer replaying the declared out-of-pattern number).
    h.waive(
        WaiverScope::rule("cseq-in-dialog-order", "declared reuse via ActorSpec.cseq")
            .on_party("alice"),
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let actors = vec![
        {
            let mut a = caller(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Options),
                    Goal::new(Barrier::None, GoalStep::Bye),
                ],
            );
            // step 0 (OPTIONS): natural → CSeq 2; step 1 (BYE): reuse → CSeq 2.
            a.cseq =
                Some(CseqPattern { offset: 0, ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }] });
            a
        },
        answering("bob", &bob, Disposition::Answer, vec![]),
    ];
    let call =
        ActorCall::new(actors, established(), SettleBarrier::default_ceiling(), Expect::HappyBye);
    let env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Test", "tok-cseq");
    let ctx = CallCtx::new();
    let res = run_built_actor_call(call, &env, &ctx).await;
    assert!(res.is_ok(), "the deviated call settled clean, got {res:?} notes={:?}", ctx.notes());

    let entries = h.wire_entries();
    assert_eq!(req_cseq(&entries, "OPTIONS"), Some(2), "OPTIONS carries the natural CSeq 2");
    assert_eq!(
        req_cseq(&entries, "BYE"),
        Some(2),
        "the teardown BYE reuses the OPTIONS's CSeq via the shared counter (declared reuse)",
    );
    h.finish().await;
}

/// Test 6 (ADR-0024 §6): a caller carrying `ActorSpec.delayed` holds the
/// automatic ACK-to-2xx for the declared duration — observable as the ACK's
/// send lagging the 200 by ~the delay on the (paused) clock.
#[tokio::test(start_paused = true)]
async fn delayed_automatic_via_actorspec_holds_the_ack() {
    let h = Harness::new("actor-delayed-ack").describe(
        "ActorSpec.delayed: the originated INVITE's automatic ACK-to-2xx is held \
         ~2s (paused clock) — the ACK's send lags the 200 by the declared delay",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let actors = vec![
        {
            let mut a = caller(
                "alice",
                &alice,
                ("bob", bob.clone()),
                vec![
                    Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                    Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
                ],
            );
            a.delayed = vec![DelayedAutomatic::ack_after(2000)];
            a
        },
        answering("bob", &bob, Disposition::Answer, vec![]),
    ];
    let call =
        ActorCall::new(actors, established(), SettleBarrier::default_ceiling(), Expect::HappyBye);
    let env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Test", "tok-delay");
    let ctx = CallCtx::new();
    let res = run_built_actor_call(call, &env, &ctx).await;
    assert!(res.is_ok(), "the delayed-ACK call settled clean, got {res:?}");

    let entries = h.wire_entries();
    let ack = req_sent_ms(&entries, "ACK").expect("an ACK was recorded");
    let ok = status_sent_ms(&entries, 200, "INVITE").expect("a 200/INVITE was recorded");
    assert!(
        ack.saturating_sub(ok) >= 1900,
        "the ACK was held ~2s after the 200 (ActorSpec.delayed honoured): gap {}ms",
        ack.saturating_sub(ok),
    );
    h.finish().await;
}

/// A SCOPED delayed automatic (`on_invite(1)`) holds ONLY the named
/// transaction's ACK: the establishing INVITE (ordinal 0) is ACKed promptly,
/// the re-INVITE (ordinal 1) has its ACK held ~1 s while the peer's 2xx
/// retransmissions inside the hold are ABSORBED (no early ACK, no double ACK),
/// and a 2xx re-surfaced AFTER the hold is re-ACKed promptly (the idempotent
/// §13.2.2.4 re-ACK) — never treated as a stray.
#[tokio::test(start_paused = true)]
async fn scoped_delayed_automatic_holds_only_the_named_reinvite_ack() {
    use std::time::Duration;
    let h = Harness::new("actor-delayed-ack-scoped").describe(
        "ActorSpec.delayed scoped on_invite(1): initial ACK prompt, re-INVITE \
         ACK held ~1s absorbing mid-hold 200 retransmissions, then a post-hold \
         retransmission is re-ACKed — clean teardown",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    // Retransmission IS the subject: drop alice to the raw wire surface so the
    // duplicate 200s reach her reactor (as they do on the SUT-facing lanes,
    // where the SUT retransmits an un-ACKed 2xx) instead of being absorbed by
    // the functional §17.2 receive view.
    alice.wire_view();

    let actors = vec![{
        let mut a = caller(
            "alice",
            &alice,
            ("bob", bob.clone()),
            vec![
                Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                Goal::new(Barrier::AllConfirmed(&["alice"]), GoalStep::Reinvite),
                // Dwell past the whole held-ACK dance (hold fires at ~1.4s,
                // post-hold re-ACK ~1.7s) but inside the peer's 2s receive
                // deadline for the BYE.
                Goal::new(Barrier::None, GoalStep::Bye).after(Duration::from_millis(2500)),
            ],
        );
        a.delayed = vec![DelayedAutomatic::ack_after(1000).on_invite(1)];
        a
    }];
    let call = ActorCall::new(actors, vec![], SettleBarrier::default_ceiling(), Expect::HappyBye);
    let env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Test", "tok-scoped");
    let ctx = CallCtx::new();

    // Hand-rolled peer: the retransmission source the actor lane has no
    // vocabulary for (the SUT/mux owns retransmission in real runs).
    let bob_side = async {
        let mut uas = bob.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
        bob.receive("ACK").await; // ordinal 0: NOT held — prompt (asserted on the wire below)
        let mut reinv = bob.receive("INVITE").await;
        reinv.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
        // Two retransmissions INSIDE the 1 s hold — each must be absorbed; an
        // early or duplicate ACK would surface as a mis-sequenced receive below.
        tokio::time::sleep(Duration::from_millis(300)).await;
        reinv.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        reinv.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
        bob.receive("ACK").await; // the ONE held ACK, after the hold
                                  // Re-surfaced AFTER the hold: the prompt idempotent re-ACK path.
        reinv.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
        bob.receive("ACK").await;
        bob.receive("BYE").await.respond(200, "OK").await;
    };
    let (res, ()) = tokio::join!(run_built_actor_call(call, &env, &ctx), bob_side);
    assert!(res.is_ok(), "the scoped-hold call settled clean, got {res:?} notes={:?}", ctx.notes());

    let entries = h.wire_entries();
    // Per-transaction reads: the ACK reuses its INVITE's CSeq (§13.2.2.4).
    let ack_ms = |cseq: u32| -> Vec<u64> {
        entries
            .iter()
            .filter_map(|e| match parse(&e.raw) {
                SipMessage::Request(r) if r.method() == "ACK" && r.cseq().seq() == cseq => {
                    Some(e.sent_ms)
                }
                _ => None,
            })
            .collect()
    };
    let first_200_ms = |cseq: u32| -> u64 {
        entries
            .iter()
            .find_map(|e| match parse(&e.raw) {
                SipMessage::Response(r)
                    if r.status() == 200
                        && r.cseq().method() == "INVITE"
                        && r.cseq().seq() == cseq =>
                {
                    Some(e.sent_ms)
                }
                _ => None,
            })
            .expect("a 200/INVITE for the transaction was recorded")
    };
    let initial_acks = ack_ms(1);
    assert_eq!(initial_acks.len(), 1, "one ACK to the establishing INVITE");
    assert!(
        initial_acks[0].saturating_sub(first_200_ms(1)) < 500,
        "the UNSCOPED ordinal-0 ACK fires promptly (scoping holds ONLY ordinal 1): gap {}ms",
        initial_acks[0].saturating_sub(first_200_ms(1)),
    );
    let reinvite_acks = ack_ms(2);
    assert_eq!(
        reinvite_acks.len(),
        2,
        "exactly the held ACK + the ONE post-hold re-ACK (mid-hold retransmissions absorbed)",
    );
    let held_gap = reinvite_acks[0].saturating_sub(first_200_ms(2));
    assert!(
        held_gap >= 1000,
        "the re-INVITE ACK was held the declared 1s despite mid-hold 200 retransmissions: gap {held_gap}ms",
    );
    h.finish().await;
}

/// Every mid-hold retransmission of the held final is RECORDED (one replay entry
/// each, so a run states how many the hold provoked) and kept OUT of the leg's
/// response log: the log carries one fact per DISTINCT response, since a
/// duplicate in it would be consumed by a later expectation on the same
/// transaction.
#[tokio::test(start_paused = true)]
async fn mid_hold_retransmissions_are_recorded_and_leave_the_response_log_alone() {
    use std::time::Duration;
    let h = Harness::new("actor-held-final-retransmissions").describe(
        "two 200 retransmissions inside a held ACK-to-2xx: each lands in the \
         replay record as a held-final retransmission, neither enters the leg's \
         response log — clean teardown",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    // Retransmission IS the subject: the raw wire surface, so the duplicates
    // reach alice's reactor instead of the §17.2 receive view.
    alice.wire_view();

    let actors = vec![{
        let mut a = caller(
            "alice",
            &alice,
            ("bob", bob.clone()),
            vec![
                Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                Goal::new(Barrier::AllConfirmed(&["alice"]), GoalStep::Reinvite),
                Goal::new(Barrier::None, GoalStep::Bye).after(Duration::from_millis(2000)),
            ],
        );
        a.delayed = vec![DelayedAutomatic::ack_after(1000).on_invite(1)];
        a
    }];
    let plan = CallPlan {
        actors,
        plan: Vec::new(),
        settle: SettleBarrier::default_ceiling(),
        automatics: Automatics::default(),
        delta_policy: None,
        reception_observer: None,
    };
    let obs = ObservedState::new();
    let ctx = CallCtx::new();

    let bob_side = async {
        let mut uas = bob.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
        bob.receive("ACK").await;
        let mut reinv = bob.receive("INVITE").await;
        reinv.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        reinv.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        reinv.respond(200, "OK").with_sdp(ANSWER_SDP).send().await;
        bob.receive("ACK").await;
        bob.receive("BYE").await.respond(200, "OK").await;
    };
    let (verdict, ()) = tokio::join!(
        run_call_with(plan, obs.clone(), &ctx, Duration::from_secs(10), None),
        bob_side
    );
    assert!(verdict.is_ok(), "the held-ACK call settled clean, got {verdict:?}");

    let retransmitted: Vec<(u16, String, u32)> = obs
        .replay_record()
        .into_iter()
        .filter_map(|e| match e {
            ReplayEntry::HeldFinalRetransmitted { leg, status, cseq_method, cseq } => {
                assert_eq!(leg, "alice", "the holding leg owns the record");
                Some((status, cseq_method, cseq))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        retransmitted,
        vec![(200, "INVITE".to_string(), 2), (200, "INVITE".to_string(), 2)],
        "both mid-hold retransmissions of the re-INVITE 200 are recorded, in arrival order",
    );
    // The response log holds the establishing 200 and the re-INVITE's FIRST 200
    // — the two distinct finals — and neither retransmission.
    let logged_200s = obs.with_snapshot(|s| {
        s.leg("alice")
            .responses()
            .iter()
            .filter(|f| f.status == 200 && f.cseq_method.eq_ignore_ascii_case("INVITE"))
            .count()
    });
    assert_eq!(
        logged_200s, 2,
        "one response fact per DISTINCT 200-to-INVITE (establishing + re-INVITE); \
         a retransmission is absorbed above the log",
    );
    h.finish().await;
}

/// Run one `ActorCall.automatics` case: alice invites, a Scripted bob answers by
/// policy, BYE teardown. Returns whether a `100 Trying` was emitted on the wire.
async fn run_100_case(name: &'static str, on: bool) -> bool {
    let h = Harness::new(name).describe(
        "ActorCall.automatics plumbs onto the CallPlan: answer_100_trying draws \
         (or not) the immediate 100 on the parked INVITE",
    );
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let actors = vec![
        caller(
            "alice",
            &alice,
            ("bob", bob.clone()),
            vec![
                Goal::new(Barrier::None, GoalStep::Invite { callee: "bob", plan: None }),
                Goal::new(Barrier::AllConfirmed(&["alice", "bob"]), GoalStep::Bye),
            ],
        ),
        answering(
            "bob",
            &bob,
            Disposition::Scripted,
            vec![
                Goal::new(Barrier::None, GoalStep::Respond { status: 180 }),
                Goal::new(Barrier::None, GoalStep::Respond { status: 200 }),
            ],
        ),
    ];
    let mut call =
        ActorCall::new(actors, established(), SettleBarrier::default_ceiling(), Expect::HappyBye);
    if on {
        call = call.with_automatics(Automatics { answer_100_trying: true, ..Default::default() });
    }
    let env = CallEnv::for_functional(&alice, &bob, None, bob.addr(), "X-Test", "tok-100");
    let ctx = CallCtx::new();
    let res = run_built_actor_call(call, &env, &ctx).await;
    assert!(res.is_ok(), "[{name}] the scripted call settled clean, got {res:?}");

    let saw = has_status(&h.wire_entries(), 100);
    h.finish().await;
    saw
}

/// Test 8 (ADR-0024 §5/§6): `ActorCall.automatics` reaches the `CallPlan` through
/// `run_built_actor_call` — ON emits the 100 Trying on the parked INVITE, the
/// default (OFF) does not.
#[tokio::test(start_paused = true)]
async fn automatics_via_actorcall_plumbs_to_the_plan() {
    assert!(run_100_case("actor-call-100-on", true).await, "ActorCall.automatics ON → 100 emitted");
    assert!(!run_100_case("actor-call-100-off", false).await, "default automatics → no 100");
}
