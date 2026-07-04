//! Service-owned per-call timers (`TimerType::Service`) end-to-end — the
//! ADR-0016 watchdog seam that unblocks the Routing API's `callTimers.timer18x`
//! downstream (newkahneed 007).
//!
//! A test-only **ringwatch** service models the real 18x deadline: `init` arms
//! `Service{ringwatch, timer18x}`; a rule disarms it on the first 18x
//! (`RuleAction::cancel_timer` — the id-recipe symmetric cancel); another rule
//! catches ONLY its own `(service_id, key)` firing and reaps the still-silent
//! call with a 480. A second **dualkeys** service pins key identity: two keys
//! coexist as distinct timers, a `Match::service_timers` wildcard rule branches
//! on `ctx.service_timer_key()`, and a rule-driven re-arm of the same key
//! supersedes instead of duplicating.

use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::ScriptedDecisionEngine;
use b2bua_harness::{establish, settle_until, B2buaSut};
use scenario_harness::Harness;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

fn reasons_of(cdr: &b2bua::cdr::CdrRecord) -> Vec<String> {
    cdr.events.iter().filter_map(|e| e.reason.clone()).collect()
}

/// The 18x watchdog service — the exact downstream `timer18x` shape.
mod ringwatch {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, RuleCall,
        ServiceSeed, Terminal,
    };
    use b2bua::{define_service, sm_rule};
    use call::{CdrEventType, Direction, LegState, TimerType};

    pub const DEADLINE_SEC: i64 = 5;
    const T18X: TimerType = TimerType::service(RINGWATCH, "timer18x");

    define_service! {
        id: "ringwatch",
        machine: RINGWATCH,
        states: RwState { Armed, Disarmed },
        // Arm the per-call 18x deadline at call setup (in production the delay
        // comes from the decision envelope's callTimers.timer18x).
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(RwState::Armed.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(RINGWATCH, "timer18x"),
                    delay_sec: DEADLINE_SEC,
                    leg_id: None,
                },
            ]))
        },
        rules: [ disarm_on_18x(), reap_on_deadline() ],
    }

    /// First 18x arrived in time → disarm the watchdog. First-`Some` wins, so
    /// this rule also owns the provisional relay while Armed (the same actions
    /// CORE `relay-provisional` emits), plus the cancel + cursor move.
    fn disarm_on_18x() -> RuleDefinition {
        sm_rule! {
            id: "ringwatch-disarm",
            machine: RINGWATCH,
            active: [ RwState::Armed ],
            transitions: [ RwState::Armed => RwState::Disarmed ],
            effects: [
                Effect::Relay { label: "relay the 18x onward" },
                Effect::GuardTimer { timer: T18X, label: "disarm timer18x" },
            ],
            matcher: Match::response().method("INVITE").status_class(1).direction(Direction::FromB),
            handle: |ctx: &RuleContext| {
                let b = ctx.source_leg_id.to_string();
                let status = ctx.response().map(|r| r.status as i64);
                Some(RuleHandleResult::new(vec![
                    // The symmetric disarm: id minted from the ONE recipe.
                    RuleAction::cancel_timer(&T18X, None),
                    RuleAction::UpdateLegState {
                        leg_id: b.clone(),
                        state: LegState::Early,
                        disposition: None,
                    },
                    RuleAction::RelayToPeer { transform: Default::default() },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Provisional,
                        leg_id: b,
                        status_code: status,
                        reason: None,
                    },
                    RuleAction::SetState { machine: RINGWATCH, to: RwState::Disarmed.label() },
                ]))
            },
        }
    }

    /// The deadline passed with no 18x → 480 the caller and tear down. Matches
    /// ONLY this service's own `(service_id, key)` — exact-key `timer_type`.
    fn reap_on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "ringwatch-fire",
            machine: RINGWATCH,
            active: [ RwState::Armed ],
            transitions: [ RwState::Armed => Terminal ],
            effects: [
                Effect::Respond { status: 480, label: "no 18x before the deadline" },
                Effect::LifecycleCommand { label: "terminate the silent call" },
            ],
            matcher: Match::timer().timer_type(TimerType::service(RINGWATCH, "timer18x")),
            handle: |ctx: &RuleContext| {
                // The fired event carries the key intact.
                let (sid, key) = ctx.service_timer_key().expect("service timer fire");
                assert_eq!(sid.as_str(), "ringwatch");
                assert_eq!(key, "timer18x");
                Some(RuleHandleResult::new(vec![
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Timeout,
                        leg_id: "a".into(),
                        status_code: Some(480),
                        reason: Some("ringwatch_18x_deadline".into()),
                    },
                    RuleAction::RespondToALeg {
                        status: 480,
                        reason: "Temporarily Unavailable".into(),
                        header_updates: vec![],
                        contacts: vec![],
                    },
                    RuleAction::BeginTermination { reason: Some("ringwatch-18x".into()) },
                    RuleAction::ClearState { machine: RINGWATCH },
                ]))
            },
        }
    }
}

