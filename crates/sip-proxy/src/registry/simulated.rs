//! Simulated worker registry — drives worker membership + health from a test. It
//! is both a [`WorkerRegistry`] (read seam) and a control handle
//! (`add`/`remove`/`set_health`/`set_acceptance`/`set_address`).
//!
//! Like every registry it is a thin wrapper over the shared `WorkerSet`: the
//! identity (ordinal + host) is a [`topology::SimulatedMembership`] sharing the
//! injected [`Clock`]; the per-worker port, health and acceptance are
//! annotation presets. Each
//! mutator drives the membership and/or the overlay, then **recomposes eagerly**,
//! so the public seam stays synchronous (tests assert right after a mutation, no
//! `.await`). Because the recompose reads the authoritative membership snapshot,
//! it is inherently lag-immune — there is no delta replay to overflow.

use std::sync::Arc;

use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use crate::addr::ProxyAddr;
use crate::poison::locked;

use super::projection::WorkerSet;
use super::{Acceptance, WorkerEntry, WorkerHealth, WorkerRegistry};

/// Fallback port for a worker with no per-worker `port_override`. Unused in
/// practice — every simulated worker is added with an explicit port.
const DEFAULT_PORT: u16 = 5060;

/// A registry whose membership/health is driven imperatively by a test. Cheap to
/// clone — clones share the same underlying state (the `Arc<WorkerSet>` and the
/// shared `SimulatedMembership`).
#[derive(Clone)]
pub struct SimulatedWorkerRegistry {
    set: Arc<WorkerSet>,
    membership: SimulatedMembership,
    /// Each worker's per-worker port, kept across a not-ready window so
    /// [`set_ready`](Self::set_ready) can re-seed it on the return.
    ports: Arc<std::sync::Mutex<std::collections::HashMap<String, u16>>>,
}

impl SimulatedWorkerRegistry {
    pub fn new(initial: Vec<WorkerEntry>) -> Self {
        Self::build(initial, Clock::system())
    }

    /// Build with an injected clock (tests use `Clock::test_at(..)` so the
    /// departed-address windows advance with `tokio::time`).
    pub fn with_clock(initial: Vec<WorkerEntry>, clock: Clock) -> Self {
        Self::build(initial, clock)
    }

    fn build(initial: Vec<WorkerEntry>, clock: Clock) -> Self {
        let peers =
            initial.iter().map(|e| Peer::new(e.id.clone(), e.address.host.clone())).collect();
        let membership = SimulatedMembership::with_clock(peers, clock.clone());
        let set =
            Arc::new(WorkerSet::new(Arc::new(membership.clone()), DEFAULT_PORT, clock.clone()));
        let ports = initial.iter().map(|e| (e.id.clone(), e.address.port)).collect();
        for e in &initial {
            set.preset(&e.id, e.address.host.clone(), e.address.port, e.health, e.acceptance);
        }
        set.recompose();
        Self { set, membership, ports: Arc::new(std::sync::Mutex::new(ports)) }
    }

    /// Add (or replace) a worker. Identity (ordinal + host) flows through the
    /// topology membership; the proxy's port/health/acceptance are seeded as an
    /// annotation preset, then the eager recompose materialises the entry.
    pub fn add(&self, entry: WorkerEntry) {
        self.set.preset(
            &entry.id,
            entry.address.host.clone(),
            entry.address.port,
            entry.health,
            entry.acceptance,
        );
        locked(&self.ports, "simulated registry ports")
            .insert(entry.id.clone(), entry.address.port);
        self.membership.add(Peer::new(entry.id.clone(), entry.address.host.clone()));
        self.set.recompose();
    }

    /// Remove a worker (no-op if absent). Membership prunes the ordinal; the
    /// recompose drops its annotation.
    pub fn remove(&self, id: &str) {
        if self.set.resolve(id).is_none() {
            return;
        }
        locked(&self.ports, "simulated registry ports").remove(id);
        self.membership.remove(id);
        self.set.recompose();
    }

