//! A timer lives as long as the scope it guards, and a fire of an instance
//! the ledger no longer holds reaches no rule.
//!
//! - A callee leg that ends takes its ring deadline with it, in the turn that
//!   ends it: a refused leg's `NoAnswer` and a fired one alike leave the
//!   ledger while the failure consult is awaited.
//! - A fire already handed to the router when its timer was re-armed is
//!   stale: the core's call-duration cap re-armed at the answer, and a
//!   service's deadline re-armed by its own rule, each run on the instance
//!   the ledger holds.
//!
//! The stale fires are posted with [`B2buaSut::post_event`], naming the
//! generation the replaced instance was armed under: the driver drops a
//! superseded instance it still queues, so the fire under test is one that
//! left the driver before the turn that re-armed it ran.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse, CallTreatment,
    NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::CallEvent;
use b2bua_harness::{settle_until, B2buaSut};
use call::{Call, TimerKey, TimerType};
use scenario_harness::{Agent, ClientInvite, Harness};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=carol 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const FROM_TAG: &str = "alice-tag";

/// How long the failure consult takes to answer: the window in which the
/// ended leg's timers are read.
const FAILURE_CONSULT: Duration = Duration::from_secs(2);

/// A decision engine whose `/call/failure` answers after [`FAILURE_CONSULT`].
struct SlowFailure(ScriptedDecisionEngine);

#[async_trait]
impl CallDecisionEngine for SlowFailure {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.0.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        tokio::time::sleep(FAILURE_CONSULT).await;
        self.0.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.0.call_refer(req).await
    }
    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        self.0.call_release(req).await
    }
}

/// The initial route toward `bob` with a ring deadline of `no_answer_sec` and a
/// callback context, whose failure fails over to `carol`.
fn failing_over(bob: u16, carol: u16, no_answer_sec: i64) -> Arc<SlowFailure> {
    Arc::new(SlowFailure(
        ScriptedDecisionEngine::builder()
            .fallback(move |_| {
                let mut r = route_to("127.0.0.1", bob);
                r.callback_context = Some("timer-scope".into());
                r.no_answer_timeout_sec = Some(no_answer_sec);
                NewCallResponse::Route(r)
            })
            .on_failure(move |_| {
                let mut r = route_to("127.0.0.1", carol);
                r.no_answer_timeout_sec = Some(30);
                CallTreatment::Route(r)
            })
            .build(),
    ))
}

/// The armed timers of `call` that name leg `leg`.
fn timers_on_leg(call: &Call, leg: &str) -> Vec<String> {
    call.timers.iter().filter(|t| t.leg_id() == Some(leg)).map(|t| format!("{:?}", t.key)).collect()
}

/// The armed timers of `call` of type `timer_type`.
fn timers_of_type(call: &Call, timer_type: &TimerType) -> Vec<String> {
    call.timers
        .iter()
        .filter(|t| t.timer_type() == timer_type)
        .map(|t| format!("{:?}", t.key))
        .collect()
}

