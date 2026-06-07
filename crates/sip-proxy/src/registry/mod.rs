//! [`WorkerRegistry`] — the B2BUA worker set the proxy load-balances across
//! (port of `registry/WorkerRegistry.ts`).
//!
//! The routing hot path reads `snapshot`/`resolve`/`lookup_by_address`
//! **synchronously and lock-free** (the worker set lives behind an
//! [`arc_swap::ArcSwap`]); only background mutators (health probe, k8s watcher)
//! write. `changes()` is a `tokio::sync::broadcast` of deltas (no backfill —
//! subscribers `snapshot` first).

use crate::addr::ProxyAddr;

pub mod composed;
pub mod control;
pub mod projection;
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

/// The read seam consumers (LB, health probe) depend on. All reads are sync +
/// lock-free per the D4 non-blocking invariant.
///
/// There is no delta/`changes()` subscription: the worker set is a projection of
/// `topology::Membership` ⊕ health, and consumers read the projection directly.
/// (The old `RegistryEvent` broadcast had no consumer outside the registries' own
/// tests; membership deltas, where needed, are observed via `topology::Membership`.)
pub trait WorkerRegistry: Send + Sync {
    /// Snapshot the current worker set.
    fn snapshot(&self) -> Vec<WorkerEntry>;
    /// Resolve a worker by id (`None` if unregistered/removed).
    fn resolve(&self, id: &str) -> Option<WorkerEntry>;
    /// Reverse-lookup the worker bound at `addr` (`None` for any non-worker
    /// source — Alice, Bob, an external SBC).
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry>;
}
