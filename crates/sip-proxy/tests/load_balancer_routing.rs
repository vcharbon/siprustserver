//! LoadBalancer end-to-end through a real `ProxyCore` SUT — ports of
//! `tests/sip-front-proxy/load-balancer/{callid-routing-guard, distribution}`
//! at the wire level: a new-dialog INVITE is HRW-routed to a worker, the worker
//! sees a signed Record-Route, and the in-dialog BYE (carrying that cookie back)
//! sticks to the same worker.

mod common;

use std::sync::Arc;

use scenario_harness::Harness;
use sip_clock::Clock;
use sip_message::message_helpers::get_headers;
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::{LoadBalancerConfig, LoadBalancerStrategy, ProxyAddr, ProxyMetrics, RoutingStrategy};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 49170 RTP/AVP 0\r\n";

#[tokio::test]
async fn new_dialog_routes_to_worker_and_in_dialog_sticks() {
    let h = Harness::with_transit_delay("lb-routing", 0);
    let alice = h.agent("alice", "127.0.0.1:5060").await;
    // Two backend workers; alice doesn't know which the HRW picks.
    let w1 = h.agent("b2b-1", "127.0.0.1:5071").await;
    let w2 = h.agent("b2b-2", "127.0.0.1:5072").await;

    let registry: Arc<dyn WorkerRegistry> = Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![
            WorkerEntry::alive("b2b-1", ProxyAddr::new("127.0.0.1", 5071)),
            WorkerEntry::alive("b2b-2", ProxyAddr::new("127.0.0.1", 5072)),
        ],
        Clock::test_at(0),
    ));
    let hmac = Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
        registry.clone(),
        hmac,
        observer,
        Arc::new(ProxyMetrics::new()),
        Clock::test_at(0),
        LoadBalancerConfig::default(),
    ));
    let proxy = common::spawn_proxy(&h, "127.0.0.1:5080", strategy, registry.clone()).await;

    // Initial INVITE addressed at a notional bob, sent through the LB proxy.
    let mut call = alice.invite(&w1).with_sdp(OFFER).through(proxy.addr()).send().await;

    // Exactly one worker receives the INVITE (whichever HRW picked); the other
    // sees nothing. Determine the winner by polling both with a short race.
    let (mut uas, winner_is_w1) = receive_on_either(&w1, &w2).await;
    let recvd = uas.request();
    // Double record-route: on this INBOUND new dialog the worker sees TWO RRs —
    // the worker-facing `;outbound` half on top, the external-facing signed cookie
    // (`w_pri=`) second. Direction is now carried by the proxy's own RRs, so the
    // worker's route set leads back out (`;outbound`) and alice's (reverse of the
    // 2xx) leads in via the cookie.
    let rrs = get_headers(&recvd.headers, "record-route");
    assert_eq!(rrs.len(), 2, "worker must see both record-route halves, got {rrs:?}");
    assert!(
        rrs[0].contains(";lr") && rrs[0].contains("outbound"),
        "the top (worker-facing) RR must be the ;outbound half, got {:?}",
        rrs[0]
    );
    assert!(
        rrs[1].contains(";lr") && rrs[1].contains("w_pri="),
        "the second (external-facing) RR must be the signed LB cookie, got {:?}",
        rrs[1]
    );

    uas.respond(200, "OK").send().await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    // ACK reaches the same worker.
    let winner = if winner_is_w1 { &w1 } else { &w2 };
    winner.receive("ACK").await;

    // BYE carries the cookie back through the proxy and sticks to the winner.
    let mut bye = dialog.bye().await;
    let mut bye_uas = winner.receive("BYE").await;
    bye_uas.respond(200, "OK").await;
    bye.expect(200).await;

    let report = h.finish().await;
    assert!(report.entries().iter().all(|e| e.delivered));
}

/// Receive an INVITE on whichever of two workers the LB picked; assert the other
/// got nothing.
async fn receive_on_either(
    w1: &scenario_harness::Agent,
    w2: &scenario_harness::Agent,
) -> (scenario_harness::ServerTxn, bool) {
    tokio::select! {
        txn = w1.receive("INVITE") => (txn, true),
        txn = w2.receive("INVITE") => (txn, false),
    }
}