/// Carol, the failover callee, answers; alice hangs up; the call is reaped
/// with one CDR.
async fn failover_answered_then_hung_up(call: &mut ClientInvite, carol: &Agent, b2bua: &B2buaSut) {
    let mut carol_uas = carol.receive("INVITE").await;
    carol_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    carol_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    carol.receive("ACK").await;
    let mut bye = dialog.bye().await;
    carol.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

/// The callee refuses 486 while its ring deadline is armed: the turn that
/// ends the leg retires the deadline, so while the failure consult is
/// awaited no timer names the ended leg.
#[tokio::test(start_paused = true)]
async fn a_refused_legs_ring_deadline_leaves_the_ledger_with_the_leg() {
    const CALL_ID: &str = "timer-scope-refused@127.0.0.1";
    let h = Harness::new("timer-scope-refused-leg");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(failing_over(5070, 5071, 30))
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut bob_uas = bob.receive("INVITE").await;
    let ringing = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(timers_on_leg(&ringing, "b-1"), vec!["NoAnswer on leg b-1"], "bob's ring deadline");

    bob_uas.respond(486, "Busy Here").await;
    bob.receive("ACK").await;
    h.advance(FAILURE_CONSULT / 2).await;
    let consulting = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(
        timers_on_leg(&consulting, "b-1"),
        Vec::<String>::new(),
        "bob's leg ended at his 486: no timer names it while the failure consult is awaited",
    );

    failover_answered_then_hung_up(&mut call, &carol, &b2bua).await;
    let _report = h.finish().await;
}

/// The callee rings past its ring deadline: the fire CANCELs it and consults
/// the failure; the fired deadline does not stay in the ledger while that
/// answer is awaited.
#[tokio::test(start_paused = true)]
async fn a_fired_ring_deadline_leaves_the_ledger_while_the_failure_consult_is_awaited() {
    const CALL_ID: &str = "timer-scope-fired@127.0.0.1";
    let h = Harness::new("timer-scope-fired-ring-deadline");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let carol = h.agent("carol", "127.0.0.1:5071").await;
    let b2bua = B2buaSut::builder(failing_over(5070, 5071, 5))
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;

    h.advance(Duration::from_secs(6)).await;
    let mut cancel = bob.receive("CANCEL").await;
    let consulting = b2bua.live_call(&call_ref).expect("the call is live");
    assert_eq!(
        timers_of_type(&consulting, &TimerType::NoAnswer),
        Vec::<String>::new(),
        "the fired ring deadline left the ledger at its fire, not at the failover answer",
    );
    cancel.respond(200, "OK").await;
    bob_uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;

    failover_answered_then_hung_up(&mut call, &carol, &b2bua).await;
    let _report = h.finish().await;
}

/// The call-duration cap armed at setup is re-armed at the answer; a fire of
/// the setup instance that reached the router behind the answer's turn is
/// stale and the answered call stays up.
#[tokio::test(start_paused = true)]
async fn a_duration_cap_fire_queued_behind_its_rearm_leaves_the_call_up() {
    const CALL_ID: &str = "timer-scope-stale-cap@127.0.0.1";
    let h = Harness::new("timer-scope-stale-duration-cap");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5070)
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    let cap = |c: &Call| {
        c.timers
            .iter()
            .find(|t| *t.timer_type() == TimerType::GlobalDuration)
            .map(|t| (t.fire_at, t.generation))
    };

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let setup = b2bua.live_call(&call_ref).expect("the call is live");
    let (setup_cap, setup_generation) = cap(&setup).expect("the setup arms the duration cap");

    h.advance(Duration::from_secs(2)).await;
    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let answered = b2bua.live_call(&call_ref).expect("the call is live");
    assert!(
        cap(&answered).expect("the answer re-arms the duration cap").0 > setup_cap,
        "the answer's instance replaced the setup's",
    );

    b2bua.post_event(CallEvent::Timer {
        key: TimerKey::call(TimerType::GlobalDuration),
        generation: setup_generation,
        call_ref: call_ref.clone(),
        incarnation: Some(answered.incarnation().to_string()),
    });
    h.advance(Duration::from_secs(1)).await;
    assert!(
        bob.try_receive_tolerating("BYE", &[]).await.is_none(),
        "the setup instance's fire is stale: the answered call is not torn down",
    );

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}

/// A test-only service: a ring deadline armed at setup, re-armed for a
/// longer wait by the callee's first ring, which fails the call 480 when it
/// fires.
mod ring_deadline {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed, Terminal, TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::{CdrEventType, Direction, TimerKey, TimerType};

    pub const SETUP_SEC: i64 = 5;
    pub const RINGING_SEC: i64 = 10;
    pub const DEADLINE: TimerType = TimerType::service(RING_DEADLINE, "ring");

