//! The worker's limiter refresh batch: a counted call's refresh never waits
//! on the limiter in the call's turn.
//!
//! The `LimiterRefresh` timer marks the call's key due; within one refresh
//! tick every key due leaves in one request, whatever the number of counted
//! calls; the limiter's answer for each key reaches its call on the call's own
//! turn. A stalled limiter so delays no call; a refresh the limiter did not
//! answer is sent again at the next tick; a set that lapsed is re-registered
//! within one tick of its refresh; an answer that finds its call ended is
//! dropped.
//!
//! Every call holds three limiters.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::CallLimiter;
use b2bua::limiter::RefreshOutcome;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua::metrics::{LimiterOp, RefreshGiveUp};
use b2bua::B2buaConfig;
use b2bua_harness::{settle_until, B2buaSut};
use call_limiter::wire::AdmitEntry;
use call_limiter::{AdmitResult, CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{
    BindError, Fault, HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpService,
    HttpTransport, SimulatedHttpNetwork,
};
use scenario_harness::{Agent, Dialog, Harness};
use sip_clock::Clock;
use tokio::time::Instant;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

/// The production admit and refresh budget.
const BUDGET: Duration = Duration::from_millis(150);
/// The workers' refresh period in these scenarios.
const REFRESH: Duration = Duration::from_secs(5);
/// The worker's refresh tick (the config default).
const TICK: Duration = Duration::from_secs(1);
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

/// The limiter answering every refresh `delay` after it applied it.
struct SlowRefresh {
    inner: Arc<LimiterServer>,
    delay: Duration,
}

#[async_trait]
impl HttpService for SlowRefresh {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        let refresh = req.path == "/v1/refresh";
        let resp = self.inner.handle(req).await;
        if refresh {
            tokio::time::sleep(self.delay).await;
        }
        resp
    }
}

/// How a scene's limiter and worker run.
struct Setup {
    lease_sec: i64,
    /// The limiter answers every refresh this long after it applied it.
    refresh_delay: Option<Duration>,
}

impl Default for Setup {
    fn default() -> Self {
        Self { lease_sec: 120, refresh_delay: None }
    }
}

/// A scenario: a limiter store served on a simulated fabric, the SUT's
/// production client on it behind [`Recording`], refreshing every
/// [`REFRESH`].
struct Scene {
    h: Harness,
    alice: Agent,
    bob: Agent,
    net: SimulatedHttpNetwork,
    store: Arc<CallStore>,
    sent: Arc<Mutex<Vec<Sent>>>,
    b2bua: B2buaSut,
    /// When the scene started: the refresh timers count from here.
    start: Instant,
    _server: Box<dyn HttpServerHandle>,
}

impl Scene {
    async fn new(name: &str, setup: Setup) -> Self {
        Self::tuned(name, setup, |_| {}).await
    }

    async fn tuned(
        name: &str,
        setup: Setup,
        tune: impl Fn(&mut B2buaConfig) + Send + Sync + 'static,
    ) -> Self {
        let h = Harness::with_transit_delay(name, 1);
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let bob = h.agent("bob", "127.0.0.1:5070").await;
        let net = SimulatedHttpNetwork::new();
        let cfg = LimiterConfig { lease_sec: setup.lease_sec };
        let store = Arc::new(CallStore::new(cfg, Clock::test_at(0)));
        let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
        let service: Arc<dyn HttpService> = match setup.refresh_delay {
            Some(delay) => Arc::new(SlowRefresh { inner: server, delay }),
            None => server,
        };
        let handle = net.serve(laddr(), service).await.expect("limiter binds");
        let sent = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(Recording { net: net.clone(), sent: sent.clone() });
        let client: Arc<dyn CallLimiter> =
            Arc::new(HttpCallLimiter::new(transport, laddr(), BUDGET));
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
            .tune(move |c| {
                c.keepalive_interval_sec = 3_600;
                c.limiter_refresh_sec = REFRESH.as_secs() as i64;
                tune(c);
            })
            .start(&h, "b2bua", "127.0.0.1:5080")
            .await;
        let start = Instant::now();
        Self { h, alice, bob, net, store, sent, b2bua, start, _server: handle }
    }

    /// INVITE → 200 → ACK, relayed.
    async fn establish(&self) -> Dialog {
        let mut call =
            self.alice.invite(&self.bob).with_sdp(OFFER).through(self.b2bua.addr).send().await;
        self.bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        let dialog = call.ack().await;
        self.bob.receive("ACK").await;
        dialog
    }

