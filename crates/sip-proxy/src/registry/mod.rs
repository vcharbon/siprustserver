//! [`WorkerRegistry`]: the B2BUA worker set the proxy load-balances across.
//!
//! The routing hot path reads `snapshot`, `resolve` and `lookup_by_address`
//! synchronously and lock-free; only background mutators (the health probe,
//! the membership watcher) write.

use crate::addr::ProxyAddr;

pub mod composed;
pub mod control;
pub mod projection;
pub mod simulated;
pub mod static_reg;
pub mod tombstone;

/// Worker identity: a plain `String` — the registry impls reject
/// empty/malformed ids at build time.
pub type WorkerId = String;

/// Whether a worker serves its dialogs, as the proxy reads it (ADR-0050 §3,
/// ADR-0069).
///
/// `Unknown` is a worker no probe has answered `200` since it joined, since
/// the proxy started, or since its incarnation changed; `Alive` one that
/// answers within its reply windows; `Slow` one that missed its probes but
/// answered one of them late, inside the slow window, or whose endpoint flipped
/// not ready while it did; `Dead` one that missed its probes with no answer at
/// all, stayed `Slow` too long, or whose address departed. An `Alive` or `Slow`
/// worker serves its dialogs; only an `Alive` one takes a new dialog, but for an
/// emergency one when no worker is `Alive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerHealth {
    Unknown,
    Alive,
    Slow,
    Dead,
}

/// Whether a worker takes a new call, as its last OPTIONS answer stated
/// (ADR-0050 §3). In-dialog routing never reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    Accept,
    Refuse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerEntry {
    pub id: WorkerId,
    pub address: ProxyAddr,
    pub health: WorkerHealth,
    pub acceptance: Acceptance,
}

impl WorkerEntry {
    pub fn alive(id: impl Into<WorkerId>, address: ProxyAddr) -> Self {
        Self { id: id.into(), address, health: WorkerHealth::Alive, acceptance: Acceptance::Accept }
    }

    /// Whether a new dialog may be placed on it: it answers its probes in time.
    pub fn is_alive(&self) -> bool {
        self.health == WorkerHealth::Alive
    }

    /// Whether its dialogs are routed to it: `Alive` or `Slow`.
    pub fn serves_dialogs(&self) -> bool {
        matches!(self.health, WorkerHealth::Alive | WorkerHealth::Slow)
    }

    pub fn accepts_new_calls(&self) -> bool {
        self.is_alive() && self.acceptance == Acceptance::Accept
    }
}

/// The read seam the load balancer and the health probe depend on. Every
/// read is synchronous and lock-free.
///
/// The worker set is a projection of `topology::Membership` and health, read
/// directly; membership deltas are observed on `topology::Membership`, and the
/// one event a reader subscribes to is a dead verdict ([`read_dead`]).
///
/// [`read_dead`]: WorkerRegistry::read_dead
pub trait WorkerRegistry: Send + Sync {
    /// Snapshot the current worker set: the workers requests are routed among.
    fn snapshot(&self) -> Vec<WorkerEntry>;
    /// Every worker the probe judges, routed or not: each member present and
    /// not terminating, and any ready one. A superset of [`snapshot`].
    ///
    /// [`snapshot`]: WorkerRegistry::snapshot
    fn probed(&self) -> Vec<WorkerEntry>;
    /// Whether [`snapshot`] routes on the probe's answers: members are present
    /// and none reads ready (ADR-0050 §11).
    ///
    /// [`snapshot`]: WorkerRegistry::snapshot
    fn routes_on_answers(&self) -> bool;
    fn resolve(&self, id: &str) -> Option<WorkerEntry>;
    fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry>;
    /// The ordinals the pool knows but does not route: members whose
    /// endpoint reads not ready, and the last ordinals of departed addresses
    /// still tombstoned (ADR-0055 §8). None of them is in [`snapshot`].
    ///
    /// [`snapshot`]: WorkerRegistry::snapshot
    fn away_ordinals(&self) -> Vec<WorkerId>;
    /// A response for the worker bound at `addr` was seen: if that address
    /// left the set and its departed window is open, the window is re-armed to
    /// Timer H from now (ADR-0031 D4).
    fn rearm_departed(&self, addr: &ProxyAddr);
    /// Whether the worker set is its source's first authoritative listing or
    /// a later one: a fixed pool is at once, a discovered one once its watch
    /// has listed the workers (`topology::Membership::synced`).
    fn synced(&self) -> bool;
    /// Every later dead verdict: the ordinal of a worker whose health turns
    /// `Dead` from `Alive` or `Slow`, or from `Unknown` while its endpoint reads
    /// ready, or of a worker read `Alive` whose endpoint flips not ready and not
    /// terminating while another member reads ready and no late answer of its is
    /// fresh (ADR-0050 §3, ADR-0069). A departure, a terminating flip and a flip
    /// that empties the view of ready members are none.
    fn read_dead(&self) -> tokio::sync::broadcast::Receiver<WorkerId>;
    /// Every later recovery: the ordinal of a worker whose health turns `Alive`
    /// from `Slow`, which is told the standing verdicts again (ADR-0069).
    fn read_recovered(&self) -> tokio::sync::broadcast::Receiver<WorkerId>;
    /// The ordinals a verdict may stand on now, for a reader that lagged
    /// behind [`read_dead`]: every worker read `Dead`, and every unrouted member
    /// whose endpoint reads not ready and not terminating.
    ///
    /// [`read_dead`]: WorkerRegistry::read_dead
    fn read_dead_now(&self) -> Vec<WorkerId>;
}
