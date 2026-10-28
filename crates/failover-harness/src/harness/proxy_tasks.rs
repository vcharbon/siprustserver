//! A front proxy SUT's own tasks beside its core: the OPTIONS health probe,
//! when a scenario runs it, and the dead notifier, which tells each dead
//! verdict a health write makes to the other alive workers, signed with the
//! workers' cluster key (ADR-0050 §3).

use std::net::SocketAddr;
use std::sync::Arc;

use observe::NodeScope;
use sip_proxy::health::{DeadNotifier, HealthProbe, HealthProbeConfig};
use sip_proxy::load_observer::WorkerLoadObserver;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::WorkerRegistry;
use sip_txn::IdGen;
use tokio::task::JoinHandle;

use super::FailoverHarness;

/// How many ports above a front proxy its dead notifier binds.
const NOTICE_PORT_OFFSET: u16 = 1000;

impl FailoverHarness {
    /// The REAL health probe of a proxy over `registry`, on its own endpoint
    /// at `at`, feeding `observer`, run as `node`.
    pub(super) async fn spawn_health_probe(
        &self,
        node: &NodeScope,
        at: &str,
        registry: &SimulatedWorkerRegistry,
        observer: Arc<WorkerLoadObserver>,
    ) -> JoinHandle<()> {
        // The REAL health probe: its own bound endpoint on the fabric, the
        // registry's control seam. Cadence is 10 s / 1.5 s — NOT the production
        // 1 s tick. Every tick fans a real sip-txn OPTIONS to every worker, and
        // under the paused clock that churn is pure CPU (each probe = client txn
        // + worker server txn + Timer E/F/J entries + recorded-trace events the
        // Drop-time RFC audit must scan): at the production cadence the long
        // keepalive cell (≈700 sim-seconds pumped in 100 ms chunks) ran ~420 s
        // wall, ~10 s at this cadence (super-linear: trace-scanning costs grow
        // with probe-event count). Nothing under test depends on the tick period
        // — cells pump until a health transition is observed (`reboot_and_reclaim` waits for
        // Alive, kill/drain set health directly) — so a slower tick is
        // semantics-preserving. Keep both values multiples of the 100 ms advance
        // chunk so there is no paused-clock reply race.
        let (probe_ep, _psock) = self.harness.bind_sut("proxy-probe", at).await;
        let registry_dyn: Arc<dyn WorkerRegistry> = Arc::new(registry.clone());
        let control = registry.control();
        let probe = HealthProbe::new(
            probe_ep,
            registry_dyn,
            control,
            observer,
            self.clock.clone(),
            Arc::new(IdGen::seeded(0x9809BE)),
            HealthProbeConfig { interval_ms: 10_000, timeout_ms: 1_500, threshold: 2 },
        );
        tokio::spawn(node.scope(probe.run()))
    }

    /// The dead notifier of the proxy at `proxy` over `registry`, on an
    /// endpoint of its own [`NOTICE_PORT_OFFSET`] ports up, run as `node`.
    pub(super) async fn spawn_dead_notifier(
        &self,
        node: &NodeScope,
        proxy: SocketAddr,
        registry: &SimulatedWorkerRegistry,
    ) -> Option<JoinHandle<()>> {
        let at = SocketAddr::new(proxy.ip(), proxy.port() + NOTICE_PORT_OFFSET);
        let (ep, _) = self.harness.bind_sut("proxy-notice", &at.to_string()).await;
        let registry: Arc<dyn WorkerRegistry> = Arc::new(registry.clone());
        let id_gen = Arc::new(IdGen::seeded(0x0D0E_AD00 ^ u64::from(proxy.port())));
        let notifier = node.enter(|| {
            DeadNotifier::new(
                ep,
                registry,
                b2bua_harness::cluster_key(),
                self.clock.clone(),
                id_gen,
            )
        });
        Some(tokio::spawn(node.scope(notifier.run())))
    }
}