    /// The caller's BYE, answered by the callee and relayed back. Returns
    /// how long the BYE took from the caller to the callee.
    async fn hang_up(&self, dialog: &mut Dialog) -> Duration {
        let sent_at = Instant::now();
        let mut bye = dialog.bye().await;
        let mut uas = self.bob.receive("BYE").await;
        let took = sent_at.elapsed();
        uas.respond(200, "OK").await;
        bye.expect(200).await;
        took
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

    /// Every key a refresh request named, in order.
    fn refreshed_keys(&self) -> Vec<String> {
        let sent = self.sent.lock().unwrap();
        sent.iter()
            .filter(|(p, _)| p == "/v1/refresh")
            .flat_map(|(_, body)| {
                body["calls"]
                    .as_array()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|c| c["key"].as_str().unwrap_or_default().to_string())
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
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

    /// Advance until `at` from the scene's start, in the harness's settled
    /// steps.
    async fn advance_to(&self, at: Duration) {
        let now = self.start.elapsed();
        if at > now {
            b2bua_harness::advance((at - now).as_millis() as u64).await;
        }
    }

    /// Advance in 10 ms steps until a refresh request has left, at most
    /// `bound` from now.
    async fn until_a_refresh_leaves(&self, bound: Duration) {
        let before = self.sent_on("/v1/refresh");
        let deadline = Instant::now() + bound;
        while self.sent_on("/v1/refresh") == before {
            assert!(Instant::now() < deadline, "no refresh request left within {bound:?}");
            b2bua_harness::advance(10).await;
        }
    }

    /// Settle until the calls are reaped and the store drained, then run
    /// the reaped check.
    async fn assert_drained(&self) {
        settle_until(|| self.b2bua.is_reaped() && self.store.stats().current_total == 0).await;
        assert_eq!(self.holds(), [0, 0, 0], "every limiter drained to 0");
        self.b2bua.assert_fully_reaped();
    }
}

/// The distinct keys of `keys`, sorted.
fn distinct(mut keys: Vec<String>) -> Vec<String> {
    keys.sort();
    keys.dedup();
    keys
}

/// Five counted calls: their refresh timers fall due within one tick, and
/// one request refreshes all five; the next period, one request again.
#[tokio::test(start_paused = true)]
async fn counted_calls_refresh_in_one_request_per_tick() {
    let s = Scene::new("refresh-batch-one-request", Setup::default()).await;
    let mut dialogs = Vec::new();
    for _ in 0..5 {
        dialogs.push(s.establish().await);
    }
    assert_eq!(s.holds(), [5, 5, 5], "five counted calls");
    let keys = distinct(s.admitted_keys());
    assert_eq!(keys.len(), 5);

    s.advance_to(REFRESH + TICK + Duration::from_millis(500)).await;
    assert_eq!(s.sent_on("/v1/refresh"), 1, "five counted calls refresh in one request");
    assert_eq!(distinct(s.refreshed_keys()), keys, "the request names every counted call");

    s.advance_to(2 * REFRESH + TICK + Duration::from_millis(500)).await;
    assert_eq!(s.sent_on("/v1/refresh"), 2, "one request per period");
    assert_eq!(s.refreshed_keys().len(), 10, "each call once per period");
    assert_eq!(s.store.stats().lease_expired_calls, 0);

    for dialog in &mut dialogs {
        s.hang_up(dialog).await;
    }
    s.assert_drained().await;
    assert_eq!(distinct(s.released_keys()), keys);
    let _ = s.h.finish().await;
}

/// The limiter stalls when a counted call's refresh falls due. The refresh
/// waits its budget off the call: the caller's BYE, sent while the refresh
/// is in flight, is relayed at once. The call ends and drains once the
/// limiter is back.
#[tokio::test(start_paused = true)]
async fn a_stalled_limiter_delays_no_call_s_turn() {
    let s = Scene::new("refresh-batch-stalled", Setup::default()).await;
    let mut dialog = s.establish().await;
    assert_eq!(s.holds(), [1, 1, 1]);

    s.advance_to(REFRESH - Duration::from_millis(100)).await;
    s.net.apply_fault(Fault::Stall { dst: laddr() });
    s.until_a_refresh_leaves(TICK + Duration::from_millis(500)).await;
    let took = s.hang_up(&mut dialog).await;
    assert!(
        took < BUDGET / 3,
        "the BYE was relayed while the refresh waited on the stalled limiter ({took:?})"
    );

    s.net.apply_fault(Fault::Resume { dst: laddr() });
    s.assert_drained().await;
    assert_eq!(s.released_keys(), s.admitted_keys());
    let _ = s.h.finish().await;
}

/// The limiter is cut when a counted call's refresh falls due, and back just
/// after: the refresh it never answered is sent again at the next tick, not
/// one refresh period later.
#[tokio::test(start_paused = true)]
async fn a_refresh_the_limiter_did_not_answer_is_sent_again_at_the_next_tick() {
    let s = Scene::new("refresh-batch-retry", Setup::default()).await;
    let mut dialog = s.establish().await;
    let key = s.admitted_keys()[0].clone();

    s.advance_to(REFRESH - Duration::from_millis(100)).await;
    s.net.apply_fault(Fault::Cut { dst: laddr() });
    s.until_a_refresh_leaves(TICK + Duration::from_millis(500)).await;
    s.net.apply_fault(Fault::Resume { dst: laddr() });
    let resumed = s.start.elapsed();

    s.advance_to(resumed + TICK + Duration::from_millis(200)).await;
    assert_eq!(s.sent_on("/v1/refresh"), 2, "the refresh is sent again within one tick");
    assert_eq!(s.refreshed_keys(), [key.clone(), key.clone()]);

    s.hang_up(&mut dialog).await;
    s.assert_drained().await;
    assert_eq!(s.released_keys(), [key]);
    let _ = s.h.finish().await;
}

/// The limiter's lease (1 s) is shorter than a refresh tick: even at a third
/// of the lease, a refresh leaves up to one tick after it falls due, so the
/// call's set lapses before each refresh and the refresh re-registers it
/// within one tick of falling due.
#[tokio::test(start_paused = true)]
async fn a_lapsed_set_is_re_registered_within_one_tick_of_its_refresh() {
    let setup = Setup { lease_sec: 1, ..Setup::default() };
    let s = Scene::new("refresh-batch-reregister", setup).await;
    let mut dialog = s.establish().await;
    assert_eq!(s.holds(), [1, 1, 1]);

    // The refresh falls due a third of the lease after the admit and leaves
    // one tick later; the set lapsed at one lease.
    let due = Duration::from_millis(1_000 / 3);
    s.advance_to(Duration::from_millis(1_100)).await;
    assert_eq!(s.holds(), [0, 0, 0], "the set lapsed with its lease");
    s.advance_to(due + TICK + Duration::from_millis(200)).await;
    assert_eq!(s.holds(), [1, 1, 1], "the refresh re-registered the set within one tick");
    assert_eq!(s.store.stats().reregistered_calls, 1);
    assert_eq!(s.b2bua.metrics().limiter().refresh_answers_total(RefreshOutcome::Reregistered), 1);

    s.hang_up(&mut dialog).await;
    s.assert_drained().await;
    let _ = s.h.finish().await;
}

/// An admit of the call's key dropped its set on the limiter behind the
/// call's back, so its refresh answers `dropped`; the limiter answers late,
/// and the call ends while the answer is on its way. The answer changes
/// nothing: one release, one CDR, nothing left behind.
#[tokio::test(start_paused = true)]
async fn a_dropped_answer_reaching_an_ended_call_is_harmless() {
    let setup = Setup { refresh_delay: Some(Duration::from_millis(100)), ..Setup::default() };
    let s = Scene::new("refresh-batch-dropped-after-end", setup).await;
    let mut dialog = s.establish().await;
    let key = s.admitted_keys()[0].clone();

    s.advance_to(REFRESH - Duration::from_millis(100)).await;
    let no_entries: &[AdmitEntry] = &[];
    assert_eq!(s.store.admit(&key, no_entries, false), AdmitResult::Admitted);
    assert_eq!(s.holds(), [0, 0, 0], "the key's set is dropped and fenced");
    s.until_a_refresh_leaves(TICK + Duration::from_millis(500)).await;
    s.hang_up(&mut dialog).await;
    b2bua_harness::advance(300).await;

    s.assert_drained().await;
    assert_eq!(s.released_keys(), [key]);
    let metrics = s.b2bua.metrics();
    assert_eq!(
        metrics.limiter().refresh_answers_total(RefreshOutcome::Dropped),
        1,
        "the answer was counted"
    );
    assert_eq!(metrics.limiter().uncounted_calls(), 0, "and applied to nothing");
    assert_eq!(
        metrics.limiter().refresh_given_up_total(RefreshGiveUp::Released),
        1,
        "the call's release forgot it"
    );
    let _ = s.h.finish().await;
}

/// The limiter answers refreshes in 300 ms, past the admit budget: no call
/// waits on a refresh, so the refresh runs under its own longer budget, and a
/// `dropped` answer reaches its call, which refreshes no more.
#[tokio::test(start_paused = true)]
async fn a_refresh_answered_slower_than_the_admit_budget_reaches_its_call() {
    let setup = Setup { refresh_delay: Some(Duration::from_millis(300)), ..Setup::default() };
    let s = Scene::new("refresh-batch-slow-limiter", setup).await;
    let mut dialog = s.establish().await;
    let key = s.admitted_keys()[0].clone();

    s.advance_to(REFRESH - Duration::from_millis(100)).await;
    let no_entries: &[AdmitEntry] = &[];
    assert_eq!(s.store.admit(&key, no_entries, false), AdmitResult::Admitted);
    s.advance_to(REFRESH + TICK + Duration::from_secs(1)).await;
    let metrics = s.b2bua.metrics();
    assert_eq!(metrics.limiter().failures_of(LimiterOp::Refresh), 0, "the slow answer came back");
    assert_eq!(
        metrics.limiter().uncounted_calls(),
        1,
        "the dropped answer reached its call, which runs uncounted"
    );
    let refreshes = s.sent_on("/v1/refresh");
    s.advance_to(2 * REFRESH + TICK + Duration::from_secs(1)).await;
    assert_eq!(s.sent_on("/v1/refresh"), refreshes, "an uncounted call refreshes no more");

    s.hang_up(&mut dialog).await;
    s.assert_drained().await;
    assert_eq!(s.released_keys(), [key], "the uncounted call still releases its key");
    assert_eq!(metrics.limiter().uncounted_calls(), 0, "the call's end leaves the gauge");
    let _ = s.h.finish().await;
}

/// The limiter is cut while the breaker stays closed (no admit is sent, and
/// only admits trip it): the batch backs off on consecutive unanswered
/// requests instead of resending every tick. Once the limiter is back, the
/// next retry is answered and the backoff is gone: the next refresh leaves
/// within one tick of falling due.
#[tokio::test(start_paused = true)]
async fn a_dead_limiter_is_retried_under_a_backoff_and_answered_once_back() {
    let s = Scene::new("refresh-batch-backoff", Setup::default()).await;
    let mut dialog = s.establish().await;

    s.advance_to(REFRESH - Duration::from_millis(100)).await;
    s.net.apply_fault(Fault::Cut { dst: laddr() });
    s.advance_to(REFRESH + Duration::from_secs(30)).await;
    let sent = s.sent_on("/v1/refresh");
    assert!(sent <= 10, "{sent} refresh requests in 30 s of a dead limiter (one per tick: 30)");
    assert!(!s.b2bua.metrics().limiter().breaker_open(), "refreshes never trip the breaker");

    s.net.apply_fault(Fault::Resume { dst: laddr() });
    let resumed = s.start.elapsed();
    // The backoff waits 5 s at most; the call's own refresh is due at 40 s.
    s.advance_to(resumed + Duration::from_secs(5) + Duration::from_millis(100)).await;
    let metrics = s.b2bua.metrics();
    assert_eq!(answered_refreshes(metrics), 1, "the retry is answered");
    assert_eq!(s.holds(), [1, 1, 1], "the call stays counted");

    // The next refresh falls due at 40 s and leaves within one tick.
    s.advance_to(8 * REFRESH + TICK + Duration::from_millis(200)).await;
    assert_eq!(answered_refreshes(metrics), 2, "no backoff left");

    s.hang_up(&mut dialog).await;
    s.assert_drained().await;
    let _ = s.h.finish().await;
}

/// Refresh requests the limiter answered.
fn answered_refreshes(metrics: &b2bua::metrics::B2buaMetrics) -> u64 {
    let m = metrics.limiter();
    m.requests_total(LimiterOp::Refresh) - m.failures_of(LimiterOp::Refresh)
}
