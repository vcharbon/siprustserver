//! `ComposedWorkerRegistry` — the production [`WorkerRegistry`]: a [`WorkerSet`]
//! (shared `topology::Membership` ⊕ health projection) driven by a live membership
//! source via the shared `topology::spawn_membership_reconcile` self-heal loop.
//!
//! In production the source is the `topology::K8sMembership` EndpointSlice informer
//! — the same watch the b2bua replication supervisor consumes (ADR-0011 X7 /
//! ADR-0012 D4). A worker reboot/scale flows through the watch automatically, with
//! no `PROXY_WORKERS` refresh / proxy redeploy. Every wakeup — a delta, a `Lagged`
//! overflow, or the periodic tick — recomposes the projection from the
//! authoritative snapshot, so a missed/lagged delta self-heals exactly as the repl
//! supervisor does (D1/D2). Health stays the OPTIONS probe's concern, written via
//! [`control`](ComposedWorkerRegistry::control) into the annotation overlay.

use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use topology::Membership;

use crate::addr::ProxyAddr;

use super::control::{WorkerRegistryControl, WorkerSetControl};
use super::projection::WorkerSet;
use super::{WorkerEntry, WorkerRegistry};

/// Cadence of the belt-and-suspenders snapshot reconcile (ADR-0012 D2/D4): any
/// missed delta self-heals; an unchanged set is a no-op.
const RECONCILE_PERIOD: Duration = Duration::from_secs(5);

/// A worker registry composed from a live [`Membership`] source. The background
/// membership loop is aborted on drop.
pub struct ComposedWorkerRegistry {
    set: Arc<WorkerSet>,
    task: tokio::task::JoinHandle<()>,
}

impl ComposedWorkerRegistry {
    /// Build over `membership` with a fresh, empty health overlay, then spawn the
    /// shared membership-reconcile loop. The driver's synchronous initial reconcile
    /// recomposes the projection before this returns, so the registry is current
    /// the moment it is handed out. `sip_port` is the cluster-wide SIP port appended
    /// to each peer's Pod IP (membership is port-agnostic).
    pub fn spawn(membership: Arc<dyn Membership>, sip_port: u16, clock: Clock) -> Self {
        let set = Arc::new(WorkerSet::new(membership.clone(), sip_port, clock));
        let s = set.clone();
        // The shared self-heal loop (ADR-0012 D1/D2), identical to the one the
        // b2bua repl supervisor consumes. Every wakeup just recomposes from the
        // authoritative snapshot.
        let task = topology::spawn_membership_reconcile(membership, RECONCILE_PERIOD, move |_snapshot| s.recompose());
        Self { set, task }
    }

    /// The OPTIONS health-write seam. Writes land in the annotation overlay (never
    /// membership) and trigger a recompose.
    pub fn control(&self) -> Arc<dyn WorkerRegistryControl> {
        Arc::new(WorkerSetControl::new(self.set.clone()))
    }
}

impl Drop for ComposedWorkerRegistry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl WorkerRegistry for ComposedWorkerRegistry {
    fn snapshot(&self) -> Vec<WorkerEntry> {
        self.set.snapshot()
    }
    fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.set.resolve(id)
    }
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.set.lookup_by_address(addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::WorkerHealth;
    use topology::{Peer, SimulatedMembership};

    #[tokio::test]
    async fn composes_membership_and_health_end_to_end() {
        let sim = SimulatedMembership::with_clock(vec![Peer::new("w0", "10.0.0.1")], Clock::test_at(0));
        let reg = ComposedWorkerRegistry::spawn(Arc::new(sim.clone()), 5060, Clock::test_at(0));
        let control = reg.control();

        // Initial reconcile ran synchronously: w0 present, Unknown, cluster port.
        let w0 = reg.resolve("w0").expect("w0 present after sync initial reconcile");
        assert_eq!(w0.health, WorkerHealth::Unknown);
        assert_eq!(w0.address, ProxyAddr::new("10.0.0.1", 5060));

        // Health write composes in without touching membership.
        control.set_health("w0", WorkerHealth::Alive);
        assert_eq!(reg.resolve("w0").unwrap().health, WorkerHealth::Alive);

        // A membership add flows through the shared loop; poll for the rebuild.
        sim.add(Peer::new("w1", "10.0.0.2"));
        let mut w1 = None;
        for _ in 0..100 {
            if let Some(w) = reg.resolve("w1") {
                w1 = Some(w);
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(w1.is_some(), "w1 picked up from membership delta");
        // w0's probe-written health survived the membership change (separation holds).
        assert_eq!(reg.resolve("w0").unwrap().health, WorkerHealth::Alive);
        assert_eq!(reg.lookup_by_address(&ProxyAddr::new("10.0.0.2", 5060)).unwrap().id, "w1");
    }
}
