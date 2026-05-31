//! Simulated worker registry — port of `registry/simulated.ts`. Drives worker
//! membership + health from a test: it is both a [`WorkerRegistry`] (read seam)
//! and a control handle (`add`/`remove`/`set_health`/`set_address`). Mutations
//! emit [`RegistryEvent`]s; an `alive → draining` transition stamps
//! `draining_since` from the injected [`Clock`].

use std::sync::Arc;

use sip_clock::Clock;
use tokio::sync::broadcast;

use crate::addr::ProxyAddr;

use super::{RegistryEvent, RegistryState, WorkerEntry, WorkerHealth, WorkerRegistry};

/// A registry whose membership/health is driven imperatively by a test. Cheap
/// to clone — clones share the same underlying state + event channel.
#[derive(Clone)]
pub struct SimulatedWorkerRegistry {
    state: Arc<RegistryState>,
    clock: Clock,
    /// When set, stamp `first_seen_at_ms` from the clock on every `add` whose
    /// entry doesn't already carry one (the source's `autoStampFirstSeenAtMs`).
    auto_first_seen: bool,
}

impl SimulatedWorkerRegistry {
    pub fn new(initial: Vec<WorkerEntry>) -> Self {
        Self { state: Arc::new(RegistryState::new(initial)), clock: Clock::system(), auto_first_seen: false }
    }

    /// Build with an injected clock (tests use `Clock::test_at(..)` so
    /// `draining_since` / `first_seen_at_ms` advance with `tokio::time`).
    pub fn with_clock(initial: Vec<WorkerEntry>, clock: Clock) -> Self {
        Self { state: Arc::new(RegistryState::new(initial)), clock, auto_first_seen: false }
    }

    /// Stamp `first_seen_at_ms` from the clock on every `add` (fresh-pod guard).
    pub fn auto_stamp_first_seen(mut self) -> Self {
        self.auto_first_seen = true;
        self
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Add (or replace) a worker, emitting `Added`.
    pub fn add(&self, mut entry: WorkerEntry) {
        if self.auto_first_seen && entry.first_seen_at_ms.is_none() {
            entry.first_seen_at_ms = Some(self.now_ms());
        }
        let event = RegistryEvent::Added { entry: entry.clone() };
        self.state.mutate(
            |entries| {
                entries.retain(|w| w.id != entry.id);
                entries.push(entry);
            },
            event,
        );
    }

    /// Remove a worker, emitting `Removed` (no-op if absent).
    pub fn remove(&self, id: &str) {
        if self.state.resolve(id).is_none() {
            return;
        }
        self.state.mutate(|entries| entries.retain(|w| w.id != id), RegistryEvent::Removed { id: id.to_string() });
    }

    /// Set a worker's health, emitting `HealthChanged` (no-op if unknown or
    /// unchanged). Entering `Draining` stamps `draining_since` from the clock.
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
    /// unchanged).
    pub fn set_address(&self, id: &str, address: ProxyAddr) {
        let Some(cur) = self.state.resolve(id) else {
            return;
        };
        if cur.address == address {
            return;
        }
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

    #[test]
    fn add_remove_set_health_emit_events() {
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

    #[test]
    fn auto_first_seen_stamps_on_add() {
        let reg = SimulatedWorkerRegistry::with_clock(vec![], Clock::test_at(42)).auto_stamp_first_seen();
        reg.add(WorkerEntry::alive("w", ProxyAddr::new("127.0.0.1", 5070)));
        assert_eq!(reg.resolve("w").unwrap().first_seen_at_ms, Some(42));
    }
}
