//! The proxy's probe answers over a real probe server: a stalled recv shard
//! fails liveness as well as readiness, an idle proxy fails neither.

use probe_http::{ProbeRoutes, ProbeServer};
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerEntry;
use sip_proxy::ProxyAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;

const STALL_MS: u64 = 2_000;

async fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes()).await.unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).await.unwrap();
    String::from_utf8_lossy(&resp).into_owned()
}

/// A probe server over [`probes`] for one `Alive` worker and `pulse`, read at
/// 10 s on the test clock.
async fn server(pulse: Arc<ShardPulse>) -> ProbeServer {
    let pool: Arc<dyn WorkerRegistry> =
        Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry::alive(
            "w0",
            ProxyAddr::new("10.0.0.1", 5060),
        )]));
    let draining = Arc::new(AtomicBool::new(false));
    let (ready, live) = probes(draining, pulse, Clock::test_at(10_000), STALL_MS, pool);
    let routes = ProbeRoutes { metrics: Arc::new(String::new), ready, heap: None };
    ProbeServer::start_live("127.0.0.1:0".parse().unwrap(), routes, live).await.unwrap()
}

/// A shard stalled past its threshold fails `/healthz`, which keepalived's
/// check and the livenessProbe read, as well as `/readyz`.
#[tokio::test]
async fn a_stalled_shard_fails_healthz_and_readyz() {
    let pulse = Arc::new(ShardPulse::new(2));
    pulse.took_packet(1, 10_000 - STALL_MS - 1);
    let s = server(pulse).await;
    let health = get(s.addr(), "/healthz").await;
    assert!(health.starts_with("HTTP/1.1 503"), "{health}");
    let ready = get(s.addr(), "/readyz").await;
    assert!(ready.starts_with("HTTP/1.1 503"), "{ready}");
}

/// Shards waiting on their sockets, however long, or busy inside the
/// threshold: both probes pass. The busy shard took its packet half the
/// threshold before the reads, so the real time the server and the two reads
/// take never carries it past the threshold.
#[tokio::test]
async fn an_idle_or_turning_proxy_passes_healthz_and_readyz() {
    let pulse = Arc::new(ShardPulse::new(2));
    pulse.took_packet(0, 10_000 - STALL_MS / 2);
    let s = server(pulse).await;
    assert!(get(s.addr(), "/healthz").await.starts_with("HTTP/1.1 200"));
    assert!(get(s.addr(), "/readyz").await.starts_with("HTTP/1.1 200"));
}

/// Readiness counts `Alive` workers whatever their acceptance: a cluster
/// refusing new calls everywhere keeps its proxy ready (ADR-0050 §11).
#[test]
fn readiness_ignores_acceptance() {
    use sip_proxy::registry::{Acceptance, WorkerHealth};
    let entry = |health, acceptance| WorkerEntry {
        health,
        acceptance,
        ..WorkerEntry::alive("w0", ProxyAddr::new("10.0.0.1", 5060))
    };
    let state =
        |e: WorkerEntry| probe_state(false, false, &StaticWorkerRegistry::from_entries(vec![e]));
    assert_eq!(state(entry(WorkerHealth::Alive, Acceptance::Refuse)), ProbeState::Ready);
    assert_eq!(state(entry(WorkerHealth::Alive, Acceptance::Accept)), ProbeState::Ready);
    assert_eq!(state(entry(WorkerHealth::Unknown, Acceptance::Accept)), ProbeState::NotReady);
    assert_eq!(state(entry(WorkerHealth::Dead, Acceptance::Accept)), ProbeState::NotReady);
}
