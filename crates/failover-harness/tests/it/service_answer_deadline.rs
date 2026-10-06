//! A service's request sent off the call's turn (an adaptation HTTP request, a
//! replacement of the call's admission set) whose answer dies with the node
//! that sent it: the node crashes with the request in flight, and the call is
//! served again either by the rebooted primary's reclaim or by the backup's
//! takeover (the primary staying dead until the copy lets the call go). The
//! copy reads the request's answer as lost at the request's deadline — not
//! when it was served again — the service goes on, the call ends properly, one
//! CDR is written and the limiter drains.
//!
//! The service under test waits in `Awaiting` for the answer and tells the
//! caller what it read with an in-dialog INFO, so the caller's inbox is the
//! observation: no INFO means the call is stranded in `Awaiting`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use b2bua::answer_deadline::MARGIN;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::http::HttpCallLimiter;
use b2bua::limiter::{CallLimiter, NoopLimiter};
use b2bua::rules::ServiceDef;
use call::LimiterEntry;
use call_limiter::{CallStore, LimiterMetrics, LimiterServer};
use failover_harness::{
    assert_call_fully_over, cookie_field, total_cdrs_for, FailoverHarness, ProxySut,
    ReplicatedB2buaSut, WorkerHealth, LEASE_OUTLIVING_THE_REPLICA_TTL,
};
use http_net::{
    Fault, HttpRequest, HttpResponse, HttpServerHandle, HttpService, HttpTransport,
    SimulatedHttpNetwork,
};
use scenario_harness::Agent;
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const SERVICE_ADDR: &str = "10.0.0.2:8080";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The adaptation port's budget for a request that states none.
const HTTP_BUDGET: Duration = Duration::from_secs(3);
/// The limiter client's admit budget.
const ADMIT_BUDGET: Duration = Duration::from_secs(2);

fn service_addr() -> SocketAddr {
    SERVICE_ADDR.parse().unwrap()
}

fn limiter_addr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// The service's HTTP peer: answers every request `200`.
struct AnswersOk;

#[async_trait]
impl HttpService for AnswersOk {
    async fn handle(&self, _req: HttpRequest) -> HttpResponse {
        HttpResponse::ok(b"answered".to_vec())
    }
}

/// A service that asks on the callee's in-dialog INFO — its body names the
/// request, `http` or `admit` — and tells the caller the outcome it read.
mod awaiter {
    use b2bua::rules::{
        Effect, Match, RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult,
        ServiceSeed,
    };
    use b2bua::{define_service, sm_rule, CallEvent};
    use b2bua_sdk::rules::{Body, BodyAuthor, Method};
    use call::{Direction, LimiterEntry};

    pub const HTTP_CORRELATION: &str = "awaiter:http";
    pub const ADMIT_CORRELATION: &str = "awaiter:admit";

    define_service! {
        id: "awaiter",
        machine: AWAITER,
        states: AwState { Idle, Awaiting, Done },
        init: |_call: &RuleCall| Some(ServiceSeed::new(AwState::Idle.label())),
        rules: [ ask(), http_answered(), admit_answered() ],
    }

    fn ask() -> RuleDefinition {
        sm_rule! {
            id: "awaiter-ask",
            machine: AWAITER,
            active: [ AwState::Idle ],
            transitions: [ AwState::Idle => AwState::Awaiting ],
            effects: [ Effect::Respond { status: 200, label: "200 → the callee's INFO" } ],
            matcher: Match::request().method("INFO").direction(Direction::FromB),
            handle: |ctx: &RuleContext| {
                let request = match ctx.request()?.body().as_ref() {
                    b"admit" => RuleAction::ReplaceAdmissionSet {
                        correlation_id: ADMIT_CORRELATION.into(),
                        entries: vec![
                            LimiterEntry { id: "trunk".into(), limit: 10 },
                            LimiterEntry { id: "service".into(), limit: 10 },
                        ],
                        moves_call: false,
                    },
                    _ => RuleAction::ServiceHttpRequest {
                        correlation_id: HTTP_CORRELATION.into(),
                        endpoint: "/ask".into(),
                        method: "POST".into(),
                        headers: vec![],
                        body: Vec::new(),
                        content_type: None,
                        timeout_ms: None,
                    },
                };
                Some(RuleHandleResult::new(vec![
                    RuleAction::Respond {
                        status: 200,
                        reason: "OK".into(),
                        body: vec![],
                        content_type: None,
                    },
                    request,
                    RuleAction::SetState { machine: AWAITER, to: AwState::Awaiting.label() },
                ]))
            },
        }
    }

