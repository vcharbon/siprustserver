//! The generic service-authorable async-HTTP callback
//! ([`RuleAction::ServiceHttpRequest`]) end-to-end — the seam that generalizes
//! the two hand-rolled `ReferAsyncHttp`/`FailureAsyncHttp` copies (ADR-0016).
//!
//! **The point of this test is BINARY SAFETY.** A service mid-dialog POSTs a
//! logical adaptation endpoint with an arbitrary `Vec<u8>` request body and gets
//! the response entity back **byte-for-byte** — including non-UTF-8 bytes
//! (`0x00`, `0x80`, `0xfe`, `0xff`, …) that could never survive a coercion
//! through a character string. The response bytes ride
//! [`CallEvent::InternalEvent::body`] (a raw `Vec<u8>`), NOT the JSON `payload`.
//!
//! A tiny `binprobe` service arms a per-call timer at setup; when it fires it
//! emits TWO `ServiceHttpRequest`s with distinct correlation ids and distinct
//! non-UTF-8 bodies, and a second rule catches each `service-http-result`
//! re-entry (matched by topic + a correlation `filter`) and records the raw
//! bytes. The test asserts each correlation got ITS OWN response bytes verbatim
//! (proving no crossing), the echoed status, and that the server saw each
//! request body binary-intact. A second `errprobe` service (no port injected)
//! proves the machine is never stranded: the dispatch folds an `outcome:"error"`
//! re-entry instead.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::AdaptationHttpPort;
use b2bua_harness::{establish, hangup, settle_until, B2buaSut};
use http_net::{
    HttpRequest, HttpResponse, HttpServerHandle, HttpService, HttpTransport, SimulatedHttpNetwork,
};
use scenario_harness::Harness;

// ── The binary payloads under test. Each contains bytes that are NOT valid
//    UTF-8 in isolation (0x80/0xC0/0xfe/0xff), so a string-coerced transport
//    would corrupt or reject them. `assert_non_utf8` pins that below. ──────────
fn req_a() -> Vec<u8> {
    vec![0xDE, 0xAD, 0x00, 0xBE, 0xEF]
}
fn req_b() -> Vec<u8> {
    vec![0x00, 0xFF, 0x10, 0x80, 0x01]
}
fn resp_a() -> Vec<u8> {
    vec![0x00, 0x01, 0x80, 0xfe, 0xff]
}
fn resp_b() -> Vec<u8> {
    vec![0x7f, 0xC0, 0x00, 0xAB, 0xff, 0x80]
}

const CORR_A: &str = "corr-a";
const CORR_B: &str = "corr-b";
const ENDPOINT: &str = "/adapt";

fn laddr() -> SocketAddr {
    "10.0.0.9:8080".parse().unwrap()
}

/// A minimal adaptation backend: maps the `X-Corr` request header to a distinct
/// non-UTF-8 response body (so a crossed correlation would surface as wrong
/// bytes), and records every received request body so the test can assert the
/// REQUEST payload arrived binary-intact too.
struct EchoServer {
    received: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
}

#[async_trait]
impl HttpService for EchoServer {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        let corr = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("X-Corr"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        self.received.lock().unwrap().push((corr.clone(), req.body.clone()));
        match corr.as_str() {
            CORR_A => HttpResponse::ok(resp_a()),
            CORR_B => HttpResponse::status(201)
                .with_body(resp_b())
                .header("X-Echo", CORR_B),
            _ => HttpResponse::status(200),
        }
    }
}

// ── binprobe: fires two concurrent binary ServiceHttpRequests on a timer, and
//    records each raw response body keyed by correlation. ───────────────────────
mod binprobe {
    use b2bua::rules::{
        Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, RuleCall, ServiceSeed,
    };
    use b2bua::{define_service, sm_rule, CallEvent};
    use call::TimerType;
    use std::sync::Mutex;

    use super::{req_a, req_b, CORR_A, CORR_B, ENDPOINT};

    /// (correlation_id, status, raw response body) recorded by the result rule.
    pub static RESULTS: Mutex<Vec<(String, u16, Vec<u8>)>> = Mutex::new(Vec::new());

    const KICK: TimerType = TimerType::service(BINPROBE, "kick");

