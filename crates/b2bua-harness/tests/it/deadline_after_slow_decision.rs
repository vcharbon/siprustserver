//! A service deadline armed in the initial INVITE's turn after a slow
//! decision runs from the instant the decision settled, not from the INVITE:
//! a 3 s deadline behind a 4 s decision round trip fires 7 s after the
//! INVITE, whether the service arms it in its `init` seed or in its handler of
//! the route's `AttemptStarted`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::rules::ServiceDef;
use b2bua_harness::{settle_until, B2buaSut};
use scenario_harness::Harness;
use tokio::time::Instant;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// The decision round trip, longer than the service's deadline.
const DECISION_DELAY: Duration = Duration::from_secs(4);

/// The service's deadline, armed once the decision is applied.
const DEADLINE_MS: i64 = 3_000;

/// The CDR reason the deadline's rule writes.
const FIRED: &str = "deadline_fired";

/// A [`CallDecisionEngine`] answering `new_call` after `delay`.
struct SlowDecisionEngine {
    inner: Arc<dyn CallDecisionEngine>,
    delay: Duration,
}

#[async_trait]
impl CallDecisionEngine for SlowDecisionEngine {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        tokio::time::sleep(self.delay).await;
        self.inner.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
}

/// The rule both services end the call with when their deadline fires: a 480
/// to the caller, the call terminated.
mod fire {
    use b2bua::rules::{RuleAction, RuleHandleResult};
    use call::{CdrEventType, MachineId};

    /// The caller answered 480 and the call ended, `machine` cleared.
    pub fn release(machine: MachineId) -> RuleHandleResult {
        RuleHandleResult::new(vec![
            RuleAction::AddCdrEvent {
                event_type: CdrEventType::Timeout,
                leg_id: "a".into(),
                status_code: Some(480),
                reason: Some(super::FIRED.into()),
            },
            RuleAction::RespondToALeg {
                status: 480,
                reason: "Temporarily Unavailable".into(),
                header_updates: vec![],
                contacts: vec![],
            },
            RuleAction::BeginTermination {
                reason: Some("deadline".into()),
                cause: call::TerminationCause::Timeout(call::TimeoutKind::NoAnswer),
                by_leg: None,
            },
            RuleAction::ClearState { machine },
        ])
    }
}

/// Arms its deadline in its `init` seed.
mod seeded {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, ServiceSeed, Terminal,
        TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{TimerKey, TimerType};

    const DEADLINE: TimerType = TimerType::service(SEEDED, "deadline");

    define_service! {
        id: "seeded",
        machine: SEEDED,
        states: SeededState { Armed },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(SeededState::Armed.label()).with_actions(vec![
                RuleAction::arm(
                    TimerKey::call(TimerType::service(SEEDED, "deadline")),
                    TimerDelay::millis(super::DEADLINE_MS),
                ),
            ]))
        },
        rules: [ on_deadline() ],
    }

    fn on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "seeded-fire",
            machine: SEEDED,
            active: [ SeededState::Armed ],
            transitions: [ SeededState::Armed => Terminal ],
            effects: [
                Effect::Respond { status: 480, label: "deadline passed" },
                Effect::LifecycleCommand { label: "terminate the call" },
            ],
            matcher: Match::timer().timer_type(DEADLINE),
            handle: |_ctx: &RuleContext| Some(super::fire::release(SEEDED)),
        }
    }
}

/// Arms its deadline when the route's attempt starts, at the turn's close.
mod on_attempt {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{TimerKey, TimerType};

    const DEADLINE: TimerType = TimerType::service(ON_ATTEMPT, "deadline");

    define_service! {
        id: "on-attempt",
        machine: ON_ATTEMPT,
        states: OnAttemptState { Idle, Armed },
        init: |_call: &RuleCall| Some(ServiceSeed::new(OnAttemptState::Idle.label())),
        rules: [ on_started(), on_deadline() ],
    }

    fn on_started() -> RuleDefinition {
        sm_rule! {
            id: "on-attempt-arm",
            machine: ON_ATTEMPT,
            active: [ OnAttemptState::Idle ],
            transitions: [ OnAttemptState::Idle => OnAttemptState::Armed ],
            effects: [
                Effect::GuardTimer { timer: DEADLINE, scope: call::TimerScopeKind::Call, label: "arm the deadline" },
            ],
            matcher: Match::attempt_started(),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::arm(
                        TimerKey::call(DEADLINE),
                        TimerDelay::millis(super::DEADLINE_MS),
                    ),
                    RuleAction::SetState { machine: ON_ATTEMPT, to: OnAttemptState::Armed.label() },
                ]))
            },
        }
    }

    fn on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "on-attempt-fire",
            machine: ON_ATTEMPT,
            active: [ OnAttemptState::Armed ],
            transitions: [ OnAttemptState::Armed => Terminal ],
            effects: [
                Effect::Respond { status: 480, label: "deadline passed" },
                Effect::LifecycleCommand { label: "terminate the call" },
            ],
            matcher: Match::timer().timer_type(DEADLINE),
            handle: |_ctx: &RuleContext| Some(super::fire::release(ON_ATTEMPT)),
        }
    }
}

/// A call through `service` behind the slow decision: bob rings and never
/// answers, the deadline answers alice 480, bob's ringing INVITE is CANCELed
/// (RFC 3261 §9.1) and answered 487. Returns how long after her INVITE alice
/// got the 480.
async fn released_after(name: &str, service: ServiceDef, ports: (u16, u16, u16)) -> Duration {
    let h = Harness::new(name);
    let alice = h.agent("alice", &format!("127.0.0.1:{}", ports.0)).await;
    let bob = h.agent("bob", &format!("127.0.0.1:{}", ports.1)).await;
    let decision = Arc::new(SlowDecisionEngine {
        inner: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", ports.1)),
        delay: DECISION_DELAY,
    });
    let b2bua = B2buaSut::builder(decision)
        .services(vec![service])
        .tune(|c| {
            c.call_control_timeout_ms = 20_000;
            c.keepalive_interval_sec = 3_600;
        })
        .start(&h, "b2bua", &format!("127.0.0.1:{}", ports.2))
        .await;

    let invited = Instant::now();
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    call.expect(480).await;
    let released = invited.elapsed();

    bob.receive("CANCEL").await.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    let reasons: Vec<String> =
        b2bua.cdr_records()[0].events.iter().filter_map(|e| e.reason.clone()).collect();
    assert!(reasons.iter().any(|r| r == FIRED), "the deadline ended the call: {reasons:?}");
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
    released
}

/// The window the 480 lands in: the deadline after the decision, within one
/// network transit.
fn assert_runs_from_the_decision(released: Duration) {
    let due = DECISION_DELAY + Duration::from_millis(DEADLINE_MS as u64);
    assert!(
        released >= due && released < due + Duration::from_millis(500),
        "the 480 left {released:?} after the INVITE; the deadline runs from the decision, due at {due:?}",
    );
}

#[tokio::test(start_paused = true)]
async fn a_seeded_deadline_runs_from_the_decision() {
    let released =
        released_after("deadline-seeded-slow-decision", seeded::service_def(), (5063, 5073, 5083))
            .await;
    assert_runs_from_the_decision(released);
}

#[tokio::test(start_paused = true)]
async fn an_attempt_started_deadline_runs_from_the_decision() {
    let released = released_after(
        "deadline-attempt-slow-decision",
        on_attempt::service_def(),
        (5064, 5074, 5084),
    )
    .await;
    assert_runs_from_the_decision(released);
}
