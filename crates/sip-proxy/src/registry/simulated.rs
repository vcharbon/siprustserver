//! Simulated worker registry — port of `registry/simulated.ts`. Drives worker
//! membership + health from a test: it is both a [`WorkerRegistry`] (read seam)
//! and a control handle (`add`/`remove`/`set_health`/`set_address`). Mutations
//! emit [`RegistryEvent`]s; an `alive → draining` transition stamps
//! `draining_since` from the injected [`Clock`].
//!
//! **Membership identity (ordinal + host) is sourced from the `topology` crate**
//! (S1b): the peer set is backed by a [`topology::SimulatedMembership`] sharing
//! the SAME injected [`Clock`]. `add`/`remove`/`set_address` drive that
//! membership for the *identity* (who-is-in-the-cluster + host); a topology
//! `changes()` subscription is then drained synchronously to reconcile the
//! proxy's richer [`WorkerEntry`] view (port + health + timing stamps) and to
//! emit the proxy-level [`RegistryEvent`]s. Health and draining are proxy-layer
//! concerns emitted by this layer directly, never by topology.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sip_clock::Clock;
use tokio::sync::broadcast;
use topology::{MemberDelta, Membership, Peer, SimulatedMembership};

use crate::addr::ProxyAddr;

use super::{RegistryEvent, RegistryState, WorkerEntry, WorkerHealth, WorkerRegistry};

/// Per-ordinal proxy annotations the topology membership does NOT carry (port +
/// the intended health/timing for an in-flight `add`). Stashed so the
/// reconciler can stamp them onto the `WorkerEntry` when it observes the
/// corresponding topology delta.
#[derive(Default)]
struct Pending {
    /// Full entry intended by an in-flight `add` (carries port/health/timing).
    add: HashMap<String, WorkerEntry>,
    /// New port for an in-flight host change (topology is port-agnostic).
    port: HashMap<String, u16>,
}

