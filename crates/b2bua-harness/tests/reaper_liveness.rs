//! Reaper liveness policy (ADR-0020 X4 refinement): only **real SIP traffic**
//! refreshes a call's last-touched stamp — a received message, or a turn that
//! sent SIP out. Self-generated housekeeping turns that touch no wire (the
//! per-call `LimiterRefresh` HTTP migration) must NOT count as liveness.
//!
//! Endurance 2026-06-12: ~169 crash-orphaned calls sat `Active` for a full
//! hour holding limiter slots. The reaper swept them ~20 times and never
//! issued a verdict, because each call's own `LimiterRefresh` fire (every
//! 300 s, < the 900 s idle threshold) refreshed its stamp — the call kept
//! itself "alive" without a single SIP message in either direction.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{establish_call, settle_until, B2buaSut};
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;

const LIMITER_ADDR: &str = "10.0.0.1:8080";

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

async fn serve_limiter(net: &SimulatedHttpNetwork) -> (Arc<WindowStore>, Box<dyn HttpServerHandle>) {
    let store = Arc::new(WindowStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = net.serve(laddr(), server).await.unwrap();
    (store, handle)
}

fn limiter_client(net: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(
        Arc::new(net.clone()),
        laddr(),
        Duration::from_millis(150),
    ))
}

fn route_with_limiter(host: &str, port: u16, id: &str, limit: i64) -> Arc<dyn CallDecisionEngine> {
    let host = host.to_string();
    let id = id.to_string();
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to(&host, port);
                r.call_limiter = vec![CallLimiterEntry { id: id.clone(), limit }];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

fn reason_of(cdr: &b2bua::cdr::CdrRecord) -> Vec<String> {
    cdr.events.iter().filter_map(|e| e.reason.clone()).collect()
}

/// A silent call whose only "activity" is its own `LimiterRefresh` timer must
/// still go stale and be reaped — and the reap must release the limiter hold.
/// Pre-fix the 10 s refresh cadence re-stamps the ledger forever, the sweep
/// never sees it idle, and the hold is pinned until GlobalDuration.
#[tokio::test(start_paused = true)]
async fn limiter_refresh_self_touch_does_not_mask_staleness() {
    let h = Harness::with_transit_delay("reaper-liveness-refresh", 0);
    let alice = h.agent("alice", "127.0.0.1:5062").await;
    let bob = h.agent("bob", "127.0.0.1:5072").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    let decision = route_with_limiter("127.0.0.1", 5072, "trunk-A", 1);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .tune(|c| {
            // No keepalive inside the horizon; an aggressive refresh cadence
            // (10 s << idle 60 s) so the self-touch masking is fully exercised.
            c.keepalive_interval_sec = 3_600;
            c.reaper_idle_max_sec = 60;
            c.reaper_sweep_interval_sec = 30;
            c.limiter_refresh_sec = 10;
        })
        .start(&h, "b2bua", "127.0.0.1:5082")
        .await;

    let _dialog = establish_call(&alice, &bob, b2bua.addr).await;
    assert_eq!(store.stats().current_total, 1, "limiter hold taken");

    // Both UAs go silent forever. Only the LimiterRefresh timer fires (every
    // 10 s). Past idle_max + a sweep the call must be reaped regardless.
    h.advance(Duration::from_secs(95)).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;

    let cdrs = b2bua.cdr_records();
    assert_eq!(
        cdrs.len(),
        1,
        "a SIP-silent call must be reaped even while its LimiterRefresh keeps firing",
    );
    assert!(
        reason_of(&cdrs[0]).iter().any(|r| r == "reaper-stale"),
        "the CDR names the reap: {:?}",
        reason_of(&cdrs[0])
    );

    // The reap settles the obligations: the limiter hold is released.
    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "reap released the limiter hold");

    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// Received SIP **does** refresh liveness: a call exchanging in-dialog INFO
/// well past the idle threshold stays alive; once it actually goes silent it
/// is reaped one idle window later.
#[tokio::test(start_paused = true)]
async fn received_sip_refreshes_liveness() {
    let h = Harness::with_transit_delay("reaper-liveness-received", 0);
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5073));
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_idle_max_sec = 60;
            c.reaper_sweep_interval_sec = 30;
        })
        .start(&h, "b2bua", "127.0.0.1:5083")
        .await;

    let mut dialog = establish_call(&alice, &bob, b2bua.addr).await;

    // In-dialog INFO every 40 s (inside the 60 s idle window): the received
    // traffic must keep the call off the reaper's stale list.
    for _ in 0..3 {
        h.advance(Duration::from_secs(40)).await;
        let mut info = dialog.request(InDialogMethod::Info, None).await;
        bob.receive("INFO").await.respond(200, "OK").await;
        info.expect(200).await;
    }
    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "call exchanging SIP stays alive past 120 s of wall time",
    );
    assert!(b2bua.cdr_records().is_empty(), "no premature reap");

    // Now go truly silent: one idle window + a sweep later it is reaped.
    h.advance(Duration::from_secs(95)).await;
    settle_until(|| b2bua.cdr_records().len() == 1).await;
    assert_eq!(b2bua.cdr_records().len(), 1, "silent call reaped after the idle window");
    settle_until(|| b2bua.active_calls() == 0).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
