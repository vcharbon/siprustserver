//! The answer to a request a call sent off its turn — a release or failure
//! consult's fold, a service's HTTP result — is the node's own work: the call sent
//! the request once and nothing sends its answer again. So the per-call
//! dispatcher never drops one for want of room: it waits past a full queue,
//! and the call acts on it once its worker frees. A request a peer sends
//! keeps its bounded room.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse, NewCallRequest,
    NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::AdaptationHttpPort;
use b2bua_harness::{settle_until, B2buaScene, B2buaSut};
use call::ReleaseEventKind;
use http_net::{
    HttpRequest, HttpResponse, HttpServerHandle, HttpService, HttpTransport, SimulatedHttpNetwork,
};
use scenario_harness::callflow::OFFER_SDP;
use sip_message::generators::InDialogMethod;

use crate::common::unrun::{establish_keeping_answer, DialogIds};

/// The call's maximum duration; its expiry sends the release consult.
const MAX_DURATION: Duration = Duration::from_secs(60);
/// How long the decision layer takes to answer a release or failure consult.
const CONSULT_DELAY: Duration = Duration::from_secs(2);
/// How long the adaptation backend takes to answer the service's request.
const HTTP_DELAY: Duration = Duration::from_secs(3);
/// The decision deadline: how long carol's parked call holds the permit.
const DECISION_DEADLINE_MS: u64 = 10_000;

fn http_addr() -> SocketAddr {
    "10.0.0.29:8080".parse().unwrap()
}

/// Routes the first call with a subscribed maximum duration; every later
/// `new_call` never resolves, so its INVITE holds a handler permit until the
/// decision deadline. A release consult is answered `Release`, and a failure
/// consult as scripted (the failed final relayed), after [`CONSULT_DELAY`].
struct SlowConsults {
    inner: ScriptedDecisionEngine,
    bob_port: u16,
    calls: AtomicUsize,
}

impl SlowConsults {
    fn to(bob_port: u16) -> Self {
        Self {
            inner: ScriptedDecisionEngine::route_all_to("127.0.0.1", bob_port),
            bob_port,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl CallDecisionEngine for SlowConsults {
    async fn new_call(&self, _req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
            return std::future::pending().await;
        }
        let mut route = route_to("127.0.0.1", self.bob_port);
        route.features.platform.max_duration_sec = MAX_DURATION.as_secs() as i64;
        route.callback_context = Some("ctx-release".into());
        route.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
        Ok(NewCallResponse::Route(route))
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        tokio::time::sleep(CONSULT_DELAY).await;
        self.inner.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.inner.call_refer(req).await
    }
    async fn call_release(
        &self,
        _req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        tokio::time::sleep(CONSULT_DELAY).await;
        Ok(CallReleaseResponse::Release { label: None, service_ext: Default::default() })
    }
}

/// An adaptation backend answering 200 after [`HTTP_DELAY`].
struct SlowBackend;

#[async_trait]
impl HttpService for SlowBackend {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        tokio::time::sleep(HTTP_DELAY).await;
        HttpResponse::status(200)
    }
}

/// A service that sends one HTTP request just before the call's maximum
/// duration and records the result it reads.
mod slowprobe {
    use std::sync::Mutex;
    use std::time::Duration;

    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceSeed,
        TimerDelay,
    };
    use b2bua::{define_service, sm_rule, CallEvent};
    use call::TimerType;

    /// When the request leaves, from the call's setup.
    pub const KICK_AFTER: Duration = Duration::from_secs(59);

    /// (outcome, error) of every result the service read.
    pub static RESULTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

    const KICK: TimerType = TimerType::service(SLOWPROBE, "kick");

    define_service! {
        id: "slowprobe",
        machine: SLOWPROBE,
        states: SpState { Working, Awaiting, Done },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(SpState::Working.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(SLOWPROBE, "kick"),
                    delay: TimerDelay::secs(KICK_AFTER.as_secs() as i64),
                    leg_id: None,
                },
            ]))
        },
        rules: [ kick(), result() ],
    }

    fn kick() -> RuleDefinition {
        sm_rule! {
            id: "slowprobe-kick",
            machine: SLOWPROBE,
            active: [ SpState::Working ],
            transitions: [ SpState::Working => SpState::Awaiting ],
            effects: [],
            matcher: Match::timer().timer_type(KICK),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::ServiceHttpRequest {
                        correlation_id: "slow-1".into(),
                        endpoint: "/adapt".into(),
                        method: "POST".into(),
                        headers: vec![],
                        body: Vec::new(),
                        content_type: None,
                        timeout_ms: None,
                    },
                    RuleAction::SetState { machine: SLOWPROBE, to: SpState::Awaiting.label() },
                ]))
            },
        }
    }

    fn result() -> RuleDefinition {
        sm_rule! {
            id: "slowprobe-result",
            machine: SLOWPROBE,
            active: [ SpState::Awaiting ],
            transitions: [ SpState::Awaiting => SpState::Done ],
            effects: [],
            matcher: Match::internal_event().topic("service-http-result"),
            handle: |ctx: &RuleContext| {
                if let CallEvent::InternalEvent { outcome, payload, .. } = ctx.event {
                    let error = payload
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    RESULTS.lock().unwrap().push((outcome.clone(), error));
                }
                Some(RuleHandleResult::new(vec![
                    RuleAction::SetState { machine: SLOWPROBE, to: SpState::Done.label() },
                ]))
            },
        }
    }
}

