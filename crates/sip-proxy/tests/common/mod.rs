//! Shared test support: spawn a real [`ProxyCore`] as a System-Under-Test on
//! the scenario-harness's shared recording fabric. Alice/Bob (and simulated
//! workers) are ordinary harness agents; the proxy auto-forwards as datagrams
//! arrive (no scripted `forward_request`).

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use scenario_harness::Harness;
use sip_clock::Clock;
use sip_proxy::registry::static_reg::StaticWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::{ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;
use tokio::task::JoinHandle;

/// A running proxy SUT. Aborts its recv loop on drop.
pub struct ProxySut {
    pub addr: SocketAddr,
    pub metrics: Arc<ProxyMetrics>,
    task: JoinHandle<()>,
}

impl ProxySut {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for ProxySut {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bind + spawn a real `ProxyCore` on the harness fabric with a seeded `IdGen`
/// (deterministic Via branches) and a test clock.
pub async fn spawn_proxy(
    h: &Harness,
    addr: &str,
    strategy: Arc<dyn RoutingStrategy>,
    registry: Arc<dyn WorkerRegistry>,
) -> ProxySut {
    let (ep, sock) = h.bind_sut("proxy", addr).await;
    let metrics = Arc::new(ProxyMetrics::new());
    let core = ProxyCoreBuilder::new(ProxyAddr::from(sock), strategy, registry)
        .clock(Clock::test_at(0))
        .id_gen(Arc::new(IdGen::seeded(0xC0FFEE)))
        .metrics(metrics.clone())
        .build(ep);
    let task = tokio::spawn(core.run());
    ProxySut { addr: sock, metrics, task }
}

/// A `ForwardAll` strategy pointed at a single backend, with an empty registry
/// (alice/bob are never classified as workers).
pub fn forward_all(target: SocketAddr) -> (Arc<dyn RoutingStrategy>, Arc<dyn WorkerRegistry>) {
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(sip_proxy::ForwardAllStrategy::new(ProxyAddr::from(target)));
    let registry: Arc<dyn WorkerRegistry> = Arc::new(StaticWorkerRegistry::from_entries(vec![]));
    (strategy, registry)
}
