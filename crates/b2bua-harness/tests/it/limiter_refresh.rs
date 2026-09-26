//! Limiter refresh on a long call (paused clock): a counted call extends its
//! lease every `limiter_refresh_sec`, so the limiter never drops its set
//! while the call lives.
//!
//! The limiter runs a 2 s lease and the call refreshes every second. Past the
//! lease the set would have lapsed UNLESS the refresh extended it. We cross
//! the lease, sweep, then prove a second call is still refused — which can
//! only happen if the refresh kept the call counted.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{settle_until, B2buaSut};
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

fn laddr() -> SocketAddr {
    "10.0.0.1:8080".parse().unwrap()
}

#[tokio::test(start_paused = true)]
async fn refresh_keeps_a_long_call_counted_across_its_lease() {
    let h = Harness::with_transit_delay("limiter-refresh", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // A 2 s lease: a set lapses in 2 s unless refreshed.
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(CallStore::new(LimiterConfig { lease_sec: 2 }, Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let limiter: Arc<dyn CallLimiter> =
        Arc::new(HttpCallLimiter::new(Arc::new(http.clone()), laddr(), Duration::from_millis(150)));
    let decision: Arc<dyn CallDecisionEngine> = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![CallLimiterEntry { id: "trunk-A".into(), limit: 1 }];
                NewCallResponse::Route(r)
            })
            .build(),
    );

    // Refresh once per second, inside the lease.
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter)
        .limiter_store(store.clone())
        .tune(|c| {
            c.limiter_refresh_sec = 1;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // Establish the long call.
    let mut call1 = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call1.expect(200).await;
    let mut dialog1 = call1.ack().await;
    bob.receive("ACK").await;
    assert_eq!(store.stats().current_total, 1);

    // Cross the lease with the refresh timer firing every second: the lease
    // is extended each time, so the set never lapses.
    h.advance(Duration::from_millis(3500)).await;
    assert_eq!(store.sweep_now(), 0, "the refreshed set never lapsed");
    assert_eq!(store.stats().lease_expired_calls, 0);

    // The call is still counted → a second call is refused. (Without the
    // refresh the set would have lapsed at 2 s and this would admit.)
    let mut call2 = carol.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    assert_eq!(call2.expect(486).await.status(), 486, "the refresh kept the call counted");

    let mut bye = dialog1.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
    let _ = h.finish().await;
}