/// A registry whose membership/health is driven imperatively by a test. Cheap
/// to clone — clones share the same underlying state + event channel.
#[derive(Clone)]
pub struct SimulatedWorkerRegistry {
    /// Port-and-health-annotated materialisation read on the hot path (the
    /// outward seam — unchanged shape/semantics).
    state: Arc<RegistryState>,
    /// Membership identity source of truth (ordinal + host, port-agnostic),
    /// clock-injected with the SAME `Clock` as this registry.
    membership: SimulatedMembership,
    /// Topology delta subscription, drained synchronously after each membership
    /// mutation to reconcile `state`. Shared across clones so concurrent drivers
    /// reconcile the one shared `state`.
    deltas: Arc<Mutex<broadcast::Receiver<MemberDelta>>>,
    /// Proxy annotations awaiting the reconcile of their topology delta.
    pending: Arc<Mutex<Pending>>,
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
        // Seed the topology membership identity (ordinal + host) from the initial
        // entries; the proxy's `RegistryState` keeps the full port/health view.
        // The initial set is wired straight in (no deltas emitted for the seed —
        // `changes()` has no backfill), matching the prior constructor.
        let peers = initial.iter().map(|e| Peer::new(e.id.clone(), e.address.host.clone())).collect();
        let membership = SimulatedMembership::with_clock(peers, clock.clone());
        let deltas = membership.changes();
        Self {
            state: Arc::new(RegistryState::new(initial)),
            membership,
            deltas: Arc::new(Mutex::new(deltas)),
            pending: Arc::new(Mutex::new(Pending::default())),
            clock,
            auto_first_seen,
        }
    }

    /// Stamp `first_seen_at_ms` from the clock on every `add` (fresh-pod guard).
    pub fn auto_stamp_first_seen(mut self) -> Self {
        self.auto_first_seen = true;
        self
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Drain every pending topology delta and reconcile it into the proxy's
    /// annotated `state`, emitting the matching `RegistryEvent`. Runs
    /// synchronously right after the membership mutation that produced the
    /// delta, so the public seam stays synchronous (tests `try_recv` right
    /// after a mutation, with no `.await`).
    fn reconcile(&self) {
        loop {
            let delta = {
                let mut rx = self.deltas.lock().unwrap();
                match rx.try_recv() {
                    Ok(d) => d,
                    Err(_) => break,
                }
            };
            match delta {
                MemberDelta::Added(peer) => {
                    // Materialise the proxy entry from the intended `add`
                    // annotations (port/health/timing), with the host owned by
                    // membership.
                    let pend = self.pending.lock().unwrap().add.remove(&peer.ordinal);
                    let entry = match pend {
                        Some(mut e) => {
                            e.address.host = peer.host;
                            e
                        }
                        // No stash (shouldn't happen via the public API) — fall
                        // back to a default alive entry on the membership host.
                        None => WorkerEntry::alive(peer.ordinal.clone(), ProxyAddr::new(peer.host, 0)),
                    };
                    let event = RegistryEvent::Added { entry: entry.clone() };
                    self.state.mutate(
                        |entries| {
                            entries.retain(|w| w.id != entry.id);
                            entries.push(entry);
                        },
                        event,
                    );
                }
                MemberDelta::Removed(ordinal) => {
                    self.state.mutate(
                        |entries| entries.retain(|w| w.id != ordinal),
                        RegistryEvent::Removed { id: ordinal.clone() },
                    );
                }
                MemberDelta::AddressChanged(peer) => {
                    let Some(cur) = self.state.resolve(&peer.ordinal) else {
                        continue;
                    };
                    // Membership owns the host; pick up a co-changed port if the
                    // proxy stashed one, else keep the existing port.
                    let port = self.pending.lock().unwrap().port.remove(&peer.ordinal).unwrap_or(cur.address.port);
                    let new_addr = ProxyAddr::new(peer.host, port);
                    let event = RegistryEvent::AddressChanged {
                        id: peer.ordinal.clone(),
                        from: cur.address,
                        to: new_addr.clone(),
                    };
                    self.state.mutate(
                        |entries| {
                            if let Some(w) = entries.iter_mut().find(|w| w.id == peer.ordinal) {
                                w.address = new_addr;
                            }
                        },
                        event,
                    );
                }
            }
        }
    }

    /// Add (or replace) a worker, emitting `Added`. Membership identity flows
    /// through the topology layer; this stamps the proxy's port/health/timing.
    pub fn add(&self, mut entry: WorkerEntry) {
        if self.auto_first_seen && entry.first_seen_at_ms.is_none() {
            entry.first_seen_at_ms = Some(self.now_ms());
        }
        self.pending.lock().unwrap().add.insert(entry.id.clone(), entry.clone());
        // Drive topology identity (ordinal + host); reconcile stamps the rest.
        self.membership.add(Peer::new(entry.id.clone(), entry.address.host.clone()));
        self.reconcile();
    }

    /// Remove a worker, emitting `Removed` (no-op if absent).
    pub fn remove(&self, id: &str) {
        if self.state.resolve(id).is_none() {
            return;
        }
        self.membership.remove(id);
        self.reconcile();
    }

    /// Set a worker's health, emitting `HealthChanged` (no-op if unknown or
    /// unchanged). Entering `Draining` stamps `draining_since` from the clock.
    ///
    /// Health is a **proxy-layer** concern (not membership), so it is applied
    /// directly to `state` and does not flow through topology.
    pub fn set_health(&self, id: &str, health: WorkerHealth) {
        let Some(cur) = self.state.resolve(id) else {
            return;
        };
        if cur.health == health {
            return;
        }
        let now = self.now_ms();
        let event = RegistryEvent::HealthChanged { id: id.to_string(), from: cur.health, to: health };
        self.state.mutate(
            |entries| {
                if let Some(w) = entries.iter_mut().find(|w| w.id == id) {
                    w.health = health;
                    if health == WorkerHealth::Draining && w.draining_since.is_none() {
                        w.draining_since = Some(now);
                    }
                }
            },
            event,
        );
    }

    /// Change a worker's address, emitting `AddressChanged` (no-op if unknown or
    /// unchanged). The host is membership identity (driven through topology); the
    /// port is the proxy's transport annotation.
    pub fn set_address(&self, id: &str, address: ProxyAddr) {
        let Some(cur) = self.state.resolve(id) else {
            return;
        };
        if cur.address == address {
            return;
        }
        if cur.address.host != address.host {
            // Host change is membership identity → drive topology; stash the
            // (possibly co-changed) port so reconcile stamps it.
            self.pending.lock().unwrap().port.insert(id.to_string(), address.port);
            self.membership.change_address(Peer::new(id.to_string(), address.host.clone()));
            self.reconcile();
        } else {
            // Pure port change (same host): topology is port-agnostic, so this is
            // a proxy-only annotation; apply it directly and emit the event.
            let event =
                RegistryEvent::AddressChanged { id: id.to_string(), from: cur.address, to: address.clone() };
            self.state.mutate(
                |entries| {
                    if let Some(w) = entries.iter_mut().find(|w| w.id == id) {
                        w.address = address;
                    }
                },
                event,
            );
        }
    }
}

