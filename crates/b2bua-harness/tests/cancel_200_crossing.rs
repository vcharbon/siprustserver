//! **Crossing 200 OK / CANCEL** — the race the `cancel-200-crossing` rule
//! (`rules/defaults.rs`) exists for, now exercised end-to-end. Sibling to the
//! `crossing_reinvite_glare` race (`reinvite.rs`): two messages cross on the
//! wire and the B2BUA must resolve them to a clean, fully-reaped state.
//!
//! The race: alice CANCELs a ringing call, so the B2BUA marks the b-leg
//! `Cancelling` and sends CANCEL to bob — but bob has *already* answered 200 OK,
//! and that 200 crosses the CANCEL. You cannot un-answer a confirmed dialog, so
//! the rule confirms it, ACKs the 200 (RFC 3261 §13.2.2.4 — a 2xx MUST be
//! ACKed), then immediately BYEs the b-leg to honour the caller's cancel. The
//! caller already saw `200`(CANCEL)+`487`(INVITE) from the transaction layer.
//!
//! As with the limit suite, a real limiter is wired so this race is also pinned
//! against the leak class: the hold taken at route time MUST be released by the
//! crossing teardown — `current_total` back to 0.

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
use call_limiter::{LimiterConfig, LimiterMetrics, LimiterServer, WindowStore};
use http_net::{HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
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

fn route_limited(host: &str, port: u16, id: &str, limit: i64) -> Arc<dyn CallDecisionEngine> {
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

#[tokio::test(start_paused = true)]
async fn cancel_200_crossing_acks_then_byes_the_b_leg_and_releases_the_limiter() {
    let h = Harness::new("b2bua-cancel-200-crossing");
    let alice = h.agent("alice", "127.0.0.1:5063").await;
    let bob = h.agent("bob", "127.0.0.1:5073").await;

    let http = SimulatedHttpNetwork::new();
    let (store, _limiter_srv) = serve_limiter(&http).await;
    let decision = route_limited("127.0.0.1", 5073, "trunk-A", 1);
    let b2bua = B2buaSut::start_with_limiter(
        &h,
        "b2bua",
        "127.0.0.1:5083",
        decision,
        limiter_client(&http),
        |c| c.reaper_enabled = false,
    )
    .await;

    // ── ringing call ─────────────────────────────────────────────────────────
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    assert_eq!(store.stats().current_total, 1, "ringing call holds one limiter slot");

    // ── alice CANCELs; the transaction layer releases the caller immediately ──
    let mut cxl = call.cancel().await;
    cxl.expect(200).await; // 200 OK to the CANCEL
    call.expect(487).await; // 487 Request Terminated on the INVITE

    // The B2BUA forwards a CANCEL to the still-ringing b-leg (Cancelling).
    let mut bob_cancel = bob.receive("CANCEL").await;

    // ── CROSSING: bob answers 200 OK before the CANCEL takes effect ───────────
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob_cancel.respond(200, "OK").await;

    // The B2BUA can't un-answer a confirmed dialog: it ACKs the crossing 200,
    // then BYEs the b-leg to honour the cancel.
    bob.receive("ACK").await;
    let mut bye = bob.receive("BYE").await;
    bye.respond(200, "OK").await;

    // ── no leak: the hold is released by the crossing teardown ───────────────
    settle_until(|| store.stats().current_total == 0).await;
    assert_eq!(store.stats().current_total, 0, "limiter hold released by the crossing teardown");
    settle_until(|| b2bua.metrics().removals_total() == b2bua.metrics().creations_total()).await;
    b2bua.assert_fully_reaped();

    let _report = h.finish().await;
}
