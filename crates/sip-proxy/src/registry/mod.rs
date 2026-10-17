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
pub mod tombstone;

/// Worker identity (the source's branded `WorkerId`; a plain `String` here —
/// the registry impls reject empty/malformed ids at build time).
pub type WorkerId = String;

/// Whether a worker serves its dialogs, as the proxy reads it (ADR-0050 §3).
///
/// `Unknown` is a worker no probe has answered `200` since it joined, since
/// the proxy started, or since its incarnation changed; `Alive` one that
/// answered; `Dead` one that missed its probes or whose address departed.
/// Only an `Alive` worker is routed to, for a new dialog and an in-dialog
/// request alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerHealth {
    Unknown,
    Alive,
    Dead,
}

/// Whether a worker takes a new call, as its last OPTIONS answer stated
/// (ADR-0050 §3). In-dialog routing never reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    Accept,
    Refuse,
}

/// A registered worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerEntry {
    pub id: WorkerId,
    pub address: ProxyAddr,
    pub health: WorkerHealth,
    pub acceptance: Acceptance,
}

impl WorkerEntry {
    /// An `Alive` worker accepting new calls.
    pub fn alive(id: impl Into<WorkerId>, address: ProxyAddr) -> Self {
        Self { id: id.into(), address, health: WorkerHealth::Alive, acceptance: Acceptance::Accept }
    }

    /// Whether requests may be routed to it: it answered its probe.
    pub fn is_alive(&self) -> bool {
        self.health == WorkerHealth::Alive
    }

    /// Whether it is alive and accepts new calls.
    pub fn accepts_new_calls(&self) -> bool {
        self.is_alive() && self.acceptance == Acceptance::Accept
    }
}

/// The read seam consumers (LB, health probe) depend on. All reads are sync +
/// lock-free per the D4 non-blocking invariant.
///
/// There is no delta/`changes()` subscription: the worker set is a projection of
/// `topology::Membership` ⊕ health, and consumers read the projection directly.
/// Membership deltas, where needed, are observed via `topology::Membership`.
pub trait WorkerRegistry: Send + Sync {
    /// Snapshot the current worker set.
    fn snapshot(&self) -> Vec<WorkerEntry>;
    /// Resolve a worker by id (`None` if unregistered/removed).
    fn resolve(&self, id: &str) -> Option<WorkerEntry>;
    /// Reverse-lookup the worker bound at `addr` (`None` for any non-worker
    /// source — Alice, Bob, an external SBC).
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry>;
    /// A response for the worker bound at `addr` was seen: if that address
    /// left the set and its departed window is open, the window is re-armed to
    /// Timer H from now (ADR-0031 D4).
    fn rearm_departed(&self, addr: &ProxyAddr);
    /// Whether the worker set is its source's first authoritative listing or
    /// a later one: a fixed pool is at once, a discovered one once its watch
    /// has listed the workers (`topology::Membership::synced`).
    fn synced(&self) -> bool;
}
