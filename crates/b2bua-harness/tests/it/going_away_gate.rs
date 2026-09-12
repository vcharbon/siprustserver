//! The executor's going-away gate, end to end.
//!
//! A call already going away makes no forward progress on its own clock: a
//! service watchdog that fires on a `Terminating` call reaches only teardown
//! rules (`RuleDefinition::teardown`); every other rule it matches is absorbed
//! and counted (`going_away_absorbed`). Termination already disarms every
//! watchdog armed before it, so the probe arms its deadline INSIDE the
//! terminating window — on the callee's `200 (CANCEL)` — where only the gate
//! stands between the fire and a guard-less rule's second final.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{invite_final_statuses, settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// Three machine-less rules: one arms a deadline on the callee's `200
/// (CANCEL)`; on that deadline a guard-less rule would answer the caller and
/// tear down, and a teardown rule books its passing on the CDR.
mod lategate {
    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef,
        ServiceSeed, TimerDelay, SERVICE_LAYER,
    };
    use call::{CdrEventType, Direction, MachineId, TimerType};

    const LATEGATE: MachineId = MachineId::new("lategate");
    const DEADLINE: TimerType = TimerType::service(LATEGATE, "late");
    pub const DEADLINE_SEC: i64 = 1;
    pub const CDR_SCRUBBED: &str = "lategate:scrubbed";

    fn arm_on_cancel_200(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![RuleAction::ScheduleTimer {
            timer_type: DEADLINE,
            delay: TimerDelay::secs(DEADLINE_SEC),
            leg_id: None,
        }]))
    }

    fn reject_on_deadline(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![
            RuleAction::RespondToALeg {
                status: 480,
                reason: "Temporarily Unavailable".into(),
                header_updates: vec![],
                contacts: vec![],
            },
            RuleAction::BeginTermination { reason: Some("lategate".into()) },
        ]))
    }

    fn scrub_on_deadline(_ctx: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![RuleAction::AddCdrEvent {
            event_type: CdrEventType::Timeout,
            leg_id: "a".into(),
            status_code: None,
            reason: Some(CDR_SCRUBBED.into()),
        }]))
    }

    fn stateless(
        id: &'static str,
        matcher: Match,
        handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    ) -> RuleDefinition {
        RuleDefinition {
            id,
            layer: SERVICE_LAYER,
            overrides: &[],
            matcher,
            handle,
            machine: None,
            active_states: &[],
            transitions: &[],
            effects: &[],
            teardown: false,
        }
    }

    fn rules() -> Vec<RuleDefinition> {
        vec![
            stateless("lategate-reject", Match::timer().timer_type(DEADLINE), reject_on_deadline),
            stateless("lategate-scrub", Match::timer().timer_type(DEADLINE), scrub_on_deadline)
                .runs_while_terminating(),
            stateless(
                "lategate-arm",
                Match::response().method("CANCEL").status_class(2).direction(Direction::FromB),
                arm_on_cancel_200,
            ),
        ]
    }

    fn init(_call: &RuleCall) -> Option<ServiceSeed> {
        None
    }

    pub fn service_def() -> ServiceDef {
        ServiceDef { id: "lategate", init, rules }
    }
}

fn decision() -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| NewCallResponse::Route(route_to("127.0.0.1", 5070)))
            .build(),
    )
}

/// Caller CANCELs mid-ring; the callee 200s the CANCEL and withholds its 487
/// past the deadline armed on that 200: the guard-less rule is absorbed and
/// counted, the teardown rule runs, nothing reaches the caller, and the
/// withheld 487 resolves the call normally.
#[tokio::test(start_paused = true)]
async fn a_watchdog_fired_inside_the_terminating_window_reaches_only_teardown_rules() {
    let h = Harness::new("going-away-gate");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::builder(decision())
        .services(vec![lategate::service_def()])
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
    // Bob 200s the CANCEL — the probe arms its deadline on the now-terminating
    // call — and WITHHOLDS the 487 across it.
    bob.receive("CANCEL").await.respond(200, "OK").await;

    // ── cross the deadline inside the terminating window ─────────────────────
    h.advance(Duration::from_secs(lategate::DEADLINE_SEC as u64 + 1)).await;
    settle_until(|| b2bua.metrics().going_away_absorbed_total() == 1).await;
    assert!(
        alice.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "nothing may reach the caller after the 487",
    );
    assert_eq!(
        b2bua.metrics().second_final_refused_total(),
        0,
        "the guard-less rule never ran, so no final reached the a-leg seam",
    );

    // ── bob's withheld 487 resolves the cancelled b-leg; the call finalizes ──
    b_inv.respond(487, "Request Terminated").await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();
    assert_eq!(b2bua.metrics().going_away_absorbed_total(), 1, "counted exactly once");

    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    let reasons: Vec<&str> = cdrs[0].events.iter().filter_map(|e| e.reason.as_deref()).collect();
    assert!(
        reasons.contains(&lategate::CDR_SCRUBBED),
        "the teardown rule ran on the fire: {reasons:?}",
    );

    let report = h.finish().await;
    assert_eq!(
        invite_final_statuses(&report, alice.addr()),
        vec![487],
        "the a-leg INVITE transaction carries exactly one final: the 487",
    );
}
