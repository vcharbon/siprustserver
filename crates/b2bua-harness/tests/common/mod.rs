//! Shared test support for the proxy+B2BUA e2e: spawn a real [`ProxyCore`] LB
//! as a System-Under-Test on the scenario-harness recording fabric. Mirrors
//! `sip-proxy/tests/common`; alice/bob are ordinary harness agents and the
//! proxy + B2BUA both auto-forward as datagrams arrive.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use scenario_harness::Harness;
use sip_clock::Clock;
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::{
    LoadBalancerConfig, LoadBalancerStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics,
    RoutingStrategy,
};
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

/// Spawn a real load-balancing `ProxyCore` on the harness fabric, fronting the
/// single registered B2BUA worker at `worker` (so HRW always picks it).
pub async fn spawn_lb_proxy(h: &Harness, addr: &str, worker_name: &str, worker: SocketAddr) -> ProxySut {
    let registry: Arc<dyn WorkerRegistry> = Arc::new(SimulatedWorkerRegistry::with_clock(
        vec![WorkerEntry::alive(
            worker_name,
            ProxyAddr::new(worker.ip().to_string(), worker.port()),
        )],
        Clock::test_at(0),
    ));
    let hmac =
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
        registry.clone(),
        hmac,
        observer,
        Arc::new(ProxyMetrics::new()),
        Clock::test_at(0),
        LoadBalancerConfig::default(),
    ));

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
