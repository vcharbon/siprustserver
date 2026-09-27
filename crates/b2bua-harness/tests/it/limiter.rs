//! Call-limiter end-to-end: alice ↔ b2bua ↔ bob with a **real** `LimiterServer`
//! bound on the simulated HTTP fabric, reached by the b2bua's `HttpCallLimiter`.
//!
//! Covers: reject → 486, release-on-BYE frees the slot, fail-open when the
//! limiter is cut, shared counting across two workers, and failover-on-reject.
//! Every route holds two limiters, the capped one and a wide one, and every
//! call ends: each test drains the store to 0 in the reaped check.
//! The refresh-on-long-call case (paused clock) lives in `limiter_refresh.rs`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallDecisionEngine, CallFailureResponse, CallLimiterEntry, NewCallResponse,
    ScriptedDecisionEngine,
};
use b2bua::limiter::CallLimiter;
use b2bua::limiter_http::HttpCallLimiter;
use b2bua_harness::{establish, hangup, settle_until, B2buaSut};
use call_limiter::{CallStore, LimiterConfig, LimiterMetrics, LimiterServer};
use http_net::{Fault, HttpServerHandle, HttpTransport, SimulatedHttpNetwork};
use scenario_harness::Harness;
use sip_clock::Clock;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";

const LIMITER_ADDR: &str = "10.0.0.1:8080";

/// The wide limiter every route holds beside its capped one.
fn wide() -> CallLimiterEntry {
    CallLimiterEntry { id: "site-B".into(), limit: 100 }
}

fn laddr() -> SocketAddr {
    LIMITER_ADDR.parse().unwrap()
}

/// Serve a real `LimiterServer` (the default lease, longer than any test here)
/// on `net` at [`LIMITER_ADDR`]; return the store (for count probes)
/// and the server handle (keep alive for the test).
async fn serve_limiter(net: &SimulatedHttpNetwork) -> (Arc<CallStore>, Box<dyn HttpServerHandle>) {
    let store = Arc::new(CallStore::new(LimiterConfig::default(), Clock::test_at(0)));
    let server = Arc::new(LimiterServer::new(store.clone(), LimiterMetrics::new()));
    let handle = net.serve(laddr(), server).await.unwrap();
    (store, handle)
}

fn limiter_client(net: &SimulatedHttpNetwork) -> Arc<dyn CallLimiter> {
    Arc::new(HttpCallLimiter::new(Arc::new(net.clone()), laddr(), Duration::from_millis(150)))
}

/// A decision that routes every call to `host:port` and attaches the limiter
/// entry `{id, limit}` then the wide one.
fn route_with_limiter(host: &str, port: u16, id: &str, limit: i64) -> Arc<dyn CallDecisionEngine> {
    let host = host.to_string();
    let id = id.to_string();
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to(&host, port);
                r.call_limiter = vec![CallLimiterEntry { id: id.clone(), limit }, wide()];
                NewCallResponse::Route(r)
            })
            .build(),
    )
}

/// Settle until `sut` reaped every call and the store counts nothing, then
/// run the reaped check.
async fn assert_drained(sut: &B2buaSut, store: &CallStore) {
    settle_until(|| sut.is_reaped() && store.stats().current_total == 0).await;
    sut.assert_fully_reaped();
}