    /// Set a worker's endpoint `ready` condition (no-op if unknown). The worker
    /// stays a member; a `ready=false` one leaves the projection and its address
    /// is tombstoned, a `ready=true` one joins as a fresh `Unknown` endpoint
    /// (ADR-0031 D1). The orchestrator's condition, never the probe's health.
    pub fn set_ready(&self, id: &str, ready: bool) {
        let Some(cur) = self.membership.resolve(id) else {
            return;
        };
        if ready && !cur.ready {
            // The per-worker port is a proxy annotation, dropped with the
            // departure; re-seed it so the returning endpoint keeps its bind.
            // Everything else is what a fresh join gets.
            if let Some(port) = locked(&self.ports, "simulated registry ports").get(id).copied() {
                self.set.preset(
                    id,
                    cur.host.clone(),
                    port,
                    WorkerHealth::Unknown,
                    Acceptance::Accept,
                );
            }
        }
        self.membership.set_conditions(id, ready, cur.terminating);
        self.set.recompose();
    }

    /// Set a worker's health (no-op if unknown or unchanged). Health is a
    /// proxy-layer concern (annotation overlay), never membership.
    pub fn set_health(&self, id: &str, health: WorkerHealth) {
        self.set.set_health(id, health);
    }

    /// Set a worker's acceptance of new calls (no-op if unknown or unchanged).
    pub fn set_acceptance(&self, id: &str, acceptance: Acceptance) {
        self.set.set_acceptance(id, acceptance);
    }

    /// Change a worker's address (no-op if unknown, unchanged, or not ready — it
    /// resolves through the projection, which holds ready members only). The host
    /// is membership identity (driven through topology, preserving health); the
    /// port is the proxy's per-worker annotation.
    pub fn set_address(&self, id: &str, address: ProxyAddr) {
        let Some(cur) = self.set.resolve(id) else {
            return;
        };
        if cur.address == address {
            return;
        }
        locked(&self.ports, "simulated registry ports").insert(id.to_string(), address.port);
        if cur.address.host != address.host {
            // Host change is membership identity. Re-seed the annotation at the new
            // host (carrying the current health/acceptance + new port) so the
            // recompose preserves it rather than resetting to a fresh endpoint.
            self.set.preset(id, address.host.clone(), address.port, cur.health, cur.acceptance);
            self.membership.change_address(Peer::new(id.to_string(), address.host.clone()));
            self.set.recompose();
            // The preset hid the move from the recompose: tombstone the old address.
            self.set.depart(cur.address, id);
        } else {
            // Pure port change (same host): a proxy-only annotation.
            self.set.set_port_override(id, address.port);
            self.set.recompose();
        }
    }

    /// The cluster membership identity backing this registry (test introspection).
    pub fn membership(&self) -> &SimulatedMembership {
        &self.membership
    }

    /// A health-write [`WorkerRegistryControl`](super::control::WorkerRegistryControl)
    /// over this pool — the same `WorkerSet` adapter the production registries
    /// hand out, so probe wiring in tests matches the runner's exactly.
    pub fn control(&self) -> Arc<dyn super::control::WorkerRegistryControl> {
        Arc::new(super::control::WorkerSetControl::new(self.set.clone()))
    }
}

