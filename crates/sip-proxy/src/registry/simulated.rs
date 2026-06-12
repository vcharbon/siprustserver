//! Simulated worker registry — drives worker membership + health from a test. It
//! is both a [`WorkerRegistry`] (read seam) and a control handle
//! (`add`/`remove`/`set_health`/`set_address`).
//!
//! Like every registry it is a thin wrapper over the shared [`WorkerSet`]: the
//! identity (ordinal + host) is a [`topology::SimulatedMembership`] sharing the
//! injected [`Clock`]; the per-worker port + health are annotation presets. Each
//! mutator drives the membership and/or the overlay, then **recomposes eagerly**,
//! so the public seam stays synchronous (tests assert right after a mutation, no
//! `.await`). Because the recompose reads the authoritative membership snapshot,
//! it is inherently lag-immune — there is no delta replay to overflow.

use std::sync::Arc;

use sip_clock::Clock;
use topology::{Peer, SimulatedMembership};

use crate::addr::ProxyAddr;

use super::projection::WorkerSet;
use super::{WorkerEntry, WorkerHealth, WorkerRegistry};

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
    clock: Clock,
    /// When set, stamp `first_seen_at_ms` from the clock on every `add` whose
    /// entry doesn't already carry one (the source's `autoStampFirstSeenAtMs`).
    auto_first_seen: bool,
}

impl SimulatedWorkerRegistry {
    pub fn new(initial: Vec<WorkerEntry>) -> Self {
        Self::build(initial, Clock::system(), false)
    }

    /// Build with an injected clock (tests use `Clock::test_at(..)` so
    /// `draining_since` / `first_seen_at_ms` advance with `tokio::time`).
    pub fn with_clock(initial: Vec<WorkerEntry>, clock: Clock) -> Self {
        Self::build(initial, clock, false)
    }

    fn build(initial: Vec<WorkerEntry>, clock: Clock, auto_first_seen: bool) -> Self {
        let peers = initial.iter().map(|e| Peer::new(e.id.clone(), e.address.host.clone())).collect();
        let membership = SimulatedMembership::with_clock(peers, clock.clone());
        let set = Arc::new(WorkerSet::new(Arc::new(membership.clone()), DEFAULT_PORT, clock.clone()));
        for e in &initial {
            set.preset(&e.id, e.address.host.clone(), e.address.port, e.health, e.draining_since, e.first_seen_at_ms);
        }
        set.recompose();
        Self { set, membership, clock, auto_first_seen }
    }

    /// Stamp `first_seen_at_ms` from the clock on every `add` (fresh-pod guard).
    pub fn auto_stamp_first_seen(mut self) -> Self {
        self.auto_first_seen = true;
        self
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Add (or replace) a worker. Identity (ordinal + host) flows through the
    /// topology membership; the proxy's port/health/timing are seeded as an
    /// annotation preset, then the eager recompose materialises the entry.
    pub fn add(&self, mut entry: WorkerEntry) {
        if self.auto_first_seen && entry.first_seen_at_ms.is_none() {
            entry.first_seen_at_ms = Some(self.now_ms());
        }
        self.set.preset(
            &entry.id,
            entry.address.host.clone(),
            entry.address.port,
            entry.health,
            entry.draining_since,
            entry.first_seen_at_ms,
        );
        self.membership.add(Peer::new(entry.id.clone(), entry.address.host.clone()));
        self.set.recompose();
    }

    /// Remove a worker (no-op if absent). Membership prunes the ordinal; the
    /// recompose drops its annotation.
    pub fn remove(&self, id: &str) {
        if self.set.resolve(id).is_none() {
            return;
        }
        self.membership.remove(id);
        self.set.recompose();
    }

    /// Set a worker's health (no-op if unknown or unchanged). Entering `Draining`
    /// stamps `draining_since` from the clock. Health is a proxy-layer concern
    /// (annotation overlay), never membership.
    pub fn set_health(&self, id: &str, health: WorkerHealth) {
        self.set.set_health(id, health);
    }

    /// Change a worker's address (no-op if unknown or unchanged). The host is
    /// membership identity (driven through topology, preserving health); the port
    /// is the proxy's per-worker annotation.
    pub fn set_address(&self, id: &str, address: ProxyAddr) {
        let Some(cur) = self.set.resolve(id) else {
            return;
        };
        if cur.address == address {
            return;
        }
        if cur.address.host != address.host {
            // Host change is membership identity. Re-seed the annotation at the new
            // host (carrying the current health/timing + new port) so the recompose
            // preserves it rather than resetting to a fresh endpoint.
            self.set.preset(id, address.host.clone(), address.port, cur.health, cur.draining_since, cur.first_seen_at_ms);
            self.membership.change_address(Peer::new(id.to_string(), address.host.clone()));
            self.set.recompose();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    // Paused tokio time so the injected `Clock::test_at` has zero real elapsed
    // between construction and the `now_ms()` stamp — the `draining_since` /
    // `first_seen_at_ms` exact-equality assertions are then deterministic.
    #[tokio::test(start_paused = true)]
    async fn add_set_health_remove_update_state() {
        let reg = SimulatedWorkerRegistry::with_clock(vec![], Clock::test_at(1_000));
        reg.add(WorkerEntry::alive("b2b-1", ProxyAddr::new("10.0.0.2", 5070)));
        assert_eq!(reg.resolve("b2b-1").unwrap().address, ProxyAddr::new("10.0.0.2", 5070));
        assert_eq!(reg.resolve("b2b-1").unwrap().health, WorkerHealth::Alive);

        reg.set_health("b2b-1", WorkerHealth::Draining);
        let w = reg.resolve("b2b-1").unwrap();
        assert_eq!(w.health, WorkerHealth::Draining);
        assert_eq!(w.draining_since, Some(1_000), "draining_since stamped from the clock");

        // Idempotent: setting the same health leaves draining_since stamp intact.
        reg.set_health("b2b-1", WorkerHealth::Draining);
        assert_eq!(reg.resolve("b2b-1").unwrap().draining_since, Some(1_000));

        reg.remove("b2b-1");
        assert!(reg.snapshot().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn auto_first_seen_stamps_on_add() {
        let reg = SimulatedWorkerRegistry::with_clock(vec![], Clock::test_at(42)).auto_stamp_first_seen();
        reg.add(WorkerEntry::alive("w", ProxyAddr::new("127.0.0.1", 5070)));
        assert_eq!(reg.resolve("w").unwrap().first_seen_at_ms, Some(42));
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
    /// channel to overflow (the old `Lagged` resync hazard is gone). Driving many
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
