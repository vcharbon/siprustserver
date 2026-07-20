//! ADR-0024 §6: deviations and lane automatics that ride the ACTOR PLAN — the
//! `ActorSpec.cseq` shared counter, the `ActorSpec.delayed` automatic, and the
//! `ActorCall.automatics` carrier plumbed onto the `CallPlan`. Each drives a
//! full SUT-less actor call through `run_built_actor_call` over the recording-
//! wrapped simulated network, then asserts the effect on the recorded wire.

use scenario_harness::actor::{
    phase, run_built_actor_call, ActorCall, ActorSpec, Automatics, Barrier, CtxFeed, Disposition,
    Expect, Goal, GoalStep, LegPhase, MediaState, SettleBarrier,
};
use scenario_harness::realcall::{CallCtx, CallEnv};
use scenario_harness::{
    Agent, CseqOp, CseqOpAt, CseqPattern, DelayedAutomatic, Harness, WaiverScope, ANSWER_SDP,
    OFFER_SDP,
};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

fn parse(raw: &[u8]) -> SipMessage {
    CustomParser::new()
        .parse(raw)
        .unwrap_or_else(|e| panic!("entry did not parse: {e}"))
}

/// The CSeq number of the first request of `method` on the recorded wire.
fn req_cseq(entries: &[sip_net::RecordedSipEntry], method: &str) -> Option<u32> {
    entries.iter().find_map(|e| match parse(&e.raw) {
        SipMessage::Request(r) if r.method == method => Some(r.cseq.seq),
        _ => None,
    })
}

/// The `sent_ms` of the first request of `method`.
fn req_sent_ms(entries: &[sip_net::RecordedSipEntry], method: &str) -> Option<u64> {
    entries.iter().find_map(|e| match parse(&e.raw) {
        SipMessage::Request(r) if r.method == method => Some(e.sent_ms),
        _ => None,
    })
}

/// The `sent_ms` of the first response of `status` whose CSeq echoes `cseq_method`.
fn status_sent_ms(entries: &[sip_net::RecordedSipEntry], status: u16, cseq_method: &str) -> Option<u64> {
    entries.iter().find_map(|e| match parse(&e.raw) {
        SipMessage::Response(r) if r.status == status && r.cseq.method == cseq_method => {
            Some(e.sent_ms)
        }
        _ => None,
    })
}

/// Whether any recorded response carries `status`.
fn has_status(entries: &[sip_net::RecordedSipEntry], status: u16) -> bool {
    entries.iter().any(|e| matches!(parse(&e.raw), SipMessage::Response(r) if r.status == status))
}

fn caller(role: &'static str, agent: &Agent, callee: (&'static str, Agent), goals: Vec<Goal>) -> ActorSpec {
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
        delayed: None,
    }
}

fn answering(role: &'static str, agent: &Agent, disposition: Disposition, goals: Vec<Goal>) -> ActorSpec {
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
        delayed: None,
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
        WaiverScope::rule("rfc3261.cseqInDialogOrder", "declared reuse via ActorSpec.cseq")
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
            a.cseq = Some(CseqPattern {
                offset: 0,
                ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }],
            });
            a
        },
        answering("bob", &bob, Disposition::Answer, vec![]),
    ];
    let call = ActorCall::new(actors, established(), SettleBarrier::default_ceiling(), Expect::HappyBye);
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
            a.delayed = Some(DelayedAutomatic::ack_after(2000));
            a
        },
        answering("bob", &bob, Disposition::Answer, vec![]),
    ];
    let call = ActorCall::new(actors, established(), SettleBarrier::default_ceiling(), Expect::HappyBye);
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
        call = call.with_automatics(Automatics { answer_100_trying: true });
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