/// Key-identity probe: two keys, one wildcard rule, a same-key re-arm.
mod dualkeys {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, RuleCall,
        ServiceSeed, Terminal,
    };
    use b2bua::{define_service, sm_rule};
    use call::{CdrEventType, TimerType};

    define_service! {
        id: "dualkeys",
        machine: DUALKEYS,
        states: DkState { Watching },
        // Two distinct keys → two distinct timers (distinct persisted ids).
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(DkState::Watching.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(DUALKEYS, "fast"),
                    delay_sec: 3,
                    leg_id: None,
                },
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(DUALKEYS, "slow"),
                    delay_sec: 6,
                    leg_id: None,
                },
            ]))
        },
        rules: [ on_any_key() ],
    }

    /// One wildcard rule for ALL of this service's keys (`service_timers`),
    /// branching on `ctx.service_timer_key()`. "fast" marks a CDR event and
    /// re-arms ITSELF once more (+2 s — same key ⇒ supersede, not duplicate);
    /// "slow" marks and terminates.
    fn on_any_key() -> RuleDefinition {
        sm_rule! {
            id: "dualkeys-any",
            machine: DUALKEYS,
            active: [ DkState::Watching ],
            transitions: [ DkState::Watching => Terminal ],
            effects: [
                Effect::GuardTimer {
                    timer: TimerType::service(DUALKEYS, "fast"),
                    label: "re-arm fast",
                },
                Effect::LifecycleCommand { label: "terminate on slow" },
            ],
            matcher: Match::timer().service_timers(DUALKEYS),
            handle: |ctx: &RuleContext| {
                let (_, key) = ctx.service_timer_key().expect("service timer fire");
                match key {
                    "fast" => Some(RuleHandleResult::new(vec![
                        RuleAction::AddCdrEvent {
                            event_type: CdrEventType::Timeout,
                            leg_id: "a".into(),
                            status_code: None,
                            reason: Some("dual_fast".into()),
                        },
                        RuleAction::ScheduleTimer {
                            timer_type: TimerType::service(DUALKEYS, "fast"),
                            delay_sec: 2,
                            leg_id: None,
                        },
                    ])),
                    "slow" => Some(RuleHandleResult::new(vec![
                        RuleAction::AddCdrEvent {
                            event_type: CdrEventType::Timeout,
                            leg_id: "a".into(),
                            status_code: None,
                            reason: Some("dual_slow".into()),
                        },
                        RuleAction::BeginTermination { reason: Some("dualkeys-slow".into()) },
                        RuleAction::ClearState { machine: DUALKEYS },
                    ])),
                    other => panic!("unexpected service timer key: {other}"),
                }
            },
        }
    }
}

