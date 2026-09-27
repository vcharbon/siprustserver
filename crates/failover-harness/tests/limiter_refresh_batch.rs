//! A bulk reclaim refreshes the calls it materialises in batches.
//!
//! A rebooted primary reclaims its whole partition at once, and every counted
//! call's refresh is past due: each falls due on the worker's refresh batch,
//! which sends every key due in one request per `limiter_refresh_batch_max`
//! keys, instead of one request per call.
//!
//! Every call holds three limiters.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use failover_harness::{
    assert_call_fully_over, cookie_field, FailoverHarness, ReplicatedB2buaSut, WorkerHealth,
};
use http_net::{
    BindError, HttpError, HttpRequest, HttpResponse, HttpServerHandle, HttpService, HttpTransport,
    SimulatedHttpNetwork,
};
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const PROXY: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The workers' refresh period.
const REFRESH_SEC: i64 = 5;
/// Most keys one refresh request carries.
const BATCH_MAX: usize = 2;
/// Calls placed: enough that each worker is the primary of several.
const CALLS: usize = 8;

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// One request a worker's limiter client sent: its path and its JSON body.
type Sent = (String, serde_json::Value);

/// The limiter's fabric behind a log of every request one worker sends.
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

/// One worker's limiter log.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<Sent>>>);

