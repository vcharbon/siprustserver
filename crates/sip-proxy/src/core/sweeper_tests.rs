use super::*;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::Mutex;

use async_trait::async_trait;
use sip_net::{SendError, UdpEndpointCounters, UdpPacket};

use crate::cancel_lru::{CancelEntry, Lifetime};
use crate::registry::static_reg::StaticWorkerRegistry;
use crate::ForwardAllStrategy;

/// A `UdpEndpoint` whose `recv` never resolves: the proxy's recv loop parks,
/// so the background sweeper is the only task making progress.
struct PendingEndpoint;

#[async_trait]
impl UdpEndpoint for PendingEndpoint {
    async fn send_to(&self, _buf: &[u8], _dst: SocketAddr) -> Result<(), SendError> {
        Ok(())
    }
    async fn recv(&self) -> Option<UdpPacket> {
        std::future::pending::<Option<UdpPacket>>().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        None
    }
    fn local_addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5060))
    }
    fn queue_depth(&self) -> usize {
        0
    }
    fn queue_max(&self) -> usize {
        0
    }
    fn counters(&self) -> UdpEndpointCounters {
        UdpEndpointCounters::default()
    }
}

/// A running core's background sweeper physically reclaims an
/// expired pending-INVITE entry and re-publishes the gauge to match. Without
/// the sweeper the entry would linger for the life of the process (an
/// answered call never re-`lookup`s its key, so lazy eviction never fires)
/// and `sip_proxy_pending_invite_lru_size` would climb without bound.
#[tokio::test(start_paused = true)]
async fn run_sweeps_expired_pending_invite_entries() {
    // (Cadence ≤ shortest TTL is a compile-time invariant in cancel_lru.rs.)
    let clock = Clock::test_at(0);
    let lru = Arc::new(CancelBranchLru::with_clock(clock.clone()));
    let metrics = Arc::new(ProxyMetrics::new());

    // Remember one entry at the SHORT (rtx) TTL and publish the gauge, as
    // the request path does on every forward.
    lru.remember(
        "call-leaky-memo",
        CancelEntry {
            target: ProxyAddr::new("10.0.0.2", 5070),
            branch: "z9hG4bK-x".into(),
            stickiness: None,
            minted: None,
            via_cookie: None,
        },
        Lifetime::Retransmission,
    );
    metrics.set_pending_invite_lru_size(lru.size() as u64);
    assert_eq!(metrics.pending_invite_lru_size(), 1);

    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new("10.0.0.2", 5070)));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let core = ProxyCoreBuilder::for_test(ProxyAddr::new("127.0.0.1", 5060), strategy, registry)
        .clock(clock)
        .cancel_lru(lru.clone())
        .metrics(metrics.clone())
        .build(Box::new(PendingEndpoint));
    let task = tokio::spawn(core.run());

    // Advance past the 32 s TTL so a sweep tick lands after the entry expires;
    // the map must drain and the gauge follow it down.
    for _ in 0..50 {
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        if lru.size() == 0 {
            break;
        }
    }

    assert_eq!(lru.size(), 0, "sweeper must physically reclaim the expired entry");
    assert_eq!(metrics.pending_invite_lru_size(), 0, "gauge must follow the reclaimed map down");

    task.abort();
}

/// A `UdpEndpoint` that yields its queued packets once, then closes
/// (`recv` → `None`), so `run()` dequeues them and exits.
struct DrainOnceEndpoint(Mutex<Vec<UdpPacket>>);

#[async_trait]
impl UdpEndpoint for DrainOnceEndpoint {
    async fn send_to(&self, _buf: &[u8], _dst: SocketAddr) -> Result<(), SendError> {
        Ok(())
    }
    async fn recv(&self) -> Option<UdpPacket> {
        let mut q = self.0.lock().unwrap();
        if q.is_empty() {
            None
        } else {
            Some(q.remove(0))
        }
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        None
    }
    fn local_addr(&self) -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 5060))
    }
    fn queue_depth(&self) -> usize {
        0
    }
    fn queue_max(&self) -> usize {
        0
    }
    fn counters(&self) -> UdpEndpointCounters {
        UdpEndpointCounters::default()
    }
}