    /// The caller told `outcome` in an in-dialog INFO; the service is done.
    fn tell_caller(ctx: &RuleContext) -> Option<RuleHandleResult> {
        let CallEvent::InternalEvent { outcome, .. } = ctx.event else { return None };
        Some(RuleHandleResult::new(vec![
            RuleAction::SendRequestToLeg {
                leg_id: ctx.call.a_leg().leg_id.clone(),
                method: "INFO".into(),
                body: Some(Body::new(
                    outcome.clone().into_bytes(),
                    Some("text/plain".into()),
                    BodyAuthor::Stack,
                )),
                headers: vec![],
            },
            RuleAction::SetState { machine: AWAITER, to: AwState::Done.label() },
        ]))
    }

    fn correlated(ctx: &RuleContext, id: &str) -> bool {
        matches!(ctx.event, CallEvent::InternalEvent { payload, .. }
            if payload.get("correlation_id").and_then(|v| v.as_str()) == Some(id))
    }

    fn http_answered() -> RuleDefinition {
        sm_rule! {
            id: "awaiter-http-answered",
            machine: AWAITER,
            active: [ AwState::Awaiting ],
            transitions: [ AwState::Awaiting => AwState::Done ],
            effects: [ Effect::Originate { method: Method::Info, label: "INFO(outcome) → caller" } ],
            matcher: Match::internal_event()
                .topic("service-http-result")
                .filter(|ctx| correlated(ctx, HTTP_CORRELATION)),
            handle: tell_caller,
        }
    }

    fn admit_answered() -> RuleDefinition {
        sm_rule! {
            id: "awaiter-admit-answered",
            machine: AWAITER,
            active: [ AwState::Awaiting ],
            transitions: [ AwState::Awaiting => AwState::Done ],
            effects: [ Effect::Originate { method: Method::Info, label: "INFO(outcome) → caller" } ],
            matcher: Match::internal_event()
                .topic("limiter-admit-result")
                .filter(|ctx| correlated(ctx, ADMIT_CORRELATION)),
            handle: tell_caller,
        }
    }
}

fn awaiter_services() -> Vec<ServiceDef> {
    vec![awaiter::service_def()]
}

/// A route to bob holding `trunk`.
fn trunk_route() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.new_ruri = None;
                r.call_limiter = vec![LimiterEntry { id: "trunk".into(), limit: 10 }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

struct Cluster {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
}

/// Two replicating workers behind the proxy, running the awaiter service over
/// `http`, with `limiter` on both.
async fn cluster(
    name: &str,
    http: &SimulatedHttpNetwork,
    limiter: Arc<dyn CallLimiter>,
) -> Cluster {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"])
        .with_worker_services(awaiter_services)
        .with_worker_adaptation_http(b2bua::AdaptationHttpPort {
            transport: Arc::new(http.clone()),
            base: service_addr(),
            default_timeout: HTTP_BUDGET,
        });
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut spawn = async |ordinal: &str, addr: &str, peer: &str| {
        fh.spawn_worker_limited(
            ordinal,
            ordinal,
            addr,
            &[peer],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            trunk_route(),
            limiter.clone(),
        )
        .await
    };
    let w_b1 = spawn("b1", B1, "b2").await;
    let w_b2 = spawn("b2", B2, "b1").await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "both ready at steady state");
    Cluster { fh, alice, bob, proxy, w_b1, w_b2 }
}

/// How the call is served once its primary crashed with the request out.
#[derive(Clone, Copy)]
enum Recovery {
    /// The primary reboots and reclaims the call.
    Reclaim,
    /// The primary stays dead; the callee's next in-dialog request makes the
    /// backup take the call over. The primary reboots once the backup's copy
    /// let the call go, and reclaims it.
    Takeover,
}

/// What [`ask`] leaves in the test's hands.
struct Asked {
    dialog: scenario_harness::Dialog,
    bob_dialog: scenario_harness::Dialog,
    call_ref: String,
    primary: String,
    /// The clock just before bob's INFO is sent: the request's deadline is
    /// its budget plus [`MARGIN`] past it.
    asked_at: i64,
}

fn worker<'c>(c: &'c mut Cluster, ordinal: &str) -> &'c mut ReplicatedB2buaSut {
    if ordinal == "b1" {
        &mut c.w_b1
    } else {
        &mut c.w_b2
    }
}

