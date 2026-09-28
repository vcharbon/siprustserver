//! The limiter circuit breaker: no call pays the admit timeout of a limiter
//! that keeps failing.
//!
//! Three consecutive admits that get no usable answer (a stalled limiter, a
//! cut one) open the worker's breaker: from then on an admit sends no request
//! and waits nothing, the call runs uncounted and owes no release, and the
//! release queue sends nothing. A background probe asks the limiter's health
//! answer every second; the first answer closes the breaker, the queue sends
//! what waited, and the next call is counted again.
//!
//! Every call holds three limiters.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua::metrics::{LimiterFailure, LimiterOp, ReleaseGiveUp};
use b2bua_harness::{settle_until, B2buaSut};
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{
    BindError, Fault, HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpService,
    HttpTransport, SimulatedHttpNetwork,
};
use scenario_harness::{Agent, Dialog, Harness, SIMULATED_TRANSIT_DELAY_MS};
use sip_clock::Clock;
use tokio::time::Instant;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The production admit budget.
const ADMIT_BUDGET: Duration = Duration::from_millis(150);
/// The breaker's default probe period.
const PROBE: Duration = Duration::from_secs(1);
/// The three limiters every call holds.
const HOLDS: &[(&str, i64)] = &[("x", 10), ("y", 10), ("z", 10)];
const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// One request the SUT's limiter client sent: its path and its JSON body.
type Sent = (String, serde_json::Value);

/// The simulated fabric behind a log of every request the client sends,
/// whether or not a fault lets it reach the limiter.
struct Recording {
    net: SimulatedHttpNetwork,
    sent: Arc<Mutex<Vec<Sent>>>,
}

#[async_trait]
impl HttpTransport for Recording {
    async fn serve(
        &self,
        addr: SocketAddr,
        service: Arc<dyn HttpService>,
    ) -> Result<Box<dyn HttpServerHandle>, BindError> {
        self.net.serve(addr, service).await
    }

    async fn request(&self, dst: SocketAddr, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let body = serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
        self.sent.lock().unwrap().push((req.path.clone(), body));
        self.net.request(dst, req).await
    }
}

/// A scenario: a limiter store served on a simulated fabric, the SUT's
/// production client on it behind [`Recording`].
struct Scene {
    h: Harness,
    alice: Agent,
    bob: Agent,
    net: SimulatedHttpNetwork,
    store: Arc<CallStore>,
    sent: Arc<Mutex<Vec<Sent>>>,
    b2bua: B2buaSut,
    _server: Box<dyn HttpServerHandle>,
}