/// The recv loop records every dequeued packet's age (`now_ms −
/// arrival_ms`) into the shared [`IntakeAgeRecorder`] — the max wins, and a
/// malformed (dropped-silently) datagram still counts: the signal measures
/// intake queueing, not parse outcomes. `Clock::test_at(0)` rides the
/// paused tokio timeline, so the advance below IS the packets' age.
#[tokio::test(start_paused = true)]
async fn run_records_packet_age_at_dequeue() {
    let clock = Clock::test_at(0);
    // Move the paused clock to now_ms = 700 in 100 ms chunks (no pending
    // timers exist yet — nothing can fire mid-advance).
    for _ in 0..7 {
        tokio::time::advance(Duration::from_millis(100)).await;
    }
    // Two packets queued in the past: ages at dequeue 500 ms and 200 ms.
    // Raw bytes are deliberately unparseable — no SIP flow is involved.
    let ep = DrainOnceEndpoint(Mutex::new(vec![
        UdpPacket::unparsed(&b"not-sip"[..], "127.0.0.1:9999".parse().unwrap(), 200),
        UdpPacket::unparsed(&b"not-sip"[..], "127.0.0.1:9999".parse().unwrap(), 500),
    ]));
    let recorder = IntakeAgeRecorder::default();

    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new("10.0.0.2", 5070)));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    let core = ProxyCoreBuilder::for_test(ProxyAddr::new("127.0.0.1", 5060), strategy, registry)
        .clock(clock)
        .intake_age(recorder.clone())
        .build(Box::new(ep));

    // Drain both packets; the endpoint then closes and run() returns.
    core.run().await;

    assert_eq!(recorder.drain_max(), 500, "the window max is the OLDEST packet's dequeue age");
    assert_eq!(recorder.drain_max(), 0, "drained — no new traffic reads 0");
}

/// The arrival stamp and the dequeue reading are ONE timeline. A `Clock`
/// anchored far from raw wall time — the wall-vs-monotonic divergence a
/// long-lived host accumulates — must not surface as intake age, or an
/// idle proxy reads critically overloaded and sheds every new call.
#[tokio::test(start_paused = true)]
async fn intake_age_is_immune_to_wall_versus_monotonic_divergence() {
    use sip_net::types::BindUdpOpts;
    use sip_net::SignalingNetwork;
    use sip_net_testkit::SimulatedSignalingNetwork;

    use crate::self_gate::AGE_CRITICAL_MS;

    // 20× the age at which the ELU arm reads fully saturated.
    let clock = Clock::test_at(sip_clock::raw_system_wall_ms() + 20 * AGE_CRITICAL_MS as i64);

    // The arrival stamp comes from a REAL bind told that clock — the seam
    // the runner wires; the bytes are deliberately unparseable (the age is
    // recorded before the parse).
    let net = SimulatedSignalingNetwork::new(1);
    let uas: SocketAddr = "127.0.0.1:5061".parse().unwrap();
    let uac = net
        .bind_udp(BindUdpOpts::new("127.0.0.1:5062".parse().unwrap(), 8).with_clock(clock.clone()))
        .await
        .unwrap();
    let ingress = net.bind_udp(BindUdpOpts::new(uas, 8).with_clock(clock.clone())).await.unwrap();
    uac.send_to(b"not-sip", uas).await.unwrap();
    let pkt = ingress.recv().await.expect("the fabric delivers one datagram");

    let recorder = IntakeAgeRecorder::default();
    let strategy: Arc<dyn RoutingStrategy> =
        Arc::new(ForwardAllStrategy::new(ProxyAddr::new("10.0.0.2", 5070)));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    ProxyCoreBuilder::for_test(ProxyAddr::new("127.0.0.1", 5060), strategy, registry)
        .clock(clock)
        .intake_age(recorder.clone())
        .build(Box::new(DrainOnceEndpoint(Mutex::new(vec![pkt]))))
        .run()
        .await;

    let age = recorder.drain_max();
    assert!(
        age < AGE_CRITICAL_MS,
        "an unqueued packet must age well under {AGE_CRITICAL_MS} ms whatever the wall/monotonic divergence, got {age} ms",
    );
}

/// Per-peer metric classification: a destination that resolves to a known
/// worker is INTERNAL (pinned); an unknown address (a UAC/UAS) is EXTERNAL
/// (under the cap). Drives `sip_proxy_peer_failures_total{scope}`.
#[test]
fn classify_peer_internal_iff_known_worker() {
    use crate::observability::peer_failures::PeerScope;
    use crate::registry::WorkerEntry;

    let worker = ProxyAddr::new("10.0.0.2", 5070);
    let registry: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w0",
            worker.clone(),
        )]));
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(ForwardAllStrategy::new(worker.clone()));
    let core = ProxyCoreBuilder::for_test(ProxyAddr::new("127.0.0.1", 5060), strategy, registry)
        .build(Box::new(PendingEndpoint));

    assert_eq!(core.classify_peer(&worker), PeerScope::Internal, "a known worker is internal");
    assert_eq!(
        core.classify_peer(&ProxyAddr::new("203.0.113.9", 5060)),
        PeerScope::External,
        "an off-cluster address is external",
    );
}