/// Establish alice ↔ bob through the proxy, stall `stalled`, and have bob ask
/// the service for `request` (`http` | `admit`); the asking turn replicates.
async fn ask(
    c: &mut Cluster,
    http: &SimulatedHttpNetwork,
    stalled: SocketAddr,
    request: &[u8],
) -> Asked {
    let mut call = c.alice.invite(&c.bob).with_sdp(OFFER).through(c.proxy.addr()).send().await;
    let mut uas = c.bob.receive("INVITE").await;
    let primary = cookie_field(uas.request(), "w_pri").unwrap_or_default();
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let dialog = call.ack().await;
    c.bob.receive("ACK").await;
    c.fh.advance(Duration::from_millis(500)).await;
    let call_ref = worker(c, &primary)
        .scan_primary(&primary)
        .into_iter()
        .next()
        .expect("the primary serves the call");

    http.apply_fault(Fault::Stall { dst: stalled });
    let mut bob_dialog = uas.dialog();
    let asked_at = c.fh.now_ms();
    let mut info = bob_dialog
        .send_request(InDialogMethod::Info)
        .with_body("text/plain", request.to_vec())
        .send()
        .await;
    info.expect(200).await;
    c.fh.advance(Duration::from_millis(300)).await;
    Asked { dialog, bob_dialog, call_ref, primary, asked_at }
}

/// The primary crashes (the request dies with it) and the proxy reads it dead;
/// the stalled peers answer again.
async fn crash(c: &mut Cluster, primary: &str, http: &SimulatedHttpNetwork) {
    worker(c, primary).crash();
    c.proxy.set_health(primary, WorkerHealth::Dead);
    c.fh.advance(Duration::from_millis(300)).await;
    http.apply_fault(Fault::Resume { dst: service_addr() });
    http.apply_fault(Fault::Resume { dst: limiter_addr() });
}

/// The crashed primary reboots, becomes ready, is announced to the proxy and
/// reclaims its call.
async fn reboot_and_reclaim(c: &mut Cluster, primary: &str) {
    let new_addr = worker(c, primary).reboot().await;
    for _ in 0..40 {
        c.fh.advance(Duration::from_millis(500)).await;
        if worker(c, primary).is_ready() {
            break;
        }
    }
    assert!(worker(c, primary).is_ready(), "rebooted primary re-hydrated from the backup");
    c.proxy.set_address(primary, new_addr);
    c.fh.note_worker_rebound(primary, new_addr);
    c.proxy.set_health(primary, WorkerHealth::Alive);
    c.fh.advance(Duration::from_millis(500)).await;
    assert_eq!(worker(c, primary).active_calls(), 1, "the reboot reclaim re-materialised the call");
}

/// The backup takes the call over on bob's next in-dialog INFO, which the
/// waiting service leaves to the core relay toward alice.
async fn take_over(c: &mut Cluster, asked: &mut Asked) {
    c.fh.advance(Duration::from_secs(1)).await;
    let mut ping = asked
        .bob_dialog
        .send_request(InDialogMethod::Info)
        .with_body("text/plain", b"ping".to_vec())
        .send()
        .await;
    let mut relayed = c.alice.receive("INFO").await;
    assert_eq!(relayed.request().body().as_ref(), b"ping");
    relayed.respond(200, "OK").await;
    ping.expect(200).await;
    let backup = if asked.primary == "b1" { "b2" } else { "b1" };
    assert!(worker(c, backup).serves(&asked.call_ref), "the backup took the call over");
}

/// The caller is told the outcome the service read at the request's
/// deadline (`budget` past the asking turn plus the margin), not earlier.
async fn caller_told(c: &mut Cluster, asked: &Asked, outcome: &str, budget: Duration) {
    let deadline = asked.asked_at + (budget + MARGIN).as_millis() as i64;
    assert!(c.fh.now_ms() < deadline, "the call is served again before its deadline");
    let mut told = c.alice.receive("INFO").await;
    assert_eq!(String::from_utf8_lossy(told.request().body()), outcome);
    assert!(c.fh.now_ms() >= deadline, "read at the deadline, not when the call was restored");
    told.respond(200, "OK").await;
}