impl WorkerRegistry for SimulatedWorkerRegistry {
    fn snapshot(&self) -> Vec<WorkerEntry> {
        self.set.snapshot()
    }
    fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.set.resolve(id)
    }
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.set.lookup_by_address(addr)
    }
    fn rearm_departed(&self, addr: &ProxyAddr) {
        self.set.rearm_departed(addr)
    }
    fn synced(&self) -> bool {
        self.set.synced()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_set_health_set_acceptance_remove_update_state() {
        let reg = SimulatedWorkerRegistry::with_clock(vec![], Clock::test_at(1_000));
        reg.add(WorkerEntry::alive("b2b-1", ProxyAddr::new("10.0.0.2", 5070)));
        assert_eq!(reg.resolve("b2b-1").unwrap().address, ProxyAddr::new("10.0.0.2", 5070));
        assert_eq!(reg.resolve("b2b-1").unwrap().health, WorkerHealth::Alive);

        reg.set_acceptance("b2b-1", Acceptance::Refuse);
        let w = reg.resolve("b2b-1").unwrap();
        assert_eq!((w.health, w.acceptance), (WorkerHealth::Alive, Acceptance::Refuse));
        assert!(w.is_alive() && !w.accepts_new_calls());

        reg.set_health("b2b-1", WorkerHealth::Dead);
        let w = reg.resolve("b2b-1").unwrap();
        assert_eq!((w.health, w.acceptance), (WorkerHealth::Dead, Acceptance::Refuse));

        reg.remove("b2b-1");
        assert!(reg.snapshot().is_empty());
    }

    #[test]
    fn set_ready_false_departs_the_worker_and_true_readmits_it_unknown() {
        let reg = SimulatedWorkerRegistry::with_clock(vec![], Clock::test_at(0));
        reg.add(WorkerEntry::alive("w", ProxyAddr::new("10.0.0.1", 5070)));
        let addr = ProxyAddr::new("10.0.0.1", 5070);

        reg.set_ready("w", false);
        assert!(reg.resolve("w").is_none(), "unroutable while not ready");
        assert_eq!(reg.lookup_by_address(&addr).unwrap().health, WorkerHealth::Dead);
        assert!(reg.membership().resolve("w").is_some(), "still a member");

        reg.set_ready("w", true);
        assert_eq!(reg.resolve("w").unwrap().health, WorkerHealth::Unknown);
        assert_eq!(reg.resolve("w").unwrap().address, addr, "the per-worker port survives");
        // Unknown ordinal: nothing.
        reg.set_ready("ghost", false);
    }

    #[test]
    fn set_address_host_change_flows_through_topology() {
        let reg = SimulatedWorkerRegistry::with_clock(vec![], Clock::test_at(0));
        reg.add(WorkerEntry::alive("w", ProxyAddr::new("10.0.0.1", 5070)));
        // Host change: identity flows through topology, port preserved, health kept.
        reg.set_address("w", ProxyAddr::new("10.0.0.9", 5070));
        assert_eq!(reg.resolve("w").unwrap().address, ProxyAddr::new("10.0.0.9", 5070));
        assert_eq!(reg.resolve("w").unwrap().health, WorkerHealth::Alive);
        assert_eq!(reg.membership().resolve("w").unwrap().host, "10.0.0.9");
        // The address the worker left answers as departed, as a host move
        // through membership does (Timer H).
        let left = reg.lookup_by_address(&ProxyAddr::new("10.0.0.1", 5070));
        assert_eq!(left.map(|w| (w.id, w.health)), Some(("w".into(), WorkerHealth::Dead)));
    }

    #[test]
    fn set_address_port_only_change_is_proxy_local() {
        let reg = SimulatedWorkerRegistry::with_clock(vec![], Clock::test_at(0));
        reg.add(WorkerEntry::alive("w", ProxyAddr::new("10.0.0.1", 5070)));
        reg.set_address("w", ProxyAddr::new("10.0.0.1", 5080));
        assert_eq!(reg.resolve("w").unwrap().address, ProxyAddr::new("10.0.0.1", 5080));
        // topology host unchanged (port is port-agnostic to membership).
        assert_eq!(reg.membership().resolve("w").unwrap().host, "10.0.0.1");
    }

    /// The snapshot-based recompose is inherently lag-immune: there is no delta
    /// channel to overflow (no `Lagged` resync hazard). Driving many
    /// membership changes then a real removal still converges to the current set.
    #[test]
    fn bulk_membership_changes_converge() {
        let reg = SimulatedWorkerRegistry::with_clock(
            vec![
                WorkerEntry::alive("keep", ProxyAddr::new("10.0.0.1", 5070)),
                WorkerEntry::alive("drop", ProxyAddr::new("10.0.0.2", 5070)),
            ],
            Clock::test_at(0),
        );
        assert!(reg.resolve("keep").is_some() && reg.resolve("drop").is_some());
        for i in 0..300 {
            reg.add(WorkerEntry::alive(format!("ghost{i}"), ProxyAddr::new("10.9.9.9", 5070)));
        }
        reg.remove("drop");
        assert!(reg.resolve("drop").is_none(), "removed worker is dropped from the proxy view");
        assert!(reg.resolve("keep").is_some(), "surviving worker kept (no spurious removal)");
        assert_eq!(reg.resolve("keep").unwrap().address, ProxyAddr::new("10.0.0.1", 5070));
    }
}
