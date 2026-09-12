//! A second final on the a-leg's initial INVITE is refused in the call layer.
//!
//! The transaction carries one final (RFC 3261 §17.2.1). The caller's CANCEL
//! makes sip-txn author that final (487) itself, and the a-leg stays `Early`
//! for the whole terminating window. Termination disarms every service
//! watchdog and the executor absorbs a timer on a going-away call, so the one
//! shape that still reaches the a-leg response seam is a rule acting on a
//! peer's message during the teardown: a guard-less rule answers the cancelled
//! caller on the callee's `200 (CANCEL)`. The `Cancelled` turn recorded the 487
//! on the leg, so the seam refuses that 480 — never built, never on the wire —
//! and the refusal is counted (`second_final_refused`). The control run fires
//! a service deadline on a live call: the 480 reaches the caller and nothing
//! is counted.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// A service whose deadline answers the caller with no going-away guard — on a
/// live call the ordinary first final; on a cancelled call the watchdog is
/// disarmed at termination and never fires.
mod latereject {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::TimerType;

    pub const DEADLINE_SEC: i64 = 8;
    const DEADLINE: TimerType = TimerType::service(LATEREJECT, "deadline");

    define_service! {
        id: "latereject",
        machine: LATEREJECT,
        states: LrState { Armed },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(LrState::Armed.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(LATEREJECT, "deadline"),
                    delay: TimerDelay::secs(DEADLINE_SEC),
                    leg_id: None,
                },
            ]))
        },
        rules: [ reject_on_deadline() ],
    }

    fn reject_on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "latereject-fire",
            machine: LATEREJECT,
            active: [ LrState::Armed ],
            transitions: [ LrState::Armed => Terminal ],
            effects: [
                Effect::Respond { status: 480, label: "answer the caller, guard-less" },
                Effect::LifecycleCommand { label: "tear the call down" },
            ],
            matcher: Match::timer().timer_type(DEADLINE),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::RespondToALeg {
                        status: 480,
                        reason: "Temporarily Unavailable".into(),
                        header_updates: vec![],
                        contacts: vec![],
                    },
                    RuleAction::BeginTermination { reason: Some("latereject".into()) },
                    RuleAction::ClearState { machine: LATEREJECT },
                ]))
            },
        }
    }
}

/// A machine-less service rule that answers the caller on the callee's
/// `200 (CANCEL)` — a peer's message, which no gate absorbs — with no
/// going-away guard: the misbehaving-rule shape the a-leg seam must hold
/// against once the caller's transaction already carries its 487.
mod lateanswer {
    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef,
        ServiceSeed, SERVICE_LAYER,
    };
    use call::Direction;

    fn answer_on_cancel_200(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![RuleAction::RespondToALeg {
            status: 480,
            reason: "Temporarily Unavailable".into(),
            header_updates: vec![],
            contacts: vec![],
        }]))
    }

    fn rules() -> Vec<RuleDefinition> {
        vec![RuleDefinition {
            id: "lateanswer-on-cancel-200",
            layer: SERVICE_LAYER,
            overrides: &[],
            matcher: Match::response().method("CANCEL").status_class(2).direction(Direction::FromB),
            handle: answer_on_cancel_200,
            machine: None,
            active_states: &[],
            transitions: &[],
            effects: &[],
            teardown: false,
        }]
    }

    fn init(_call: &RuleCall) -> Option<ServiceSeed> {
        None
    }

    pub fn service_def() -> ServiceDef {
        ServiceDef { id: "lateanswer", init, rules }
    }
}

fn decision() -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    )
}

/// Caller CANCELs mid-ring, the callee 200s the CANCEL and withholds its 487
/// across the service deadline: the guard-less rule's 480 on that 200 is
/// refused, counted once, and never reaches the caller; the deadline itself
/// was disarmed at termination and never fires; the withheld 487 then
/// resolves the call normally.
#[tokio::test(start_paused = true)]
async fn a_service_final_on_a_cancelled_call_is_refused_and_counted() {
    let h = Harness::new("second-final-refused");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![latereject::service_def(), lateanswer::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // ── alice INVITEs, bob rings ─────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    // ── the caller gives up: 200 + 487 from the txn layer, the ONE final ─────
    h.advance(Duration::from_secs(1)).await;
    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    // The 180 let the b-leg CANCEL go at once; bob 200s it — the message the
    // guard-less rule answers the caller on — and WITHHOLDS the 487, so the
    // call sits Terminating with a Cancelling leg across the deadline.
    bob.receive("CANCEL").await.respond(200, "OK").await;
    settle_until(|| b2bua.metrics().second_final_refused_total() == 1).await;

    // ── cross the service deadline on the cancelled call ─────────────────────
    h.advance(Duration::from_secs(latereject::DEADLINE_SEC as u64)).await;
    assert!(
        alice.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "nothing may reach the caller after the 487",
    );
    assert_eq!(
        b2bua.metrics().second_final_refused_total(),
        1,
        "the rule's final was refused against the 487 and counted once",
    );
    assert_eq!(
        b2bua.metrics().going_away_absorbed_total(),
        0,
        "the deadline was disarmed at termination: nothing fired to absorb",
    );

    // ── bob's withheld 487 resolves the cancelled b-leg; the call finalizes ──
    b_inv.respond(487, "Request Terminated").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.metrics().second_final_refused_total(), 1, "no further refusal at teardown");

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    assert!(
        cdrs[0].events.iter().any(|e| e.event_type == call::CdrEventType::Cancel),
        "the caller's CANCEL is on the CDR: {:?}",
        cdrs[0].events,
    );

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
}

/// Control: the same deadline on a live ringing call answers the caller —
/// the seam admits a transaction's first final and counts nothing.
#[tokio::test(start_paused = true)]
async fn the_first_final_on_a_live_call_is_admitted() {
    let h = Harness::new("second-final-refused-control");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![latereject::service_def()])
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut b_inv = bob.receive("INVITE").await;
    b_inv.respond(180, "Ringing").await;
    call.expect(180).await;

    // ── the deadline answers the still-ringing caller and tears down ─────────
    h.advance(Duration::from_secs(latereject::DEADLINE_SEC as u64 + 1)).await;
    call.expect(480).await;
    bob.receive("CANCEL").await.respond(200, "OK").await;
    b_inv.respond(487, "Request Terminated").await;
    assert_eq!(b2bua.metrics().second_final_refused_total(), 0, "a first final is not a refusal");

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![480],
        "the a-leg INVITE transaction carries exactly one final: the 480",
    );
}
