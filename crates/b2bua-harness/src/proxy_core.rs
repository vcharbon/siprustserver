//! The shared load-balancing `ProxyCore` spawn primitive.
//!
//! The registry→hmac→load-observer→LoadBalancerStrategy→ProxyCoreBuilder→spawn
//! wiring for a real `sip_proxy::ProxyCore` LB SUT is identical between the
//! single-worker proxy helper in this crate's `tests/common/mod.rs`
//! (`spawn_lb_proxy`) and the multi-worker `failover-harness`
//! (`FailoverHarness::spawn_proxy_inner`): the ONLY things that vary are
//! single-vs-N workers (single = a one-element slice), the clock source, and the
//! failover-only health-probe task + retained registry handle. The endpoint is
//! bound by each caller (this crate via `Harness::bind_sut`, failover via its
//! `HarnessHandle::bind_sut`) — both yield the same `(endpoint, sock)` — so, as
//! with `spawn_b2bua_core`, the caller binds and passes the bound pair in; the
//! SHARED part is everything from "build the registry" to "spawn the core",
//! captured by [`spawn_proxy_core`] returning [`ProxyCoreParts`]. Each caller
//! wraps the parts in its own `ProxySut` (the two structs differ — the test one
//! is minimal; failover's retains the registry handle for health control and
//! adds the OPTIONS probe task — so only the wiring is unified, not the struct).

use std::net::SocketAddr;
use std::sync::Arc;

use sip_clock::Clock;
use sip_net::UdpEndpoint;
use sip_proxy::load_observer::{LoadObserverConfig, WorkerLoadObserver};
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerEntry, WorkerRegistry};
use sip_proxy::security::hmac::{HmacKey, StaticHmacKeyProvider};
use sip_proxy::{LoadBalancerStrategy, ProxyAddr, ProxyCoreBuilder, ProxyMetrics, RoutingStrategy};
use sip_txn::IdGen;
use tokio::task::JoinHandle;

/// The wired-but-caller-owned pieces of a load-balancing `ProxyCore` SUT, as
/// returned by [`spawn_proxy_core`]. The caller wraps these in its own
/// `ProxySut`. The `registry` is the CONCRETE [`SimulatedWorkerRegistry`] the
/// running `ProxyCore` resolves through (a clone of the same instance), so a
/// caller that retains it can flip worker health/address and have the live proxy
/// observe the change.
pub struct ProxyCoreParts {
    /// The proxy's listen address (the bound `sock`).
    pub addr: SocketAddr,
    /// The concrete registry the live `ProxyCore` resolves through. Retain it to
    /// drive worker health/address; a probe loop can take its `control()` seam.
    pub registry: SimulatedWorkerRegistry,
    /// The proxy's metrics (the same `Arc` wired into the core).
    pub metrics: Arc<ProxyMetrics>,
    /// The AIMD load observer the `LoadBalancerStrategy` reads. Returned so a
    /// caller running a `HealthProbe` can feed the SAME instance the probe's
    /// `X-Overload` payloads (failover wires this; the single-worker test
    /// helper drops it).
    pub observer: Arc<WorkerLoadObserver>,
    /// The spawned recv-loop task. The caller's `ProxySut::drop` aborts it.
    pub task: JoinHandle<()>,
}

/// Spawn a real load-balancing `ProxyCore` on the harness fabric fronting
/// `workers` (single-worker = a one-element slice; HRW always picks the lone
/// entry). The caller binds the proxy endpoint (`Harness::bind_sut("proxy", …)`)
/// and passes the resulting `(endpoint, addr)` plus the shared `clock`; this
/// builds the `SimulatedWorkerRegistry` (all workers alive), the HMAC provider
/// (`k1`/`[7;32]`), the load observer, the `LoadBalancerStrategy`, and the core
/// (id-gen seed `0xC0FFEE`), spawns its run loop, and returns the common
/// [`ProxyCoreParts`] for the caller to wrap in its own `ProxySut`.
///
/// The health-probe task and the retained-registry health controls (the
/// failover-only extras) stay at the call site: `failover-harness` takes
/// `parts.registry.control()` for the `HealthProbe` and keeps `parts.registry`
/// in its `ProxySut` so `set_health`/`set_address` drive the live proxy.
pub fn spawn_proxy_core(
    endpoint: Box<dyn UdpEndpoint>,
    addr: SocketAddr,
    workers: &[(&str, SocketAddr)],
    clock: Clock,
) -> ProxyCoreParts {
    spawn_proxy_core_with(endpoint, addr, None, workers, clock, 0xC0FFEE)
}

/// The **external face** of a dual-face proxy SUT (see
/// [`spawn_proxy_core_with`]): a second bound endpoint on the caller plane +
/// its advertise + the internal-plane CIDR list driving the egress face picker
/// (`PROXY_FACE_INT_CIDRS` equivalent).
pub struct ExternalProxyFace {
    pub endpoint: Box<dyn UdpEndpoint>,
    pub addr: SocketAddr,
    pub int_cidrs: sip_proxy::FaceCidrs,
}

/// [`spawn_proxy_core`] generalized: optional dual-face external endpoint and
/// an explicit `id_seed` (a takeover twin re-bound on the same VIPs must mint
/// DIFFERENT Via branches than the proxy it replaces — reusing the seed would
/// replay the dead proxy's branch sequence into the same dialogs). The run
/// loop serves the caller's node ([`observe::NodeScope`]).
pub fn spawn_proxy_core_with(
    endpoint: Box<dyn UdpEndpoint>,
    addr: SocketAddr,
    external: Option<ExternalProxyFace>,
    workers: &[(&str, SocketAddr)],
    clock: Clock,
    id_seed: u64,
) -> ProxyCoreParts {
    let entries: Vec<WorkerEntry> = workers
        .iter()
        .map(|(id, sa)| WorkerEntry::alive(*id, ProxyAddr::new(sa.ip().to_string(), sa.port())))
        .collect();
    let registry = SimulatedWorkerRegistry::with_clock(entries, clock.clone());
    let registry_dyn: Arc<dyn WorkerRegistry> = Arc::new(registry.clone());
    let hmac =
        Arc::new(StaticHmacKeyProvider::new(HmacKey::new("k1", vec![7u8; 32]), None).unwrap());
    let observer = Arc::new(WorkerLoadObserver::new(LoadObserverConfig::default()));
    let strategy: Arc<dyn RoutingStrategy> = Arc::new(LoadBalancerStrategy::new(
        registry_dyn.clone(),
        hmac,
        observer.clone(),
        Arc::new(ProxyMetrics::new()),
        clock.clone(),
    ));

    let metrics = Arc::new(ProxyMetrics::new());
    let mut builder = ProxyCoreBuilder::new(ProxyAddr::from(addr), strategy, registry_dyn)
        .clock(clock)
        .id_gen(Arc::new(IdGen::seeded(id_seed)))
        .metrics(metrics.clone());
    if let Some(ext) = external {
        builder = builder.external_face(sip_proxy::ExternalFaceParts {
            endpoint: ext.endpoint,
            advertised: ProxyAddr::from(ext.addr),
            int_cidrs: ext.int_cidrs,
        });
    }
    let core = builder.build(endpoint);
    let task = observe::spawn_keeping_node(core.run());
    ProxyCoreParts { addr, registry, metrics, observer, task }
}