    define_service! {
        id: "binprobe",
        machine: BINPROBE,
        states: BpState { Working, Awaiting },
        // Arm the trigger at call setup (a drivable per-call timer — the same
        // seam the ringwatch/dualkeys service-timer tests use).
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(BpState::Working.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(BINPROBE, "kick"),
                    delay_sec: 1,
                    leg_id: None,
                },
            ]))
        },
        rules: [ kick(), result() ],
    }

    /// The trigger fires → emit TWO ServiceHttpRequests with distinct
    /// correlation ids + distinct BINARY request bodies.
    fn kick() -> RuleDefinition {
        sm_rule! {
            id: "binprobe-kick",
            machine: BINPROBE,
            active: [ BpState::Working ],
            transitions: [ BpState::Working => BpState::Awaiting ],
            effects: [],
            matcher: Match::timer().timer_type(KICK),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::ServiceHttpRequest {
                        correlation_id: CORR_A.into(),
                        endpoint: ENDPOINT.into(),
                        method: "POST".into(),
                        headers: vec![("X-Corr".into(), CORR_A.into())],
                        body: req_a(),
                        content_type: Some("application/octet-stream".into()),
                        timeout_ms: None,
                    },
                    RuleAction::ServiceHttpRequest {
                        correlation_id: CORR_B.into(),
                        endpoint: ENDPOINT.into(),
                        method: "POST".into(),
                        headers: vec![("X-Corr".into(), CORR_B.into())],
                        body: req_b(),
                        content_type: Some("application/octet-stream".into()),
                        timeout_ms: None,
                    },
                    RuleAction::SetState { machine: BINPROBE, to: BpState::Awaiting.label() },
                ]))
            },
        }
    }

    /// Correlation filter (the reuse-safe seam): gate on the echoed
    /// `correlation_id` in the result payload. Stays `Awaiting` (no transition),
    /// so it catches BOTH concurrent re-entries. Records the RAW response bytes.
    fn result() -> RuleDefinition {
        sm_rule! {
            id: "binprobe-result",
            machine: BINPROBE,
            active: [ BpState::Awaiting ],
            transitions: [],
            effects: [],
            matcher: Match::internal_event()
                .topic("service-http-result")
                .filter(corr_is_ours),
            handle: |ctx: &RuleContext| {
                if let CallEvent::InternalEvent { payload, body, .. } = ctx.event {
                    let corr = payload
                        .get("correlation_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let status = payload
                        .get("status")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u16;
                    // `body` is `&Vec<u8>` — the entity bytes verbatim, straight
                    // off the wire, never round-tripped through a String.
                    RESULTS.lock().unwrap().push((corr, status, body.clone()));
                }
                Some(RuleHandleResult::new(vec![]))
            },
        }
    }

    /// The `filter` surface: only accept results this service minted. In
    /// production a service compares against its call's pending-request set; here
    /// the two ids share a known prefix.
    fn corr_is_ours(ctx: &RuleContext) -> bool {
        matches!(
            ctx.event,
            CallEvent::InternalEvent { payload, .. }
                if payload.get("correlation_id").and_then(|v| v.as_str())
                    .map(|s| s.starts_with("corr-")).unwrap_or(false)
        )
    }
}

// ── errprobe: fires one request with NO port injected → the dispatch folds an
//    `outcome:"error"` re-entry (the machine is never stranded). ────────────────
mod errprobe {
    use b2bua::rules::{
        Match, RuleAction, RuleContext, RuleDefinition, RuleHandleResult, RuleCall, ServiceSeed,
    };
    use b2bua::{define_service, sm_rule, CallEvent};
    use call::TimerType;
    use std::sync::Mutex;

    use super::ENDPOINT;

    /// (correlation_id, outcome, error string) recorded by the result rule.
    pub static RESULTS: Mutex<Vec<(String, String, String)>> = Mutex::new(Vec::new());

    pub const CORR: &str = "err-1";
    const KICK: TimerType = TimerType::service(ERRPROBE, "kick");

    define_service! {
        id: "errprobe",
        machine: ERRPROBE,
        states: EpState { Working, Awaiting },
        init: |_call: &RuleCall| {
            Some(ServiceSeed::new(EpState::Working.label()).with_actions(vec![
                RuleAction::ScheduleTimer {
                    timer_type: TimerType::service(ERRPROBE, "kick"),
                    delay_sec: 1,
                    leg_id: None,
                },
            ]))
        },
        rules: [ kick(), result() ],
    }

    fn kick() -> RuleDefinition {
        sm_rule! {
            id: "errprobe-kick",
            machine: ERRPROBE,
            active: [ EpState::Working ],
            transitions: [ EpState::Working => EpState::Awaiting ],
            effects: [],
            matcher: Match::timer().timer_type(KICK),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![
                    RuleAction::ServiceHttpRequest {
                        correlation_id: CORR.into(),
                        endpoint: ENDPOINT.into(),
                        method: "POST".into(),
                        headers: vec![],
                        body: vec![0x00, 0x80, 0xff],
                        content_type: None,
                        timeout_ms: None,
                    },
                    RuleAction::SetState { machine: ERRPROBE, to: EpState::Awaiting.label() },
                ]))
            },
        }
    }

    fn result() -> RuleDefinition {
        sm_rule! {
            id: "errprobe-result",
            machine: ERRPROBE,
            active: [ EpState::Awaiting ],
            transitions: [],
            effects: [],
            matcher: Match::internal_event().topic("service-http-result"),
            handle: |ctx: &RuleContext| {
                if let CallEvent::InternalEvent { outcome, payload, .. } = ctx.event {
                    let corr = payload
                        .get("correlation_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let err = payload
                        .get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    RESULTS.lock().unwrap().push((corr, outcome.clone(), err));
                }
                Some(RuleHandleResult::new(vec![]))
            },
        }
    }
}

