//! Setup-timeout (a-leg initial-INVITE transaction deadline).
//!
//! Endurance 2026-06-12: a `kill_worker` crash caught ~169 calls **mid-setup**
//! (b-leg INVITE sent, no final response yet). Their only timers were the
//! route-time `GlobalDuration` (1 h) + `LimiterRefresh`, so the reclaimed
//! copies sat `Active` for a full hour — holding their **limiter slots** (the
//! cap20 SIPp stream pinned at 15/20 for ~50 min) and refreshing them every
//! 300 s, invisible to the reaper. The per-b-leg `NoAnswer` timer is NOT the
//! fix: it is route-supplied (the endurance adapter supplies `None`) and
//! reroute/failover creates fresh b-legs, each needing its own.
//!
//! The fix is a single **call-level `SetupTimeout`** anchored on the calling
//! leg: armed at route time (so it rides the replicated `call.timers` ledger
//! and survives crash → reclaim), untouched by reroutes, cancelled at answer.
//! On fire for a still-unanswered call: 408 to the a-leg, CANCEL the pending
//! b-leg(s), terminate — which settles the obligations (limiter decrement +
//! CDR) through the ordinary enforce path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallLimiterEntry, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua::cdr::CdrRecord;
use b2bua_harness::{settle_until, B2buaSut};
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The production default for the a-leg setup deadline (`B2buaConfig`
/// `setup_timeout_sec`): below the sip-txn `INVITE_INITIAL_TIMEOUT` (158 s)
/// so the rules path owns the teardown (clean 408 + CANCEL + obligations)
/// while the txn timer stays the lower-layer backstop. The torn-down test
/// rides the *default* deliberately: the regression is that a
/// default-configured worker leaks setup-stalled calls.
const DEFAULT_SETUP_TIMEOUT: Duration = Duration::from_secs(150);

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

fn reasons_of(cdr: &CdrRecord) -> Vec<String> {
    cdr.events.iter().filter_map(|e| e.reason.clone()).collect()
}

/// A call that rings forever (b-leg never sends a final response) must be torn
/// down at the **setup timeout** — 408 to the caller, CANCEL to the ringing
/// b-leg — and must release its limiter hold then, NOT an hour later at the
/// GlobalDuration cap. Pre-fix this wedges `Active` holding the limiter slot.
#[tokio::test(start_paused = true)]
async fn ringing_forever_is_torn_down_at_setup_timeout_and_releases_the_limiter() {
    let h = Harness::new("b2bua-setup-timeout");
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    let decision = route_with_limiter("127.0.0.1", 5070, "trunk-A", 1);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        // Isolate the timer mechanism: the reaper's liveness policy has its own
        // suite (`reaper_liveness.rs`); in production the reaper was masked by
        // the LimiterRefresh self-touch, so it must not save this test either.
        .tune(|c| c.reaper_enabled = false)
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // ── Setup stalls: bob rings, then silence (no final response ever) ───────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    assert_eq!(store.stats().current_total, 1, "limiter hold taken at route time");
    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "one call wedged in setup",
    );

    // ── At the setup deadline the call must resolve ──────────────────────────
    // 151 s: past the 150 s SetupTimeout, before the 158 s sip-txn
    // INVITE_INITIAL_TIMEOUT backstop — the ledger timer must own the teardown
    // (it is the only one of the two that survives a crash → reclaim).
    h.advance(DEFAULT_SETUP_TIMEOUT + Duration::from_secs(1)).await;

    // The caller gets its final 408 IMMEDIATELY (setup-timeout answers the a-leg
    // explicitly), and the ringing b-leg gets a CANCEL. Call teardown, however,
    // now HOLDS until that CANCEL resolves — the call-liveness ordering fix: a
    // ringing b-leg's internal CANCEL must quiesce (its 487, or a crossing 200
    // reaped by ACK+BYE) before RemoveCall, so a 200 crossing the CANCEL is never
    // stranded on a removed call. So the reap completes when bob answers 487, NOT
    // in the same turn as the setup timeout.
    let mut cancel = bob.receive("CANCEL").await;
    cancel.respond(200, "OK").await;
    let final_resp = call.expect(408).await;
    assert_eq!(final_resp.status, 408, "caller's INVITE resolves with 408 at the setup timeout");
    uas.respond(487, "Request Terminated").await;

    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    assert_eq!(
        b2bua.metrics().removals_total(),
        b2bua.metrics().creations_total(),
        "setup-stalled call torn down once the ringing b-leg's CANCEL resolves (not the 1h GlobalDuration)",
    );

    // The limiter hold is released once teardown completes (the leak fixed by this test).
    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "limiter hold released at teardown");

    // CDR records the setup timeout; per-call state fully reclaimed.
    settle_until(|| !b2bua.cdr_records().is_empty()).await;
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1);
    assert!(
        reasons_of(&cdrs[0]).iter().any(|r| r.contains("setup")),
        "CDR carries the setup-timeout reason: {:?}",
        reasons_of(&cdrs[0]),
    );
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}

/// A long ring that answers BEFORE the deadline must survive: the answer
/// cancels the setup timer, so advancing past the deadline after the answer
/// must NOT tear the call down.
#[tokio::test(start_paused = true)]
async fn long_ring_that_answers_before_the_deadline_survives() {
    let h = Harness::new("b2bua-setup-timeout-long-ring");
    let alice = h.agent("alice", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5071").await;
    let decision = Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5071));
    // Keepalive pushed out so the post-answer advance exercises ONLY the
    // (cancelled) setup timer, not the keepalive machinery.
    let b2bua = B2buaSut::builder(decision)
        .tune(|c| c.keepalive_interval_sec = 3_600)
        .start(&h, "b2bua", "127.0.0.1:5081")
        .await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    // Ring for a long time (2 min), but answer inside the 150 s default
    // deadline (and inside the 158 s sip-txn INVITE_INITIAL_TIMEOUT).
    h.advance(Duration::from_secs(120)).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // Cross the (cancelled) 150 s deadline AND the 158 s txn mark: the
    // answered call must stay up.
    h.advance(Duration::from_secs(100)).await;
    assert_eq!(
        b2bua.metrics().creations_total() - b2bua.metrics().removals_total(),
        1,
        "answered call must survive past the setup deadline",
    );

    // Clean teardown still works.
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
