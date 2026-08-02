//! The `no-answer` timer on an ALREADY-ANSWERED call (upstreamneed-059).
//!
//! A `kill_worker` reclaim can restore a stale per-b-leg `NoAnswer` ledger
//! entry whose cancel died with the crashed node; pre-fix its fire tore a
//! CONFIRMED call down with a 480 (~107 established-call drops per endurance
//! kill event). The rule must absorb the fire when the call is answered —
//! only the spent ledger entry is scrubbed; no 480, no leg teardown, no
//! `/call/failure` consult.
//!
//! The stale entry is re-materialised by a probe service that re-arms
//! `TimerType::NoAnswer` on the confirmed call — the same ledger shape a
//! reboot reclaim restores — so the fire reaches the CORE `no-answer` rule
//! exactly as it does after a real reclaim.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{establish, hangup, settle_until, B2buaSut};
use scenario_harness::Harness;
use std::time::Duration;

/// The route-supplied ring deadline — armed at route time, cancelled at answer.
const NO_ANSWER_SEC: i64 = 15;

/// Probe service: re-materialises the reclaim-restored stale `NoAnswer` ledger
/// entry on the (by then) confirmed call.
mod stalerestore {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal,
    };
    use b2bua::{define_service, sm_rule};
    use call::TimerType;

    /// When the probe injects the stale entry (well after the answer and after
    /// the real, cancelled route-time `NoAnswer` deadline has passed).
    pub const INJECT_AT_SEC: i64 = 30;
    /// The stale entry's own remaining delay after injection.
    pub const STALE_FIRE_SEC: i64 = 5;

    define_service! {
        id: "stalerestore",
        machine: STALERESTORE,
        states: SrState { Waiting },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(SrState::Waiting.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(STALERESTORE, "inject"),
                    delay_sec: INJECT_AT_SEC,
                    leg_id: None,
                },
            ]))
        },
        rules: [ inject_stale_entry() ],
    }

    /// Re-arms the b-leg's `NoAnswer` on the confirmed call — the ledger shape
    /// a reboot reclaim restores when the entry's cancel died with the node.
    fn inject_stale_entry() -> RuleDefinition {
        sm_rule! {
            id: "stalerestore-inject",
            machine: STALERESTORE,
            active: [ SrState::Waiting ],
            transitions: [ SrState::Waiting => Terminal ],
            effects: [
                Effect::GuardTimer {
                    timer: TimerType::NoAnswer,
                    label: "re-arm the stale NoAnswer entry",
                },
            ],
            matcher: Match::timer().timer_type(TimerType::service(STALERESTORE, "inject")),
            handle: |ctx: &RuleContext| {
                let b = ctx
                    .call
                    .b_legs()
                    .first()
                    .expect("established call has a b-leg")
                    .leg_id
                    .clone();
                Some(RuleHandleResult::new(vec![
                    RuleAction::ScheduleTimer {
                        timer_type: TimerType::NoAnswer,
                        delay_sec: STALE_FIRE_SEC,
                        leg_id: Some(b),
                    },
                    RuleAction::ClearState { machine: STALERESTORE },
                ]))
            },
        }
    }
}

fn reasons_of(cdr: &b2bua::cdr::CdrRecord) -> Vec<String> {
    cdr.events.iter().filter_map(|e| e.reason.clone()).collect()
}

/// A stale `NoAnswer` fire on a CONFIRMED call is absorbed: the call survives
/// (no 480, no BYE, no teardown), lives past the fire, and hangs up normally.
#[tokio::test(start_paused = true)]
async fn stale_no_answer_fire_on_a_confirmed_call_is_absorbed() {
    let h = Harness::new("no-answer-absorb");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    // The route supplies a real ring deadline, so the live path arms (and the
    // answer cancels) a genuine per-b-leg NoAnswer before the probe re-arms it.
    let decision: Arc<dyn CallDecisionEngine> = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.no_answer_timeout_sec = Some(NO_ANSWER_SEC);
                NewCallResponse::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .services(vec![stalerestore::service_def()])
        // Keepalive pushed out so the advances exercise ONLY the probe's inject
        // timer and the stale NoAnswer fire.
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut dialog = establish(&alice, &bob, b2bua.addr).await;
    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "one confirmed call up",
    );

    // Cross ONLY the probe's inject deadline (30 s; the route-time NoAnswer at
    // 15 s was cancelled by the answer): the stale entry is re-armed for +5 s.
    h.advance(Duration::from_secs(stalerestore::INJECT_AT_SEC as u64 + 1)).await;

    // Cross ONLY the stale NoAnswer deadline. Pre-fix this 480'd the answered
    // INVITE and tore the call down; the guard must absorb it.
    h.advance(Duration::from_secs(stalerestore::STALE_FIRE_SEC as u64 + 1)).await;

    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "confirmed call survives the stale NoAnswer fire",
    );

    // The surviving dialog terminates normally: BYE end-to-end.
    hangup(&mut dialog, &bob).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    // The CDR carries no trace of the absorbed fire.
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "exactly one CDR");
    assert!(
        !reasons_of(&cdrs[0]).iter().any(|r| r.contains("no_answer") || r.contains("no-answer")),
        "no no-answer marker on the absorbed fire: {:?}",
        reasons_of(&cdrs[0]),
    );
    assert!(
        !cdrs[0].events.iter().any(|e| e.event_type == call::CdrEventType::Timeout),
        "no timeout CDR event for the absorbed fire",
    );

    let _report = h.finish().await;
}