#[tokio::test]
async fn rejected_call_gets_486_and_no_second_increment() {
    let h = Harness::with_transit_delay("limiter-486", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let http = SimulatedHttpNetwork::new();
    let (store, _lh) = serve_limiter(&http).await;
    let decision = route_with_limiter("127.0.0.1", 5070, "trunk-A", 1);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .limiter_store(store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // First call admitted + answered: it holds both limiters.
    let mut dialog1 = establish(&alice, &bob, b2bua.addr).await;
    assert_eq!((store.held("trunk-A"), store.held("site-B")), (1, 1), "first call counted");

    // Second concurrent call: trunk-A at cap 1 → 486 Busy Here.
    let mut call2 = carol.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    let resp = call2.expect(486).await;
    assert_eq!(resp.status(), 486);

    // No second increment happened on either id (all or none).
    assert_eq!((store.held("trunk-A"), store.held("site-B")), (1, 1), "reject took nothing");

    hangup(&mut dialog1, &bob).await;
    assert_drained(&b2bua, &store).await;
    let _ = h.finish().await;
}

#[tokio::test]
async fn release_on_bye_frees_the_slot() {
    let h = Harness::with_transit_delay("limiter-release", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let http = SimulatedHttpNetwork::new();
    let (store, _lh) = serve_limiter(&http).await;
    let decision = route_with_limiter("127.0.0.1", 5070, "trunk-A", 1);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .limiter_store(store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // Establish then hang up call 1.
    let mut dialog1 = establish(&alice, &bob, b2bua.addr).await;
    assert_eq!(store.stats().current_total, 2);
    hangup(&mut dialog1, &bob).await;

    // The release must drain the counter back to 0.
    assert_drained(&b2bua, &store).await;

    // The freed slot admits a fresh call (bob sees its INVITE, not a 486).
    let mut call2 = carol.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call2.expect(200).await;
    // Complete the handshake so the 2xx is ACKed (RFC 3261 §13.3.1.4 — an un-ACKed
    // answered call is now caught by the unacked-2xx-not-cleared audit).
    let mut dialog2 = call2.ack().await;
    bob.receive("ACK").await;
    assert_eq!(store.stats().current_total, 2, "the second call holds both limiters");
    hangup(&mut dialog2, &bob).await;
    assert_drained(&b2bua, &store).await;
    let _ = h.finish().await;
}

#[tokio::test]
async fn fail_open_admits_when_limiter_is_cut() {
    let h = Harness::with_transit_delay("limiter-fail-open", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let http = SimulatedHttpNetwork::new();
    let (store, _lh) = serve_limiter(&http).await;
    // Cut the limiter so admit fails → b2bua must fail open.
    http.apply_fault(Fault::Cut { dst: laddr() });

    let decision = route_with_limiter("127.0.0.1", 5070, "trunk-A", 1);
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .limiter_store(store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // The call is admitted despite the limiter being down: bob sees the INVITE.
    let mut call = alice.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    // Complete the handshake so the 2xx is ACKed (unacked-2xx-not-cleared).
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    // No increment ever reached the server (it was cut), so no hold to leak.
    assert_eq!(store.stats().current_total, 0, "fail-open records no hold");
    assert_eq!(b2bua.limiter_count().failed_open, 1, "the admit failed open");

    hangup(&mut dialog, &bob).await;
    // The release the admit owes waits in the release queue while the limiter
    // is cut, and leaves once it is back.
    settle_until(|| b2bua.calls_reaped()).await;
    assert_eq!(b2bua.limiter_releases_waiting(), 1, "the release waits for the limiter");
    http.apply_fault(Fault::Resume { dst: laddr() });
    assert_drained(&b2bua, &store).await;
    assert_eq!(store.stats().releases_total, 1, "the release landed once the limiter was back");
    let _ = h.finish().await;
}

#[tokio::test]
async fn shared_counting_across_two_workers() {
    let h = Harness::with_transit_delay("limiter-x-worker", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let http = SimulatedHttpNetwork::new();
    let (store, _lh) = serve_limiter(&http).await;

    // Two workers, distinct ordinals, ONE shared limiter server.
    let w0 = B2buaSut::builder(route_with_limiter("127.0.0.1", 5070, "trunk-A", 1))
        .limiter(limiter_client(&http))
        .limiter_store(store.clone())
        .tune(|c| c.self_ordinal = "w0".into())
        .start(&h, "w0", "127.0.0.1:5080")
        .await;
    let w1 = B2buaSut::builder(route_with_limiter("127.0.0.1", 5070, "trunk-A", 1))
        .limiter(limiter_client(&http))
        .limiter_store(store.clone())
        .tune(|c| c.self_ordinal = "w1".into())
        .start(&h, "w1", "127.0.0.1:5081")
        .await;

    // Call through w0 fills the shared cap of 1.
    let mut dialog = establish(&alice, &bob, w0.addr).await;

    // Call through w1 sees the SAME counter → rejected 486.
    let mut call2 = carol.invite(&bob).with_sdp(OFFER).through(w1.addr).send().await;
    assert_eq!(call2.expect(486).await.status(), 486);

    hangup(&mut dialog, &bob).await;
    assert_drained(&w0, &store).await;
    assert_drained(&w1, &store).await;
    let _ = h.finish().await;
}

#[tokio::test]
async fn failover_on_reject_routes_to_backup() {
    let h = Harness::with_transit_delay("limiter-failover", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    let carol = h.agent("carol", "127.0.0.1:5061").await;
    let bob = h.agent("bob", "127.0.0.1:5070").await;
    let bob2 = h.agent("bob2", "127.0.0.1:5071").await;
    let http = SimulatedHttpNetwork::new();
    let (store, _lh) = serve_limiter(&http).await;

    // Primary route: trunk-A cap 1 + the wide limiter + callback_context
    // (failover-capable). On /call/failure: failover to bob2 under the wide
    // limiter alone.
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", 5070);
                r.call_limiter = vec![CallLimiterEntry { id: "trunk-A".into(), limit: 1 }, wide()];
                r.callback_context = Some("limiter-failover".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |_req| {
                let mut r = route_to("127.0.0.1", 5071);
                r.call_limiter = vec![wide()];
                CallFailureResponse::Route(r)
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .limiter(limiter_client(&http))
        .limiter_store(store.clone())
        .start(&h, "b2bua", "127.0.0.1:5080")
        .await;

    // Call 1 fills the cap on the primary.
    let mut dialog1 = establish(&alice, &bob, b2bua.addr).await;

    // Call 2 is rejected by the limiter, but callback_context → /call/failure →
    // failover to bob2: bob2 (not bob) receives the INVITE, no 486 to carol.
    let mut call2 = carol.invite(&bob).with_sdp(OFFER).through(b2bua.addr).send().await;
    bob2.receive("INVITE").await.respond(200, "OK").with_sdp(ANSWER).await;
    call2.expect(200).await;
    // Complete the handshake so the 2xx is ACKed (unacked-2xx-not-cleared).
    let mut dialog2 = call2.ack().await;
    bob2.receive("ACK").await;
    settle_until(|| store.held("site-B") == 2).await;
    assert_eq!(
        (store.held("trunk-A"), store.held("site-B")),
        (1, 2),
        "the failed-over call holds the backup route's limiter only"
    );

    hangup(&mut dialog1, &bob).await;
    hangup(&mut dialog2, &bob2).await;
    assert_drained(&b2bua, &store).await;
    let _ = h.finish().await;
}