impl WorkerRegistry for SimulatedWorkerRegistry {
    fn snapshot(&self) -> Vec<WorkerEntry> {
        self.state.snapshot()
    }
    fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.state.resolve(id)
    }
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.state.lookup_by_address(addr)
    }
    fn changes(&self) -> broadcast::Receiver<RegistryEvent> {
        self.state.changes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Paused tokio time so the injected `Clock::test_at` has *zero* real elapsed
    // between construction and the `now_ms()` stamp — the `draining_since` /
    // `first_seen_at_ms` exact-equality assertions are then deterministic (a
    // plain `#[test]` rides real wall-clock and flakes when the stamp crosses a
    // 1 ms boundary; CLAUDE.md: tests use `start_paused`).
    #[tokio::test(start_paused = true)]
    async fn add_remove_set_health_emit_events() {
        let reg = SimulatedWorkerRegistry::with_clock(vec![], Clock::test_at(1_000));
        let mut rx = reg.changes();
        reg.add(WorkerEntry::alive("b2b-1", ProxyAddr::new("10.0.0.2", 5070)));
        assert!(matches!(rx.try_recv().unwrap(), RegistryEvent::Added { .. }));

        reg.set_health("b2b-1", WorkerHealth::Draining);
        let ev = rx.try_recv().unwrap();
        assert!(matches!(ev, RegistryEvent::HealthChanged { to: WorkerHealth::Draining, .. }));
        // draining_since stamped from the clock.
        assert_eq!(reg.resolve("b2b-1").unwrap().draining_since, Some(1_000));

        // Idempotent: setting the same health emits nothing.
        reg.set_health("b2b-1", WorkerHealth::Draining);
        assert!(rx.try_recv().is_err());

        reg.remove("b2b-1");
        assert!(matches!(rx.try_recv().unwrap(), RegistryEvent::Removed { .. }));
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
        let mut rx = reg.changes();
        // Host change: identity flows through topology, port preserved.
        reg.set_address("w", ProxyAddr::new("10.0.0.9", 5070));
        let ev = rx.try_recv().unwrap();
        assert!(matches!(ev, RegistryEvent::AddressChanged { .. }));
        assert_eq!(reg.resolve("w").unwrap().address, ProxyAddr::new("10.0.0.9", 5070));
        // topology membership host is the source of truth.
        assert_eq!(reg.membership.resolve("w").unwrap().host, "10.0.0.9");
    }

    #[test]
    fn set_address_port_only_change_is_proxy_local() {
        let reg = SimulatedWorkerRegistry::with_clock(vec![], Clock::test_at(0));
        reg.add(WorkerEntry::alive("w", ProxyAddr::new("10.0.0.1", 5070)));
        let mut rx = reg.changes();
        reg.set_address("w", ProxyAddr::new("10.0.0.1", 5080));
        assert!(matches!(rx.try_recv().unwrap(), RegistryEvent::AddressChanged { .. }));
        assert_eq!(reg.resolve("w").unwrap().address, ProxyAddr::new("10.0.0.1", 5080));
        // topology host unchanged (port is port-agnostic to membership).
        assert_eq!(reg.membership.resolve("w").unwrap().host, "10.0.0.1");
    }
}