/// Alice hangs up; the call is over everywhere with one CDR and nothing held.
async fn hang_up(c: &mut Cluster, mut asked: Asked, store: &CallStore) {
    scenario_harness::callflow::hangup(&mut asked.dialog, &c.bob).await;
    // A replica goes once its holder's puller reconnects, whose backoff caps
    // at 30 s: the harness's terminal settle outlasts it.
    let (b1, b2, call_ref) = (&c.w_b1, &c.w_b2, asked.call_ref.as_str());
    c.fh.settle_terminal(async || {
        let mut over = true;
        for n in [b1, b2] {
            over &= !n.holds_any_trace(call_ref).await && n.memory_clean();
        }
        over
    })
    .await;
    assert_eq!(total_cdrs_for(&[&c.w_b1, &c.w_b2], &asked.call_ref), 1, "one CDR");
    assert_call_fully_over(&[&c.w_b1, &c.w_b2], &asked.call_ref, store).await;
}

/// The request named by `request` is lost with the primary and the call is
/// served again by `recovery`; the service reads `outcome` at the deadline.
async fn lost_answer_cell(
    c: &mut Cluster,
    http: &SimulatedHttpNetwork,
    stalled: SocketAddr,
    request: &[u8],
    recovery: Recovery,
    (outcome, budget): (&str, Duration),
    store: &CallStore,
) {
    let mut asked = ask(c, http, stalled, request).await;
    let primary = asked.primary.clone();
    crash(c, &primary, http).await;
    match recovery {
        Recovery::Reclaim => {
            reboot_and_reclaim(c, &primary).await;
            caller_told(c, &asked, outcome, budget).await;
        }
        Recovery::Takeover => {
            take_over(c, &mut asked).await;
            caller_told(c, &asked, outcome, budget).await;
            // The copy lets the call go once its transactions clear (the
            // non-INVITE ones linger 64·T1, RFC 3261 §17.2.2).
            let backup = if primary == "b1" { "b2" } else { "b1" };
            for _ in 0..160 {
                if !worker(c, backup).serves(&asked.call_ref) {
                    break;
                }
                c.fh.advance(Duration::from_millis(500)).await;
            }
            assert!(!worker(c, backup).serves(&asked.call_ref), "the copy let the call go");
            reboot_and_reclaim(c, &primary).await;
        }
    }
    hang_up(c, asked, store).await;
}

async fn http_cell(name: &str, recovery: Recovery) {
    let http = SimulatedHttpNetwork::new();
    let _service: Box<dyn HttpServerHandle> =
        http.serve(service_addr(), Arc::new(AnswersOk)).await.unwrap();
    let store = CallStore::new(LEASE_OUTLIVING_THE_REPLICA_TTL, Clock::test_at(0));
    let mut c = cluster(name, &http, Arc::new(NoopLimiter)).await;
    let read = ("error", HTTP_BUDGET);
    lost_answer_cell(&mut c, &http, service_addr(), b"http", recovery, read, &store).await;
}

async fn admit_cell(name: &str, recovery: Recovery) {
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(CallStore::new(LEASE_OUTLIVING_THE_REPLICA_TTL, Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _limiter: Box<dyn HttpServerHandle> = http.serve(limiter_addr(), server).await.unwrap();
    let limiter: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), limiter_addr(), ADMIT_BUDGET));
    let mut c = cluster(name, &http, limiter).await;
    let read = ("unavailable", ADMIT_BUDGET);
    lost_answer_cell(&mut c, &http, limiter_addr(), b"admit", recovery, read, &store).await;
    assert_eq!(store.stats().current_total, 0, "the call's release drained the limiter");
}

#[tokio::test(start_paused = true)]
async fn an_http_answer_lost_with_its_node_is_read_as_lost_at_its_deadline() {
    http_cell("service-answer-deadline-http", Recovery::Reclaim).await;
}

#[tokio::test(start_paused = true)]
async fn an_http_answer_lost_with_its_node_is_read_as_lost_on_the_takeover_copy() {
    http_cell("service-answer-deadline-http-takeover", Recovery::Takeover).await;
}

#[tokio::test(start_paused = true)]
async fn an_admit_answer_lost_with_its_node_is_read_as_unavailable_at_its_deadline() {
    admit_cell("service-answer-deadline-admit", Recovery::Reclaim).await;
}

#[tokio::test(start_paused = true)]
async fn an_admit_answer_lost_with_its_node_is_read_as_unavailable_on_the_takeover_copy() {
    admit_cell("service-answer-deadline-admit-takeover", Recovery::Takeover).await;
}
