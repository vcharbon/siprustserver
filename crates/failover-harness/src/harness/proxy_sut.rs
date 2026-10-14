//! The proxy SUT — a real LoadBalancer over a SimulatedWorkerRegistry — and
//! the registry handle a scenario drives it through.

use std::net::SocketAddr;
use std::sync::Arc;

use observe::NodeScope;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerHealth, WorkerRegistry};
use sip_proxy::{ProxyAddr, ProxyMetrics};
use tokio::task::JoinHandle;

/// A running load-balancing proxy SUT + its driver-side registry handle, so the
/// scenario can flip a worker's health (dead on crash, alive+ready on recovery).
pub struct ProxySut {
    pub(super) addr: SocketAddr,
    /// Dual-face mode: the external (caller-plane) face's address. `None` for
    /// the classic single-face proxy.
    pub(super) ext_addr: Option<SocketAddr>,
    pub(super) registry: SimulatedWorkerRegistry,
    pub(super) metrics: Arc<ProxyMetrics>,
    pub(super) task: JoinHandle<()>,
    /// The real OPTIONS health-probe loop (ADR-0012). `Some` when the proxy was
    /// stood up via [`FailoverHarness::spawn_proxy_with_health_probe`]: health is
    /// then driven by actual probe replies (200→Alive, 503 not-ready→NotReady)
    /// instead of `set_health`, so a rebooted worker's Unknown→NotReady→Alive
    /// lifecycle — the state the response-path reverse-failover branches on — is
    /// exercised for real. Aborted on drop.
    ///
    /// [`FailoverHarness::spawn_proxy_with_health_probe`]: super::FailoverHarness::spawn_proxy_with_health_probe
    pub(super) probe_task: Option<JoinHandle<()>>,
    /// The node the proxy serves: its tasks run under it, and every registry
    /// change the scenario drives is lined as it.
    pub(super) node: NodeScope,
}

impl ProxySut {
    /// The proxy's listen address (alice/bob send through it). In dual-face
    /// mode this is the INTERNAL face (the workers' outbound proxy target).
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Dual-face mode: the EXTERNAL face's address — what callers dial
    /// `through(..)`. Panics on a single-face proxy (a test asking for the
    /// external face of a single-face proxy is a topology bug).
    pub fn ext_addr(&self) -> SocketAddr {
        self.ext_addr.expect("ext_addr on a single-face proxy — spawn with spawn_proxy_dual")
    }

    /// The proxy's metrics.
    pub fn metrics(&self) -> &Arc<ProxyMetrics> {
        &self.metrics
    }

    /// Mark a worker's health (e.g. `Dead` on crash, `Alive` on recovery).
    pub fn set_health(&self, ordinal: &str, health: WorkerHealth) {
        self.node.enter(|| self.registry.set_health(ordinal, health));
    }

    /// Re-learn a worker's address — the proxy's k8s-EndpointSlice path. On reboot
    /// a pod returns at a NEW IP, and in-dialog routing only follows it once the
    /// registry resolves the worker ordinal (carried in the signed Record-Route
    /// cookie) to the new address. Without this, a cookie-routed in-dialog request
    /// — e.g. the 200 coming back for the rebooted worker's own keepalive OPTIONS —
    /// would target the dead address and be lost.
    pub fn set_address(&self, ordinal: &str, addr: SocketAddr) {
        let addr = ProxyAddr::new(addr.ip().to_string(), addr.port());
        self.node.enter(|| self.registry.set_address(ordinal, addr));
    }

    /// Drop `ordinal` from the proxy's MEMBERSHIP — the endpoint is gone from
    /// its view entirely (not a health annotation: the ordinal no longer
    /// resolves, so nothing routes to it and no cookie can name it). The
    /// worker's process is untouched.
    pub fn remove_worker(&self, ordinal: &str) {
        self.node.enter(|| self.registry.remove(ordinal));
    }

    /// Publish `ordinal` at `addr` in the proxy's membership, health `Unknown`
    /// — a freshly observed endpoint the probe has not judged yet. Use
    /// [`set_health`](Self::set_health) to state a judged health.
    pub fn add_worker(&self, ordinal: &str, addr: SocketAddr) {
        let entry = sip_proxy::registry::WorkerEntry {
            id: ordinal.to_string(),
            address: ProxyAddr::new(addr.ip().to_string(), addr.port()),
            health: WorkerHealth::Unknown,
            draining_since: None,
            first_seen_at_ms: None,
        };
        self.node.enter(|| self.registry.add(entry));
    }

    /// The health the proxy holds for the worker bound at `addr`, an address
    /// that left the set included (`Dead` for Timer H after a departure; `None`
    /// once its window closed or for an address the pool never served).
    pub fn health_at(&self, addr: SocketAddr) -> Option<WorkerHealth> {
        self.registry
            .lookup_by_address(&ProxyAddr::new(addr.ip().to_string(), addr.port()))
            .map(|w| w.health)
    }

    /// The proxy's CURRENT health view of a worker (as the registry holds it).
    /// When a health probe is running this reflects real probe replies; callers
    /// poll it to wait for a rebooted worker to be re-confirmed `Alive` by the
    /// probe (rather than asserting it via `set_health`).
    pub fn health(&self, ordinal: &str) -> Option<WorkerHealth> {
        self.registry.resolve(ordinal).map(|w| w.health)
    }
}

impl Drop for ProxySut {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(p) = self.probe_task.take() {
            p.abort();
        }
    }
}