/// A service whose two timers come due one second into the call: what fills
/// the call's worker and queue while it is still in setup.
mod ticker {
    use std::time::Duration;

    use b2bua::rules::{
        Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceSeed,
        TimerDelay,
    };
    use b2bua::{define_service, sm_rule};
    use call::TimerType;

    /// When both timers come due, from the call's setup.
    pub const DUE_AFTER: Duration = Duration::from_secs(1);

    const TICK_A: TimerType = TimerType::service(TICKER, "a");
    const TICK_B: TimerType = TimerType::service(TICKER, "b");

    define_service! {
        id: "ticker",
        machine: TICKER,
        states: TkState { Ticking },
        init: |_call: &RuleCall| {
            let due = TimerDelay::secs(DUE_AFTER.as_secs() as i64);
            Some(ServiceSeed::new(TkState::Ticking.label()).with_actions(vec![
                RuleAction::ScheduleTimer { timer_type: TICK_A, delay: due, leg_id: None },
                RuleAction::ScheduleTimer { timer_type: TICK_B, delay: due, leg_id: None },
            ]))
        },
        rules: [ tick_a(), tick_b() ],
    }

    fn tick_a() -> RuleDefinition {
        sm_rule! {
            id: "ticker-a",
            machine: TICKER,
            active: [ TkState::Ticking ],
            transitions: [],
            effects: [],
            matcher: Match::timer().timer_type(TICK_A),
            handle: |_ctx: &RuleContext| Some(RuleHandleResult::new(vec![])),
        }
    }

    fn tick_b() -> RuleDefinition {
        sm_rule! {
            id: "ticker-b",
            machine: TICKER,
            active: [ TkState::Ticking ],
            transitions: [],
            effects: [],
            matcher: Match::timer().timer_type(TICK_B),
            handle: |_ctx: &RuleContext| Some(RuleHandleResult::new(vec![])),
        }
    }
}