    define_service! {
        id: "ring-deadline",
        machine: RING_DEADLINE,
        states: RdState { Waiting, Ringing },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(RdState::Waiting.label()).with_actions(vec![
                RuleAction::arm(TimerKey::call(DEADLINE), TimerDelay::secs(SETUP_SEC)),
            ]))
        },
        rules: [ extend_on_ring(), fail_on_deadline() ],
    }

    /// The first 18x relays onward and re-arms the deadline for the ring.
    fn extend_on_ring() -> RuleDefinition {
        sm_rule! {
            id: "ring-deadline-extend",
            machine: RING_DEADLINE,
            active: [ RdState::Waiting ],
            transitions: [ RdState::Waiting => RdState::Ringing ],
            effects: [
                Effect::Relay { label: "relay the 18x onward" },
                Effect::GuardTimer { timer: DEADLINE, scope: call::TimerScopeKind::Call, label: "re-arm the ring deadline" },
            ],
            matcher: Match::response().method("INVITE").status_class(1).direction(Direction::FromB),
            handle: |ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::RelayToPeer { transform: Default::default() },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Provisional,
                        leg_id: ctx.source_leg_id.to_string(),
                        status_code: ctx.response().map(|r| r.status() as i64),
                        reason: None,
                    },
                    RuleAction::arm(TimerKey::call(DEADLINE), TimerDelay::secs(RINGING_SEC)),
                    RuleAction::SetState {
                        machine: RING_DEADLINE,
                        to: RdState::Ringing.label(),
                    },
                ]))
            },
        }
    }

    /// The deadline fired: 480 to the caller and tear the call down.
    fn fail_on_deadline() -> RuleDefinition {
        sm_rule! {
            id: "ring-deadline-fire",
            machine: RING_DEADLINE,
            active: [ RdState::Waiting, RdState::Ringing ],
            transitions: [ RdState::Waiting => Terminal, RdState::Ringing => Terminal ],
            effects: [
                Effect::Respond { status: 480, label: "the deadline passed" },
                Effect::LifecycleCommand { label: "terminate the call" },
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
                    RuleAction::BeginTermination {
                        reason: Some("ring-deadline".into()),
                        cause: call::TerminationCause::Timeout(call::TimeoutKind::NoAnswer),
                        by_leg: None,
                    },
                    RuleAction::ClearState { machine: RING_DEADLINE },
                ]))
            },
        }
    }
}

/// A service re-arms its own deadline on the callee's ring; a fire of the
/// setup instance that reached the router behind that turn is stale and the
/// call rings on to its answer.
#[tokio::test(start_paused = true)]
async fn a_service_deadline_fire_queued_behind_its_rearm_reaches_no_rule() {
    const CALL_ID: &str = "timer-scope-stale-service@127.0.0.1";
    let h = Harness::new("timer-scope-stale-service-deadline");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let b2bua = B2buaSut::route_all_to("127.0.0.1", 5070)
        .services(vec![ring_deadline::service_def()])
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;
    let call_ref = call::derive_call_ref("w0", CALL_ID, FROM_TAG);
    let deadline = |c: &Call| {
        c.timers
            .iter()
            .find(|t| *t.timer_type() == ring_deadline::DEADLINE)
            .map(|t| (t.fire_at, t.generation))
    };

    let mut call = alice
        .invite(&bob)
        .identity(CALL_ID, FROM_TAG)
        .with_sdp(OFFER)
        .through(b2bua.addr)
        .send()
        .await;
    let mut bob_uas = bob.receive("INVITE").await;
    let setup = b2bua.live_call(&call_ref).expect("the call is live");
    let (setup_deadline, setup_generation) =
        deadline(&setup).expect("the service arms its deadline at setup");

    h.advance(Duration::from_secs(1)).await;
    bob_uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let ringing = b2bua.live_call(&call_ref).expect("the call is live");
    assert!(
        deadline(&ringing).expect("the ring re-arms the deadline").0 > setup_deadline,
        "the ring's instance replaced the setup's",
    );

    b2bua.post_event(CallEvent::Timer {
        key: TimerKey::call(ring_deadline::DEADLINE),
        generation: setup_generation,
        call_ref: call_ref.clone(),
        incarnation: Some(ringing.incarnation().to_string()),
    });
    h.advance(Duration::from_secs(1)).await;
    assert!(
        bob.try_receive_tolerating("CANCEL", &[]).await.is_none(),
        "the setup instance's fire is stale: the ringing callee is not cancelled",
    );

    bob_uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _report = h.finish().await;
}
