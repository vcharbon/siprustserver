//! Shared test support for the proxy+B2BUA e2e: spawn a real [`ProxyCore`] LB
//! as a System-Under-Test on the scenario-harness recording fabric. Mirrors
//! `sip-proxy/tests/common`; alice/bob are ordinary harness agents and the
//! proxy + B2BUA both auto-forward as datagrams arrive.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua_harness::spawn_proxy_core;
use scenario_harness::Harness;
use sip_clock::Clock;
use sip_proxy::ProxyMetrics;
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
/// single registered B2BUA worker at `worker` (so HRW always picks it). Thin
/// wrapper over the shared [`b2bua_harness::spawn_proxy_core`] primitive: bind
/// the proxy endpoint, hand it a one-element worker slice + a fresh
/// `Clock::test_at(0)`, and wrap the returned parts in the test-local
/// [`ProxySut`] (the registry/observer parts are unused on this single-worker,
/// no-health-probe path).
pub async fn spawn_lb_proxy(h: &Harness, addr: &str, worker_name: &str, worker: SocketAddr) -> ProxySut {
    let (ep, sock) = h.bind_sut("proxy", addr).await;
    let parts = spawn_proxy_core(ep, sock, &[(worker_name, worker)], Clock::test_at(0));
    ProxySut { addr: parts.addr, metrics: parts.metrics, task: parts.task }
}