impl Log {
    fn client(&self, net: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
        let transport = Arc::new(Recording { net: net.clone(), sent: self.0.clone() });
        Arc::new(HttpCallLimiter::new(transport, laddr(), Duration::from_millis(150)))
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// The refresh requests sent since entry `from` of the log.
    fn refreshes_since(&self, from: usize) -> Vec<serde_json::Value> {
        let sent = self.0.lock().unwrap();
        sent[from..].iter().filter(|(p, _)| p == "/v1/refresh").map(|(_, b)| b.clone()).collect()
    }
}

/// Every key the refresh requests `bodies` name.
fn keys_of(bodies: &[serde_json::Value]) -> Vec<String> {
    let mut keys: Vec<String> = bodies
        .iter()
        .flat_map(|b| {
            b["calls"]
                .as_array()
                .map(|calls| {
                    calls
                        .iter()
                        .map(|c| c["key"].as_str().unwrap_or_default().to_string())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| vec![b["key"].as_str().unwrap_or_default().to_string()])
        })
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

fn decision() -> Arc<dyn CallDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_| {
                let mut r = route_to("127.0.0.1", 5070);
                r.new_ruri = None;
                r.call_limiter = ["x", "y", "z"]
                    .iter()
                    .map(|id| CallLimiterEntry { id: (*id).into(), limit: 100 })
                    .collect();
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// A primary crashes with its counted calls silent past their refresh, then
/// reboots and reclaims them all. Every reclaimed call's refresh is due at
/// once; the primary refreshes all of them in at most
/// `ceil(calls / BATCH_MAX)` requests. Every call ends and drains.
#[tokio::test(start_paused = true)]
async fn a_bulk_reclaim_refreshes_its_counted_calls_in_batches() {
    let mut fh = FailoverHarness::new("limiter-refresh-batch-reclaim", &["b1", "b2"])
        .with_worker_tune(|c| {
            c.limiter_refresh_sec = REFRESH_SEC;
            c.limiter_refresh_batch_max = BATCH_MAX;
        });
    let alice = fh.agent("alice", "127.0.0.1:5060").await;
    let bob = fh.agent("bob", "127.0.0.1:5070").await;
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _server = http.serve(laddr(), server).await.unwrap();
    let logs = [Log::default(), Log::default()];

    let proxy =
        fh.spawn_proxy(PROXY, &[("b1", B1.parse().unwrap()), ("b2", B2.parse().unwrap())]).await;
    let mut w_b1 = fh
        .spawn_worker_limited(
            "b1",
            "b1",
            B1,
            &["b2"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            decision(),
            logs[0].client(&http),
        )
        .await;
    let mut w_b2 = fh
        .spawn_worker_limited(
            "b2",
            "b2",
            B2,
            &["b1"],
            ("127.0.0.1", 5070),
            ("127.0.0.1", 5080),
            decision(),
            logs[1].client(&http),
        )
        .await;
    fh.advance(Duration::from_millis(500)).await;
    assert!(w_b1.is_ready() && w_b2.is_ready(), "workers ready");

    let mut dialogs = Vec::new();
    let mut primaries = Vec::new();
    for _ in 0..CALLS {
        let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
        let mut uas = bob.receive("INVITE").await;
        primaries.push(cookie_field(uas.request(), "w_pri").unwrap_or_default());
        uas.respond(200, "OK").with_sdp(ANSWER).await;
        call.expect(200).await;
        dialogs.push(call.ack().await);
        bob.receive("ACK").await;
    }
    fh.advance(Duration::from_millis(500)).await;
    assert_eq!(store.held("x"), CALLS as i64, "every call is counted");

    // The primary of the most calls crashes and reboots.
    let on_b1 = primaries.iter().filter(|p| *p == "b1").count();
    let primary_ord = if on_b1 * 2 >= CALLS { "b1" } else { "b2" };
    let reclaimed = primaries.iter().filter(|p| *p == primary_ord).count();
    assert!(reclaimed >= CALLS / 2, "{primary_ord} is the primary of {reclaimed} calls");
    let log = if primary_ord == "b1" { logs[0].clone() } else { logs[1].clone() };
    let (primary, backup): (&mut ReplicatedB2buaSut, &ReplicatedB2buaSut) =
        if primary_ord == "b1" { (&mut w_b1, &w_b2) } else { (&mut w_b2, &w_b1) };
    let mut call_refs = Vec::new();
    for _ in 0..50 {
        call_refs = backup.scan_backed_up(primary_ord);
        if call_refs.len() == reclaimed {
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    assert_eq!(call_refs.len(), reclaimed, "the backup holds every call of {primary_ord}");

    // ── the primary crashes; its calls stay silent past their refresh ────
    primary.crash();
    proxy.set_health(primary_ord, WorkerHealth::Dead);
    backup.simulate_peer_removed(primary_ord);
    fh.advance(Duration::from_secs(2 * REFRESH_SEC as u64)).await;

    // ── it reboots and reclaims them all ─────────────────────────────────
    let from = log.len();
    let new_addr = primary.reboot().await;
    proxy.set_address(primary_ord, new_addr);
    fh.note_worker_rebound(primary_ord, new_addr);
    backup.simulate_peer_added(primary_ord);
    for _ in 0..40 {
        fh.advance(Duration::from_millis(500)).await;
        if primary.is_ready() {
            break;
        }
    }
    assert!(primary.is_ready(), "rebooted primary re-hydrated from the backup");
    proxy.set_health(primary_ord, WorkerHealth::Alive);

    // Every reclaimed call's refresh leaves, and in batches.
    for _ in 0..(10 * REFRESH_SEC) {
        if keys_of(&log.refreshes_since(from)).len() >= reclaimed {
            break;
        }
        fh.advance(Duration::from_millis(100)).await;
    }
    let bodies = log.refreshes_since(from);
    assert_eq!(keys_of(&bodies).len(), reclaimed, "every reclaimed call refreshed");
    let bound = reclaimed.div_ceil(BATCH_MAX);
    assert!(
        bodies.len() <= bound,
        "{reclaimed} reclaimed calls refreshed in {} requests (at most {bound})",
        bodies.len()
    );

    for dialog in &mut dialogs {
        scenario_harness::callflow::hangup(dialog, &bob).await;
    }
    let drained = fh
        .settle_terminal(async || {
            store.stats().current_total == 0
                && w_b1.cdr_records().len() + w_b2.cdr_records().len() == CALLS
        })
        .await;
    assert!(drained, "every call ended and drained; store {}", store.stats().current_total);
    for call_ref in &call_refs {
        assert_call_fully_over(&[&w_b1, &w_b2], call_ref, &store).await;
    }
}
