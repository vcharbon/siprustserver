//! Limiter refresh on a long call (paused clock): an admitted call's hold is
//! migrated to the current window every `limiter_refresh_sec`, so it never ages
//! out of the summed lookback.
//!
//! The limiter is configured with `active_windows = 1` (only the current window
//! counts) and `window_sec = 1`. So once the clock crosses a window boundary,
//! the original hold would stop counting UNLESS the refresh timer migrated it.
//! We cross the boundary, then prove a second call is still rejected — which can
//! only happen if the refresh kept the slot occupied.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::B2buaSut;
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

fn laddr() -> SocketAddr {
    "10.0.0.1:8080".parse().unwrap()
}

#[tokio::test(start_paused = true)]
async fn refresh_keeps_a_long_call_counted_across_a_window() {
    let h = Harness::with_transit_delay("limiter-refresh", 1);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    // 1 s windows, only the current window counts → a hold ages out in 1 s
    // unless refreshed.
    let http = SimulatedHttpNetwork::new();
    let store = Arc::new(WindowStore::new(
        LimiterConfig {
            window_sec: 1,
            active_windows: 1,
            ttl_sec: 60,
        },
        Clock::test_at(0),
    ));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let _lh: Box<dyn HttpServerHandle> = http.serve(laddr(), server).await.unwrap();

    let limiter: Arc<dyn CallLimiter> = Arc::new(HttpCallLimiter::new(
        Arc::new(http.clone()),
        laddr(),
        Duration::from_millis(150),
    ));
    let decision: Arc<dyn CallDecisionEngine> = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(|_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![CallLimiterEntry {
                    id: "trunk-A".into(),
                    limit: 1,
                }];
                NewCallResponse::Route(r)
            })
            .build(),
    );

    // Refresh once per second (matches the window).
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter)
        .tune(|c| {
            c.limiter_refresh_sec = 1;
        })
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // Establish the long call (hold in window 0).
    let mut call1 = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call1.expect(200).await;
    let _dialog1 = call1.ack().await;
    bob.receive("ACK").await;
    assert_eq!(store.stats().current_total, 1);

    // Cross the window boundary AND fire the 1 s refresh timer. With
    // active_windows=1 the window-0 hold would stop counting at t=1 s; the
    // refresh must migrate it to window 1.
    h.advance(Duration::from_millis(1500)).await;

    // The hold migrated, so the slot is still occupied → a second call is
    // rejected. (Without refresh, window 1 would be empty and this would admit.)
    let mut call2 = carol.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    assert_eq!(call2.expect(486).await.status, 486, "refresh kept the slot occupied");

    let _ = h.finish().await;
}