/// One handler permit, held by carol's call parked on its decision; alice's
/// call's queue one deep. At its maximum duration alice's call sends its
/// release consult, and its service has an HTTP request in flight; two
/// INFOs then fill the call's worker and queue, and a third, a peer's
/// request, is dropped at dispatch. Both answers land on the full queue and
/// wait past it: once the permit frees the INFOs are relayed, the service
/// reads its backend's 200, and the release ends the call, one CDR, nothing
/// left behind.
#[tokio::test(start_paused = true)]
async fn answers_landing_on_a_full_queue_still_reach_the_call() {
    slowprobe::RESULTS.lock().unwrap().clear();
    let http = SimulatedHttpNetwork::new();
    let _backend: Box<dyn HttpServerHandle> =
        http.serve(http_addr(), Arc::new(SlowBackend)).await.unwrap();
    let s = B2buaScene::with_b2bua("b2bua-own-answer-queue-full", |bob_port| {
        B2buaSut::builder(Arc::new(SlowConsults::to(bob_port)))
            .services(vec![slowprobe::service_def()])
            .adaptation_http(AdaptationHttpPort {
                transport: Arc::new(http.clone()) as Arc<dyn HttpTransport>,
                base: http_addr(),
                default_timeout: Duration::from_secs(5),
            })
            .tune(|c| {
                c.event_dispatch_concurrency = 1;
                c.per_call_queue_depth = 1;
                c.call_control_timeout_ms = DECISION_DEADLINE_MS as i64;
                c.keepalive_interval_sec = 3_600;
            })
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;
    let (mut dialog, answer) = establish_keeping_answer(&s.alice, &s.bob, s.b2bua.addr).await;
    let ids = DialogIds::of(&answer);

    // The service's request and the release consult leave on free turns.
    s.h.advance(MAX_DURATION + Duration::from_millis(100)).await;

    // carol's call parks on its decision and holds the one handler permit;
    // two INFOs fill alice's call's worker and queue; a third is dropped.
    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(Duration::from_millis(200)).await;
    let mut info1 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(200)).await;
    let mut info2 = dialog.send_request(InDialogMethod::Info).send().await;
    s.h.advance(Duration::from_millis(200)).await;
    let info3 = ids.info(&s.alice, dialog.local_cseq() + 1, "info-queue-full");
    s.alice.try_send_datagram(&info3, s.b2bua.addr).await.expect("the INFO leaves");
    s.h.advance(Duration::from_millis(200)).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 1, "a peer's request keeps its bound");
    assert_eq!(s.b2bua.txn_metrics().unanswered_forgotten(), 1, "its transaction is forgotten");

    // Both answers land on the full queue and wait past it.
    let waiting = s.b2bua.metrics().past_bound_total();
    s.h.advance(HTTP_DELAY).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 1, "no answer is dropped");
    assert_eq!(s.b2bua.metrics().past_bound_total(), waiting + 2, "both answers wait past it");

    // carol's decision deadline frees the permit: the INFOs are relayed,
    // then the queued answers run.
    s.h.advance(Duration::from_millis(DECISION_DEADLINE_MS)).await;
    parked.expect(503).await;
    for _ in 0..2 {
        s.bob.receive("INFO").await.respond(200, "OK").await;
    }

    // The release fold ends the call; the INFOs still draw their 200s.
    s.alice
        .try_receive("BYE")
        .await
        .expect("the release fold ends the call toward alice")
        .respond(200, "OK")
        .await;
    s.bob
        .try_receive("BYE")
        .await
        .expect("the release fold ends the call toward bob")
        .respond(200, "OK")
        .await;
    info1.expect(200).await;
    info2.expect(200).await;

    // The service read its backend's answer, not its deadline's.
    assert_eq!(
        slowprobe::RESULTS.lock().unwrap().clone(),
        vec![("ok".to_string(), String::new())],
        "the service reads the backend's 200"
    );

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    let cdrs = s.b2bua.cdr_records();
    let ended = cdrs
        .iter()
        .filter(|c| c.events.iter().any(|e| e.reason.as_deref() == Some("max_duration")))
        .count();
    assert_eq!(ended, 1, "one CDR states the maximum duration");
    let _ = s.finish().await;
}

/// One handler permit, held by carol's call parked on its decision; alice's
/// call's queue one deep. bob refuses alice's call and its failure consult
/// leaves; the call's two service timers then fill its worker and queue.
/// The consult's fold lands on the full queue and waits past it: once the
/// permit frees, alice hears bob's 486 at once, not at the consult's
/// deadline, one CDR, nothing left behind.
#[tokio::test(start_paused = true)]
async fn a_failure_fold_landing_on_a_full_queue_still_reaches_the_call() {
    let s = B2buaScene::with_b2bua("b2bua-failure-fold-queue-full", |bob_port| {
        B2buaSut::builder(Arc::new(SlowConsults::to(bob_port)))
            .services(vec![ticker::service_def()])
            .tune(|c| {
                c.event_dispatch_concurrency = 1;
                c.per_call_queue_depth = 1;
                c.call_control_timeout_ms = DECISION_DEADLINE_MS as i64;
            })
    })
    .await;
    let carol = s.h.agent("carol", "127.0.0.1:5062").await;

    // bob refuses alice's call: its failure consult leaves.
    let mut call = s.alice.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.bob.receive("INVITE").await.respond(486, "Busy Here").await;
    s.bob.receive("ACK").await;

    // carol's call parks on its decision and holds the one handler permit;
    // the two service timers fill alice's call's worker and queue.
    let mut parked = carol.invite(&s.bob).with_sdp(OFFER_SDP).through(s.b2bua.addr).send().await;
    s.h.advance(ticker::DUE_AFTER).await;
    let waiting = s.b2bua.metrics().past_bound_total();

    // The fold lands on the full queue and waits past it.
    s.h.advance(CONSULT_DELAY).await;
    assert_eq!(s.b2bua.metrics().queue_drops_total(), 0, "the fold is not dropped");
    assert_eq!(s.b2bua.metrics().past_bound_total(), waiting + 1, "it waits past the queue");

    // carol's decision deadline frees the permit: alice hears bob's refusal.
    s.h.advance(Duration::from_millis(DECISION_DEADLINE_MS)).await;
    parked.expect(503).await;
    call.expect(486).await;

    settle_until(|| s.b2bua.metrics().removals_total() == s.b2bua.metrics().creations_total())
        .await;
    let refused = s.b2bua.cdr_records().iter().filter(|c| !c.b_legs.is_empty()).count();
    assert_eq!(refused, 1, "one CDR for alice's refused call");
    let _ = s.finish().await;
}