fn assert_non_utf8(bytes: &[u8]) {
    assert!(
        String::from_utf8(bytes.to_vec()).is_err(),
        "test payload must be non-UTF-8 to prove binary-safety: {bytes:?}",
    );
}

/// The core binary-safety assertion: two concurrent in-flight requests round-trip
/// their non-UTF-8 bodies byte-for-byte, keyed correctly by correlation id (no
/// crossing), with the echoed status — and the server saw each request body
/// binary-intact.
#[tokio::test(start_paused = true)]
async fn service_http_request_round_trips_a_binary_body_verbatim() {
    // Guard: the payloads really are non-UTF-8 (a string transport would break).
    for p in [req_a(), req_b(), resp_a(), resp_b()] {
        assert_non_utf8(&p);
    }
    binprobe::RESULTS.lock().unwrap().clear();

    let h = Harness::new("service-http-binary");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // The adaptation backend on the simulated HTTP fabric (mirrors how
    // `limiter_refresh` serves a real `LimiterServer`).
    let http = SimulatedHttpNetwork::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    let server = Arc::new(EchoServer { received: received.clone() });
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070));
    let b2bua = B2buaSut::builder(decision)
        .services(vec![binprobe::service_def()])
        .adaptation_http(AdaptationHttpPort {
            transport: Arc::new(http.clone()) as Arc<dyn HttpTransport>,
            base: laddr(),
            default_timeout: Duration::from_secs(3),
        })
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    let mut dialog = establish(&alice, &bob, b2bua.addr).await;

    // Fire the trigger (1 s) → two ServiceHttpRequests → transport → re-entries.
    h.advance(Duration::from_secs(2)).await;
    settle_until(|| binprobe::RESULTS.lock().unwrap().len() == 2).await;

    let results = binprobe::RESULTS.lock().unwrap().clone();
    let a = results.iter().find(|(c, ..)| c == CORR_A).expect("corr-a re-entered");
    let b = results.iter().find(|(c, ..)| c == CORR_B).expect("corr-b re-entered");

    // THE binary-safety assertion: each correlation got its OWN response bytes,
    // byte-for-byte, including 0x00/0x80/0xfe/0xff — proving no string coercion
    // and no crossing between the two concurrent requests.
    assert_eq!(a.1, 200, "corr-a echoed status");
    assert_eq!(a.2, resp_a(), "corr-a response body verbatim (binary-safe)");
    assert_eq!(b.1, 201, "corr-b echoed status");
    assert_eq!(b.2, resp_b(), "corr-b response body verbatim (binary-safe)");

    // The REQUEST body also arrived binary-intact at the backend, keyed right.
    let recv = received.lock().unwrap().clone();
    let ra = recv.iter().find(|(c, _)| c == CORR_A).expect("server saw corr-a");
    let rb = recv.iter().find(|(c, _)| c == CORR_B).expect("server saw corr-b");
    assert_eq!(ra.1, req_a(), "server received corr-a request body verbatim");
    assert_eq!(rb.1, req_b(), "server received corr-b request body verbatim");

    hangup(&mut dialog, &bob).await;
    let _ = h.finish().await;
}

/// The error path: with NO `AdaptationHttpPort` injected, a `ServiceHttpRequest`
/// still folds an `outcome:"error"` re-entry, so the service machine is never
/// stranded waiting on a response that will never come.
#[tokio::test(start_paused = true)]
async fn service_http_request_reenters_error_when_no_port_injected() {
    errprobe::RESULTS.lock().unwrap().clear();

    let h = Harness::new("service-http-error");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;

    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071));
    // NOTE: no `.adaptation_http(..)` — the port is absent.
    let b2bua = B2buaSut::builder(decision)
        .services(vec![errprobe::service_def()])
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;

    let mut dialog = establish(&alice, &bob, b2bua.addr).await;

    h.advance(Duration::from_secs(2)).await;
    settle_until(|| !errprobe::RESULTS.lock().unwrap().is_empty()).await;

    let results = errprobe::RESULTS.lock().unwrap().clone();
    let (corr, outcome, err) = &results[0];
    assert_eq!(corr, errprobe::CORR, "error re-entry echoes the correlation id");
    assert_eq!(outcome, "error", "absent port folds an error re-entry (machine not stranded)");
    assert!(!err.is_empty(), "error re-entry carries a reason: {err:?}");

    hangup(&mut dialog, &bob).await;
    let _ = h.finish().await;
}