impl Scene {
    async fn new(name: &str) -> Self {
        let h = Harness::new(name);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let net = SimulatedHttpNetwork::new();
        let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
        let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        let handle = net.serve(laddr(), server).await.expect("limiter binds");
        let sent = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(Recording { net: net.clone(), sent: sent.clone() });
        let client: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::new(transport, laddr(), ADMIT_BUDGET));
        let decision = Arc::new(
            ScriptedDecisionEngine::builder()
                .fallback(|_| {
                    let mut r = route_to("127.0.0.1", 5070);
                    r.call_limiter = HOLDS
                        .iter()
                        .map(|(id, limit)| CallLimiterEntry { id: (*id).into(), limit: *limit })
                        .collect();
                    NewCallResponse::Route(r)
                })
                .build(),
        );
        let b2bua = B2buaSut::builder(decision)
            .limiter(client)
            .limiter_store(store.clone())
            .tune(|c| c.keepalive_interval_sec = 3_600)
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;
        Self { h, alice, bob, net, store, sent, b2bua, _server: handle }
    }

    /// INVITE → 200 → ACK, relayed. Returns the dialog and how long the
    /// INVITE took from the caller to the callee.
    async fn establish(&self) -> (Dialog, Duration) {
        let start = Instant::now();
        let mut call =
            self.alice.invite(&self.bob).with_sdp(OFFER).through(self.b2bua.addr).send().await;
        let mut uas = self.bob.receive("INVITE").await;
        let took = start.elapsed();
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        let dialog = call.ack().await;
        self.bob.receive("ACK").await;
        (dialog, took)
    }

    /// The caller's BYE, answered by the callee and relayed back.
    async fn hang_up(&self, dialog: &mut Dialog) {
        let mut bye = dialog.bye().await;
        self.bob.receive("BYE").await.respond(200, "OK").await;
        bye.expect(200).await;
        b2bua_harness::advance(SIMULATED_TRANSIT_DELAY_MS).await;
    }

    /// Requests the client sent on `path` so far.
    fn sent_on(&self, path: &str) -> usize {
        self.sent.lock().unwrap().iter().filter(|(p, _)| p == path).count()
    }

    /// The limiter key of every admit sent, in order.
    fn admitted_keys(&self) -> Vec<String> {
        let sent = self.sent.lock().unwrap();
        sent.iter()
            .filter(|(p, _)| p == "/v1/admit")
            .map(|(_, body)| body["key"].as_str().expect("an admit names its key").to_string())
            .collect()
    }

    /// Every key a release request named, in order.
    fn released_keys(&self) -> Vec<String> {
        let sent = self.sent.lock().unwrap();
        sent.iter()
            .filter(|(p, _)| p == "/v1/release")
            .flat_map(|(_, body)| {
                body["keys"]
                    .as_array()
                    .expect("a release names its keys")
                    .iter()
                    .map(|k| k.as_str().expect("a key is a string").to_string())
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// The live count of every id the calls hold.
    fn holds(&self) -> Vec<i64> {
        HOLDS.iter().map(|(id, _)| self.store.held(id)).collect()
    }

    /// Settle until the calls are reaped and the store drained, then run
    /// the reaped check.
    async fn assert_drained(&self) {
        settle_until(|| self.b2bua.is_reaped() && self.store.stats().current_total == 0).await;
        assert_eq!(self.holds(), [0, 0, 0], "every limiter drained to 0");
        self.b2bua.assert_fully_reaped();
    }
}

/// Advance `d` in the harness's settled steps, fine enough that a limiter
/// request in flight is answered inside its budget.
async fn advance(d: Duration) {
    b2bua_harness::advance(d.as_millis() as u64).await;
}

/// The distinct keys of `keys`, sorted.
fn distinct(mut keys: Vec<String>) -> Vec<String> {
    keys.sort();
    keys.dedup();
    keys
}

/// Stalled limiter: three admits time out; the fourth call's INVITE is
/// forwarded at once with no admit request, and a call counted before the
/// outage ends while the breaker is open: its release waits, unsent, until
/// the probe finds the limiter back, then leaves at once. The call admitted
/// while open sends no release; every limiter drains to 0.
#[tokio::test(start_paused = true)]
async fn a_stalled_limiter_opens_the_breaker_and_its_return_closes_it_within_one_probe() {
    let s = Scene::new("limiter-breaker-stall").await;
    let (mut counted, _) = s.establish().await;
    assert_eq!(s.holds(), [1, 1, 1], "the call before the outage is counted");

    s.net.apply_fault(Fault::Stall { dst: laddr() });
    let mut dialogs = Vec::new();
    for n in 1..=3 {
        let (dialog, took) = s.establish().await;
        assert!(took >= ADMIT_BUDGET, "call {n} waited its admit timeout ({took:?})");
        dialogs.push(dialog);
    }

    let metrics = s.b2bua.metrics();
    assert_eq!(metrics.limiter().failures_total(LimiterOp::Admit, LimiterFailure::Timeout), 3);
    assert_eq!(metrics.limiter().uncounted_calls(), 3, "each timed-out call runs uncounted");
    assert!(metrics.limiter().breaker_open(), "three failed admits open the breaker");
    assert_eq!(metrics.limiter().breaker_transitions_total(true), 1);
    let admits = s.sent_on("/v1/admit");
    let (open_call, took) = s.establish().await;
    assert_eq!(
        metrics.limiter().failures_total(LimiterOp::Admit, LimiterFailure::BreakerOpen),
        1,
        "counted as not sent"
    );
    assert_eq!(s.sent_on("/v1/admit"), admits, "the breaker is open: no admit request");
    let transit = Duration::from_millis(2 * SIMULATED_TRANSIT_DELAY_MS);
    assert!(took < transit + ADMIT_BUDGET / 2, "no admit timeout paid ({took:?})");
    dialogs.push(open_call);
    assert_eq!(metrics.limiter().uncounted_calls(), 4, "and the call the open breaker let by");

    // The counted call ends while the breaker is open: its release waits.
    let releases = s.sent_on("/v1/release");
    s.hang_up(&mut counted).await;
    advance(3 * PROBE).await;
    assert_eq!(s.sent_on("/v1/release"), releases, "no release request while open");
    assert_eq!(s.b2bua.limiter_releases_waiting(), 1, "the counted call's release waits");
    assert_eq!(s.holds(), [1, 1, 1], "the counted call is still held");

    // The limiter answers again: within one probe period the breaker
    // closes, the waiting release leaves and the next call is counted.
    s.net.apply_fault(Fault::Resume { dst: laddr() });
    advance(PROBE + Duration::from_millis(200)).await;
    assert!(!metrics.limiter().breaker_open(), "the probe closed the breaker");
    assert_eq!(metrics.limiter().breaker_transitions_total(false), 1);
    assert_eq!(s.b2bua.limiter_releases_waiting(), 0, "the release left on close");
    assert_eq!(s.holds(), [0, 0, 0], "the call counted before the outage drained");
    let (next, _) = s.establish().await;
    assert_eq!(s.holds(), [1, 1, 1], "the next call is counted");
    dialogs.push(next);

    assert_eq!(metrics.limiter().uncounted_calls(), 4, "a counted call is not in it");
    for dialog in &mut dialogs {
        s.hang_up(dialog).await;
    }
    s.assert_drained().await;
    assert_eq!(metrics.limiter().uncounted_calls(), 0, "every call's end leaves the gauge");
    let admitted = distinct(s.admitted_keys());
    assert_eq!(admitted.len(), 5, "six calls, five admits: {admitted:?}");
    assert_eq!(distinct(s.released_keys()), admitted, "only a call that sent an admit releases");
    let _ = s.h.finish().await;
}

/// Cut limiter: three admits fail at once; every call while the cut lasts
/// is forwarded with no admit request, the probe keeps failing, and within
/// one probe period of the limiter coming back the next call is counted.
/// The calls admitted while open send no release.
#[tokio::test(start_paused = true)]
async fn a_cut_limiter_opens_the_breaker_until_a_probe_answers() {
    let s = Scene::new("limiter-breaker-cut").await;
    s.net.apply_fault(Fault::Cut { dst: laddr() });
    let mut dialogs = Vec::new();
    for _ in 0..3 {
        dialogs.push(s.establish().await.0);
    }
    assert_eq!(s.sent_on("/v1/admit"), 3, "three admits failed");

    for n in 4..=5 {
        dialogs.push(s.establish().await.0);
        assert_eq!(s.sent_on("/v1/admit"), 3, "call {n} sent no admit");
        advance(2 * PROBE).await;
    }

    s.net.apply_fault(Fault::Resume { dst: laddr() });
    advance(PROBE + Duration::from_millis(200)).await;
    dialogs.push(s.establish().await.0);
    assert_eq!(s.sent_on("/v1/admit"), 4, "the breaker closed: the next call admits");
    let metrics = s.b2bua.metrics();
    assert_eq!(metrics.limiter().failures_total(LimiterOp::Admit, LimiterFailure::BreakerOpen), 2);
    assert!(metrics.limiter().failures_of(LimiterOp::Health) >= 4, "the probe failed while cut");
    assert_eq!(
        (
            metrics.limiter().breaker_transitions_total(true),
            metrics.limiter().breaker_transitions_total(false)
        ),
        (1, 1)
    );
    let text = metrics.prometheus_text();
    assert!(text.contains("b2bua_limiter_breaker_open 0"), "{text}");
    assert!(text.contains("b2bua_limiter_breaker_transitions_total{to=\"open\"} 1"), "{text}");
    for line in [
        "b2bua_limiter_failures_total{op=\"admit\",cause=\"breaker_open\"} 2",
        "b2bua_limiter_failures_total{op=\"admit\",cause=\"transport\"} 3",
        "b2bua_limiter_uncounted_calls 5",
    ] {
        assert!(text.contains(line), "{line} in {text}");
    }
    assert_eq!(s.holds(), [1, 1, 1], "the next call is counted");

    for dialog in &mut dialogs {
        s.hang_up(dialog).await;
    }
    s.assert_drained().await;
    assert_eq!(metrics.limiter().uncounted_calls(), 0);
    let admitted = distinct(s.admitted_keys());
    assert_eq!(admitted.len(), 4);
    assert_eq!(
        distinct(s.released_keys()),
        admitted,
        "the calls admitted while open release nothing"
    );
    let _ = s.h.finish().await;
}

/// Limiter stalled past the lease: a counted call's refresh stays due in the
/// held refresh batch while the breaker is open, and its set lapses on the
/// limiter. The breaker's close sends the batch at once, which re-registers
/// the set within one probe period of the limiter's return, not one refresh
/// period later.
#[tokio::test(start_paused = true)]
async fn a_refresh_held_while_open_re_registers_a_lapsed_set_on_close() {
    let s = Scene::new("limiter-breaker-held-refresh").await;
    let (mut counted, _) = s.establish().await;
    assert_eq!(s.holds(), [1, 1, 1], "the call before the outage is counted");

    s.net.apply_fault(Fault::Stall { dst: laddr() });
    let mut outage = Vec::new();
    for _ in 0..3 {
        outage.push(s.establish().await.0);
    }
    assert!(s.b2bua.metrics().limiter().breaker_open(), "three failed admits open the breaker");
    for dialog in &mut outage {
        s.hang_up(dialog).await;
    }

    advance(Duration::from_secs(130)).await;
    assert!(s.b2bua.metrics().limiter().breaker_open(), "still open while stalled");
    assert_eq!(s.sent_on("/v1/refresh"), 0, "no refresh request while open");
    assert_eq!(s.b2bua.metrics().limiter().refresh_due(), 1, "the counted call's refresh is held");
    assert_eq!(s.holds(), [0, 0, 0], "the call's set lapsed on the limiter");

    s.net.apply_fault(Fault::Resume { dst: laddr() });
    advance(PROBE + Duration::from_millis(200)).await;
    assert!(!s.b2bua.metrics().limiter().breaker_open(), "the probe closed the breaker");
    assert_eq!(s.holds(), [1, 1, 1], "the held refresh re-registered the set on close");

    s.hang_up(&mut counted).await;
    s.assert_drained().await;
    // The outage calls' releases waited past the lease and were given up:
    // the calls never held anything. Only the counted call is released.
    assert_eq!(s.b2bua.metrics().limiter().release_given_up_total(ReleaseGiveUp::LeaseExpired), 3);
    assert_eq!(s.released_keys(), s.admitted_keys()[..1]);
    let _ = s.h.finish().await;
}
