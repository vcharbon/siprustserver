//! [`WorkerRegistry`] — the B2BUA worker set the proxy load-balances across
//! (port of `registry/WorkerRegistry.ts`).
//!
//! The routing hot path reads `snapshot`/`resolve`/`lookup_by_address`
//! **synchronously and lock-free** (the worker set lives behind an
//! [`arc_swap::ArcSwap`]); only background mutators (health probe, k8s watcher)
//! write. `changes()` is a `tokio::sync::broadcast` of deltas (no backfill —
//! subscribers `snapshot` first).

use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::broadcast;

use crate::addr::ProxyAddr;

pub mod control;
pub mod simulated;
pub mod static_reg;

/// Worker identity (the source's branded `WorkerId`; a plain `String` here —
/// the registry impls reject empty/malformed ids at build time).
pub type WorkerId = String;

/// Health classification for a worker (D5 draining model).
///
/// `Unknown` (cold start, not yet probed) is **distinct from `Dead`** (confirmed
/// gone) — routing filters treat both as not-routable for new dialogs, but the
/// split lets future `Dead → Alive` recovery hysteresis avoid penalizing
/// cold-start workers. `NotReady` is a worker whose process is up but whose boot
/// replication drain is unfinished (answered OPTIONS `503 + not-ready`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerHealth {
    Unknown,
    Alive,
    NotReady,
    Draining,
    Dead,
}

/// A registered worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerEntry {
    pub id: WorkerId,
    pub address: ProxyAddr,
    pub health: WorkerHealth,
    /// Epoch ms when the worker first entered `Draining` (drives the LB's
    /// in-dialog grace window). `None` for non-draining.
    pub draining_since: Option<u64>,
    /// Epoch ms of the worker's "fresh pod" window start (drives the LB's
    /// fresh-pod guard). `None` → no guard.
    pub first_seen_at_ms: Option<u64>,
}

impl WorkerEntry {
    pub fn alive(id: impl Into<WorkerId>, address: ProxyAddr) -> Self {
        Self { id: id.into(), address, health: WorkerHealth::Alive, draining_since: None, first_seen_at_ms: None }
    }
}

/// Tagged delta emitted on observable state change (port of `RegistryEvent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryEvent {
    Added { entry: WorkerEntry },
    Removed { id: WorkerId },
    HealthChanged { id: WorkerId, from: WorkerHealth, to: WorkerHealth },
    AddressChanged { id: WorkerId, from: ProxyAddr, to: ProxyAddr },
}

/// The read seam consumers (LB, health probe) depend on. All reads are sync +
/// lock-free per the D4 non-blocking invariant.
pub trait WorkerRegistry: Send + Sync {
    /// Snapshot the current worker set.
    fn snapshot(&self) -> Vec<WorkerEntry>;
    /// Resolve a worker by id (`None` if unregistered/removed).
    fn resolve(&self, id: &str) -> Option<WorkerEntry>;
    /// Reverse-lookup the worker bound at `addr` (`None` for any non-worker
    /// source — Alice, Bob, an external SBC).
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry>;
    /// Subscribe to deltas from this point on (no backfill).
    fn changes(&self) -> broadcast::Receiver<RegistryEvent>;
}

/// Shared lock-free state backing both the read seam and the mutators. The
/// static + simulated registries are thin wrappers over this.
pub(crate) struct RegistryState {
    entries: ArcSwap<Vec<WorkerEntry>>,
    tx: broadcast::Sender<RegistryEvent>,
}

impl RegistryState {
    pub(crate) fn new(initial: Vec<WorkerEntry>) -> Self {
        let (tx, _rx) = broadcast::channel(256);
        Self { entries: ArcSwap::from_pointee(initial), tx }
    }

    pub(crate) fn snapshot(&self) -> Vec<WorkerEntry> {
        self.entries.load().as_ref().clone()
    }

    pub(crate) fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.entries.load().iter().find(|w| w.id == id).cloned()
    }

    pub(crate) fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.entries.load().iter().find(|w| &w.address == addr).cloned()
    }

    pub(crate) fn changes(&self) -> broadcast::Receiver<RegistryEvent> {
        self.tx.subscribe()
    }

    /// Replace the set with `f(current)` and emit `event` (best-effort — a
    /// dropped event when there are no subscribers is fine, `changes` has no
    /// backfill).
    pub(crate) fn mutate(&self, f: impl FnOnce(&mut Vec<WorkerEntry>), event: RegistryEvent) {
        let mut next = self.snapshot();
        f(&mut next);
        self.entries.store(Arc::new(next));
        let _ = self.tx.send(event);
    }

    /// Annotate a worker's health, emitting `HealthChanged` (no-op if unknown or
    /// unchanged). Entering `Draining` stamps `draining_since` from `now_ms`.
    /// The canonical health write shared by every registry + control adapter.
    pub(crate) fn set_health(&self, id: &str, health: WorkerHealth, now_ms: u64) {
        let Some(cur) = self.resolve(id) else {
            return;
        };
        if cur.health == health {
            return;
        }
        let event = RegistryEvent::HealthChanged { id: id.to_string(), from: cur.health, to: health };
        self.mutate(
            |entries| {
                if let Some(w) = entries.iter_mut().find(|w| w.id == id) {
                    w.health = health;
                    if health == WorkerHealth::Draining && w.draining_since.is_none() {
                        w.draining_since = Some(now_ms);
                    }
                }
            },
            event,
        );
    }
}