/// Fire path: bob never sends a provisional → the service's own timer fires at
/// its deadline, the exact-key rule catches it, the caller gets the 480 and the
/// b-leg is CANCELled — a service watchdog with zero core involvement.
#[tokio::test(start_paused = true)]
async fn service_timer_fires_and_owning_rule_reaps_the_silent_call() {
    let h = Harness::new("service-timer-fires");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![ringwatch::service_def()])
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    // Bob stays silent — no 18x, ever.

    // Cross the 5 s watchdog deadline (far below the 150 s core SetupTimeout).
    h.advance(Duration::from_secs(ringwatch::DEADLINE_SEC as u64 + 1)).await;

    // The service rule answered the caller and tore the call down. Bob never
    // sent a provisional, so deterministic Timer-A INVITE retransmits (+0.5 s,
    // +1.5 s, +3.5 s) queued ahead of the CANCEL — same transaction, bob keeps
    // ignoring them (a 200 here would fork unACKed dialogs and fail the audit).
    for _ in 0..3 {
        let _dup = bob.receive("INVITE").await;
    }
    let mut cancel = bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    let final_resp = call.expect(480).await;
    assert_eq!(final_resp.status, 480, "service watchdog authored the caller's final");

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    let cdrs = b2bua.cdr_records();
    assert!(
        reasons_of(&cdrs[0]).iter().any(|r| r == "ringwatch_18x_deadline"),
        "CDR carries the service's own marker: {:?}",
        reasons_of(&cdrs[0]),
    );
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// Cancel path: the 18x arrives BEFORE the deadline → the service disarms its
/// watchdog (`RuleAction::cancel_timer`); advancing far past the deadline must
/// NOT fire it — the call answers and lives on.
#[tokio::test(start_paused = true)]
async fn cancelled_service_timer_does_not_fire_after_the_18x() {
    let h = Harness::new("service-timer-cancelled");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![ringwatch::service_def()])
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;

    // Ring immediately — well inside the 5 s deadline (and before any INVITE
    // retransmit). The service rule relays the 180 AND disarms its watchdog.
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Cross the (disarmed) deadline, keep ringing well past it.
    h.advance(Duration::from_secs(10)).await;

    // Still alive: bob answers, the call establishes normally.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "call survives past the disarmed deadline",
    );

    // Clean teardown; the CDR must NOT carry the watchdog marker.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    assert!(
        !reasons_of(&b2bua.cdr_records()[0]).iter().any(|r| r.contains("ringwatch")),
        "disarmed watchdog left no trace",
    );
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// Key identity: "fast" (3 s, re-armed once to 5 s) and "slow" (6 s) coexist as
/// DISTINCT timers; the wildcard rule receives each fire with its key intact,
/// and the same-key re-arm supersedes (exactly two "fast" fires, never a
/// duplicate pair from the re-arm).
#[tokio::test(start_paused = true)]
async fn two_keys_fire_independently_and_same_key_rearm_supersedes() {
    let h = Harness::new("service-timer-dual-keys");
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5072));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![dualkeys::service_def()])
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5082")
        .await;

    let dialog = establish(&alice, &bob, b2bua.addr).await;
    let _ = dialog;

    // fast@3s, fast-rearm@5s, slow@6s → the slow branch tears the call down.
    h.advance(Duration::from_secs(7)).await;
    alice.receive("BYE").await.respond(200, "OK").await;
    bob.receive("BYE").await.respond(200, "OK").await;

    settle_until(|| b2bua.cdr_records().len() == 1).await;
    let cdrs = b2bua.cdr_records();
    let reasons = reasons_of(&cdrs[0]);
    let fasts = reasons.iter().filter(|r| *r == "dual_fast").count();
    let slows = reasons.iter().filter(|r| *r == "dual_slow").count();
    assert_eq!(fasts, 2, "fast fired at 3 s and (re-armed, superseded) at 5 s: {reasons:?}");
    assert_eq!(slows, 1, "slow fired once at its own deadline: {reasons:?}");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
