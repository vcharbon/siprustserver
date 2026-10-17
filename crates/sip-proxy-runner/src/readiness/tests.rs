//! The proxy's probe answers over a real probe server: a stalled recv shard
//! fails liveness as well as readiness and the VRRP check, an idle proxy
//! fails none; before its first worker view the proxy is neither ready nor
//! fit to hold the virtual address, and is live all along.

use probe_http::{ProbeRoutes, ProbeServer};
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::{Acceptance, WorkerEntry, WorkerHealth};
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

/// One worker `w0` read as `health`.
fn pool(health: WorkerHealth) -> Arc<dyn WorkerRegistry> {
    Arc::new(StaticWorkerRegistry::from_entries(vec![WorkerEntry {
        health,
        ..WorkerEntry::alive("w0", ProxyAddr::new("10.0.0.1", 5060))
    }]))
}

/// A probe server over [`probes`] for `pool`, `pulse` and `view`, read at
/// 10 s on the test clock.
async fn server_over(
    pool: Arc<dyn WorkerRegistry>,
    pulse: Arc<ShardPulse>,
    view: Arc<FirstView>,
) -> ProbeServer {
    let draining = Arc::new(AtomicBool::new(false));
    let p = probes(draining, pulse, Clock::test_at(10_000), STALL_MS, pool, view);
    let routes = ProbeRoutes {
        metrics: Arc::new(String::new),
        ready: p.ready,
        heap: None,
        vrrp: Some(p.vrrp),
        admin: None,
    };
    ProbeServer::start_live("127.0.0.1:0".parse().unwrap(), routes, p.live).await.unwrap()
}

/// A probe server with one `Alive` worker and its first view in.
async fn server(pulse: Arc<ShardPulse>) -> ProbeServer {
    server_over(pool(WorkerHealth::Alive), pulse, FirstView::given()).await
}

fn is_ok(resp: &str) -> bool {
    resp.starts_with("HTTP/1.1 200")
}

/// A shard stalled past its threshold fails `/healthz`, which the
/// livenessProbe reads, `/vrrp`, which keepalived's check reads, and
/// `/readyz`.
#[tokio::test]
async fn a_stalled_shard_fails_healthz_vrrp_and_readyz() {
    let pulse = Arc::new(ShardPulse::new(2));
    pulse.took_packet(1, 10_000 - STALL_MS - 1);
    let s = server(pulse).await;
    for path in ["/healthz", "/vrrp", "/readyz"] {
        let resp = get(s.addr(), path).await;
        assert!(resp.starts_with("HTTP/1.1 503"), "{path}: {resp}");
    }
}

/// Shards waiting on their sockets, however long, or busy inside the
/// threshold: every probe passes. The busy shard took its packet half the
/// threshold before the reads, so the real time the server and the reads
/// take never carries it past the threshold.
#[tokio::test]
async fn an_idle_or_turning_proxy_passes_healthz_vrrp_and_readyz() {
    let pulse = Arc::new(ShardPulse::new(2));
    pulse.took_packet(0, 10_000 - STALL_MS / 2);
    let s = server(pulse).await;
    for path in ["/healthz", "/vrrp", "/readyz"] {
        assert!(is_ok(&get(s.addr(), path).await), "{path}");
    }
}

/// A starting proxy, its worker already read alive: `/readyz` and `/vrrp`
/// fail until the first worker view is in; `/healthz` passes from the
/// start, so the livenessProbe never restarts it for want of a view.
#[tokio::test]
async fn before_the_first_view_readyz_and_vrrp_fail_and_healthz_passes() {
    let view = FirstView::pending();
    let s =
        server_over(pool(WorkerHealth::Alive), Arc::new(ShardPulse::new(1)), view.clone()).await;
    assert!(is_ok(&get(s.addr(), "/healthz").await));
    let ready = get(s.addr(), "/readyz").await;
    assert!(ready.starts_with("HTTP/1.1 503") && ready.contains("not-ready"), "{ready}");
    let vrrp = get(s.addr(), "/vrrp").await;
    assert!(vrrp.starts_with("HTTP/1.1 503") && vrrp.contains("not-ready"), "{vrrp}");

    view.latch();
    for path in ["/healthz", "/vrrp", "/readyz"] {
        assert!(is_ok(&get(s.addr(), path).await), "{path}");
    }
}

/// The first view in and no worker alive (none answered the first round):
/// the proxy is not ready, and fit to hold the address, so a pair that sees
/// no worker keeps equal priorities and the address stays where it is
/// (ADR-0046 D4).
#[tokio::test]
async fn with_the_view_in_and_no_worker_alive_vrrp_passes_and_readyz_fails() {
    let s =
        server_over(pool(WorkerHealth::Unknown), Arc::new(ShardPulse::new(1)), FirstView::given())
            .await;
    assert!(is_ok(&get(s.addr(), "/healthz").await));
    assert!(is_ok(&get(s.addr(), "/vrrp").await));
    assert!(get(s.addr(), "/readyz").await.starts_with("HTTP/1.1 503"));
}

/// Readiness counts `Alive` workers whatever their acceptance: a cluster
/// refusing new calls everywhere keeps its proxy ready (ADR-0050 §11).
#[test]
fn readiness_ignores_acceptance() {
    let entry = |health, acceptance| WorkerEntry {
        health,
        acceptance,
        ..WorkerEntry::alive("w0", ProxyAddr::new("10.0.0.1", 5060))
    };
    let state = |e: WorkerEntry| {
        probe_state(false, false, true, &StaticWorkerRegistry::from_entries(vec![e]))
    };
    assert_eq!(state(entry(WorkerHealth::Alive, Acceptance::Refuse)), ProbeState::Ready);
    assert_eq!(state(entry(WorkerHealth::Alive, Acceptance::Accept)), ProbeState::Ready);
    assert_eq!(state(entry(WorkerHealth::Unknown, Acceptance::Accept)), ProbeState::NotReady);
    assert_eq!(state(entry(WorkerHealth::Dead, Acceptance::Accept)), ProbeState::NotReady);
}
