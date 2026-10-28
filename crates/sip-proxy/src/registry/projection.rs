//! The shared worker-registry core: a [`topology::Membership`] identity/address
//! set + a proxy-only `WorkerAnnotations` overlay, composed into the
//! [`WorkerEntry`] set the LB reads.
//!
//! This is "the rest" that every registry impl shares. Once **health** (the
//! one genuinely proxy-only, OPTIONS-driven field) is lifted out of the worker
//! set, the set *is* membership, and a `WorkerEntry` is a pure projection:
//!
//! ```text
//!   WorkerEntry = membership.host  ⊕  annotation{ port, health, acceptance }
//! ```
//!
//! - **Identity + host** live in `topology::Membership` — one container, one diff
//!   (`reconcile_to_desired`), one self-heal loop (`spawn_membership_reconcile`),
//!   shared with the b2bua replication supervisor.
//! - **Port + health + acceptance** live in `WorkerAnnotations`, keyed by ordinal.
//!   Port is here (not in membership) because topology is deliberately
//!   port-agnostic — the port is a consumer concern. In production every worker
//!   uses the cluster-wide `default_port`; `static`/`simulated` may set a
//!   per-worker `port_override`.
//! - **Departed addresses** live in `Tombstones`, keyed by address: an address
//!   the set stopped serving still resolves as `Dead` for Timer H, so a response
//!   from a node that left the pool is recognised as such.
//! - **Only a `ready` member is routable** (ADR-0031 D1). The filter is applied
//!   here, at the projection, so a member whose endpoint is not ready
//!   departs exactly as a removed one: its ordinal leaves the projection and its
//!   address is tombstoned. Filtering in the LB instead would keep a live entry
//!   and never insert the tombstone.
//! - **The projection** is cached in one [`ArcSwap`] for lock-free hot reads and
//!   is a pure function of the two authoritative sources — never an independent
//!   source of truth, always re-derivable, inherently lag-immune (it recomposes
//!   from the snapshot, never replays deltas).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use sip_clock::Clock;
use tokio::sync::broadcast;
use topology::{Membership, Peer};

use crate::addr::ProxyAddr;

use super::tombstone::{Tombstones, DEPARTED_ADDRESS_TTL_MS};
use super::{Acceptance, WorkerEntry, WorkerHealth, WorkerId};

/// Per-ordinal proxy annotation — everything a [`WorkerEntry`] carries that
/// membership does not. `host` is shadowed only to detect an address move (→ a
/// fresh, re-probed endpoint); routing always takes the host from membership at
/// projection time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Annot {
    health: WorkerHealth,
    acceptance: Acceptance,
    host: String,
    /// Per-worker port. `None` → use the registry's `default_port` (the
    /// production/k8s case; every worker shares the cluster SIP port).
    port_override: Option<u16>,
}

impl Annot {
    /// A brand-new annotation for a just-(re)joined endpoint: `Unknown`, not
    /// routable until the OPTIONS probe reads it `Alive` (ADR-0050 §8).
    fn fresh(host: String) -> Self {
        Self {
            health: WorkerHealth::Unknown,
            acceptance: Acceptance::Accept,
            host,
            port_override: None,
        }
    }
}

/// Proxy-only worker annotations keyed by ordinal — the OPTIONS/health + port
/// concerns lifted out of the worker set. Lock-free reads via [`ArcSwap`].
#[derive(Default)]
struct WorkerAnnotations {
    records: ArcSwap<HashMap<WorkerId, Annot>>,
}

impl WorkerAnnotations {
    fn snapshot(&self) -> Arc<HashMap<WorkerId, Annot>> {
        self.records.load_full()
    }

    /// Read-modify-write helper (annotations only mutate off the hot path).
    /// `rcu`, not load-clone-store: the probe task's `set_health` and the
    /// reconcile task's `sync_membership` run concurrently, and a plain store
    /// could publish a map cloned BEFORE the other writer's update — silently
    /// erasing a `Dead` mark until the next probe tick re-wrote it (a window
    /// where new dialogs kept routing to a dead worker). `rcu` re-runs the
    /// closure on contention, so `f` must be idempotent over its captures.
    fn update(&self, f: impl Fn(&mut HashMap<WorkerId, Annot>)) {
        self.records.rcu(|cur| {
            let mut next = (**cur).clone();
            f(&mut next);
            next
        });
    }

    /// Keep the overlay in step with the membership set: drop departed ordinals,
    /// seed a fresh record for a new ordinal, and **reset** an ordinal whose host
    /// moved (a recreated pod must be re-probed — a stale `Dead`/`Alive` must not
    /// survive). An unchanged ordinal keeps its probe-written health and port.
    /// This is the only proxy-specific lifecycle rule; it has no membership twin.
    ///
    /// Returns the `(ordinal, address)` pairs the overlay stopped serving — a
    /// departure or a host move — for the caller to tombstone.
    fn sync_membership(&self, peers: &[Peer], default_port: u16) -> Vec<(WorkerId, ProxyAddr)> {
        let live: HashSet<&str> = peers.iter().map(|p| p.ordinal.as_str()).collect();
        let mut departed = Vec::new();
        // Membership deltas are rare state changes: each gets its own line. The
        // diff is read before the `rcu` update so a retried closure cannot
        // double-log it.
        {
            let current = self.snapshot();
            let addr_of = |rec: &Annot| {
                ProxyAddr::new(rec.host.clone(), rec.port_override.unwrap_or(default_port))
            };
            for (id, rec) in current.iter() {
                if !live.contains(id.as_str()) {
                    tracing::info!(
                        node = observe::node(),
                        worker = %id,
                        host = %rec.host,
                        "worker left the registry"
                    );
                    departed.push((id.clone(), addr_of(rec)));
                }
            }
            for p in peers {
                match current.get(&p.ordinal) {
                    Some(rec) if rec.host == p.host => {}
                    Some(rec) => {
                        tracing::info!(
                            node = observe::node(),
                            worker = %p.ordinal,
                            from_host = %rec.host,
                            host = %p.host,
                            "worker re-created at a new host"
                        );
                        departed.push((p.ordinal.clone(), addr_of(rec)));
                    }
                    None => tracing::info!(
                        node = observe::node(),
                        worker = %p.ordinal,
                        host = %p.host,
                        "worker joined the registry"
                    ),
                }
            }
        }
        self.update(|recs| {
            recs.retain(|id, _| live.contains(id.as_str()));
            for p in peers {
                match recs.get(&p.ordinal) {
                    Some(rec) if rec.host == p.host => {} // unchanged → preserve
                    _ => {
                        recs.insert(p.ordinal.clone(), Annot::fresh(p.host.clone()));
                    }
                }
            }
        });
        departed
    }

    /// Annotate health and acceptance of new calls in one write, `None`
    /// leaving that one as it is. No-op if the worker is unknown or unchanged.
    /// Returns whether the write turned the worker's health `Dead`.
    fn set_view(
        &self,
        id: &str,
        health: Option<WorkerHealth>,
        acceptance: Option<Acceptance>,
    ) -> bool {
        let Some(previous) = self.snapshot().get(id).map(|r| (r.health, r.acceptance)) else {
            return false;
        };
        let next = (health.unwrap_or(previous.0), acceptance.unwrap_or(previous.1));
        if next == previous {
            return false;
        }
        if next.0 != previous.0 {
            let (from, to) = (previous.0, next.0);
            tracing::info!(node = observe::node(), worker = id, ?from, ?to, "worker health flip");
        }
        if next.1 != previous.1 {
            let (from, to) = (previous.1, next.1);
            tracing::info!(
                node = observe::node(),
                worker = id,
                ?from,
                ?to,
                "worker acceptance flip"
            );
        }
        self.update(|recs| {
            if let Some(rec) = recs.get_mut(id) {
                (rec.health, rec.acceptance) = next;
            }
        });
        next.0 == WorkerHealth::Dead && previous.0 != WorkerHealth::Dead
    }
}

/// Project the membership peer set + the annotation overlay into the worker set
/// the LB reads. Pure: same inputs → same output, no diff, no stored identity.
/// `default_port` fills in workers with no `port_override` (the production case).
/// A peer with no annotation reads as `Unknown` (not routable) and self-corrects
/// on the next recompose. Duplicate ordinals are de-duped first-wins (defensive;
/// topology snapshots are already unique).
fn project(
    peers: &[Peer],
    annots: &HashMap<WorkerId, Annot>,
    default_port: u16,
) -> Vec<WorkerEntry> {
    let mut seen = HashSet::new();
    peers
        .iter()
        .filter(|p| seen.insert(p.ordinal.clone()))
        .map(|p| {
            let a = annots.get(&p.ordinal);
            let port = a.and_then(|a| a.port_override).unwrap_or(default_port);
            WorkerEntry {
                id: p.ordinal.clone(),
                address: ProxyAddr::new(p.host.clone(), port),
                health: a.map(|a| a.health).unwrap_or(WorkerHealth::Unknown),
                acceptance: a.map(|a| a.acceptance).unwrap_or(Acceptance::Accept),
            }
        })
        .collect()
}

/// The shared registry core: the two authoritative sources (membership +
/// annotations), a lock-free projection cache, and the recompose that derives one
/// from the other. Every registry impl embeds this; they differ only in *when*
/// they call [`recompose`](WorkerSet::recompose) (a live task, a one-shot at
/// build, or an eager call after a test mutation).
pub(crate) struct WorkerSet {
    membership: Arc<dyn Membership>,
    annotations: WorkerAnnotations,
    tombstones: Tombstones,
    projection: ArcSwap<Vec<WorkerEntry>>,
    /// The projection was composed from an authoritative listing.
    listed: AtomicBool,
    default_port: u16,
    clock: Clock,
    /// Each worker the health view turns `Dead` (ADR-0050 §3).
    read_dead: broadcast::Sender<WorkerId>,
}

/// How many dead verdicts a slow reader of [`WorkerSet::read_dead`] may fall
/// behind before it reads a lag.
const READ_DEAD_BACKLOG: usize = 64;

impl WorkerSet {
    /// Build over `membership` with an empty annotation overlay; the projection is
    /// computed once so reads are valid immediately.
    pub(crate) fn new(membership: Arc<dyn Membership>, default_port: u16, clock: Clock) -> Self {
        #[expect(clippy::disallowed_methods, reason = "READ_DEAD_BACKLOG is non-zero")]
        let (read_dead, _) = broadcast::channel(READ_DEAD_BACKLOG);
        let set = Self {
            membership,
            annotations: WorkerAnnotations::default(),
            tombstones: Tombstones::default(),
            projection: ArcSwap::from_pointee(Vec::new()),
            listed: AtomicBool::new(false),
            default_port,
            clock,
            read_dead,
        };
        set.recompose();
        set
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.clock.now_ms()).unwrap_or(0)
    }

    /// Recompute the projection from the **ready** members of the authoritative
    /// membership snapshot + the annotation overlay, and keep the departed-address
    /// overlay in step: an address the set stopped serving — the member left, or
    /// its endpoint reads not `ready` — is tombstoned, an address a worker
    /// joined at is forgotten. A `terminating` flip on a ready member changes
    /// nothing. Idempotent; the single write path for every registry.
    pub(crate) fn recompose(&self) {
        let peers: Vec<Peer> = self.membership.snapshot().into_iter().filter(|p| p.ready).collect();
        let now_ms = self.now_ms();
        let departed = self.annotations.sync_membership(&peers, self.default_port);
        let annots = self.annotations.snapshot();
        let entries = project(&peers, &annots, self.default_port);
        for (id, address) in departed {
            tracing::info!(
                node = observe::node(),
                worker = %id,
                address = %address,
                ttl_ms = DEPARTED_ADDRESS_TTL_MS,
                "departed worker address tombstoned"
            );
            self.tombstones.insert(address, id, now_ms);
        }
        for e in &entries {
            self.tombstones.clear(&e.address);
        }
        self.tombstones.prune(now_ms);
        self.projection.store(Arc::new(entries));
    }

    /// Annotate a worker's health (the OPTIONS write seam) and recompose.
    pub(crate) fn set_health(&self, id: &str, health: WorkerHealth) {
        self.set_view(id, Some(health), None);
    }

    /// Annotate a worker's acceptance of new calls (the OPTIONS write seam)
    /// and recompose.
    pub(crate) fn set_acceptance(&self, id: &str, acceptance: Acceptance) {
        self.set_view(id, None, Some(acceptance));
    }

    /// Annotate a worker's health and acceptance at once (`None` keeps that
    /// one) and recompose once: no read sees one without the other. A write
    /// that turns the worker `Dead` is a dead verdict, told to every
    /// [`read_dead`](Self::read_dead) reader once the projection holds it.
    pub(crate) fn set_view(
        &self,
        id: &str,
        health: Option<WorkerHealth>,
        acceptance: Option<Acceptance>,
    ) {
        let turned_dead = self.annotations.set_view(id, health, acceptance);
        self.recompose();
        if turned_dead {
            let _ = self.read_dead.send(id.to_string());
        }
    }

    /// Every later dead verdict: a worker whose health the view turns `Dead`
    /// from another health. A membership departure is no verdict: its address
    /// is tombstoned and its ordinal leaves the projection.
    pub(crate) fn read_dead(&self) -> broadcast::Receiver<WorkerId> {
        self.read_dead.subscribe()
    }

    /// Pin a per-worker `port_override` for `id` (the `static`/`simulated` per-worker
    /// port; no-op for an unknown ordinal). Does not recompose — the caller pairs it
    /// with a membership mutation + `recompose`.
    pub(crate) fn set_port_override(&self, id: &str, port: u16) {
        self.annotations.update(|recs| {
            if let Some(rec) = recs.get_mut(id) {
                rec.port_override = Some(port);
            }
        });
    }

    /// Seed an explicit annotation for `id` *before* it is added to membership, so
    /// the subsequent `recompose` preserves the intended port/health/acceptance
    /// instead of seeding a fresh `Unknown`. For the `static`/`simulated` add paths.
    pub(crate) fn preset(
        &self,
        id: &str,
        host: String,
        port: u16,
        health: WorkerHealth,
        acceptance: Acceptance,
    ) {
        self.annotations.update(|recs| {
            recs.insert(
                id.to_string(),
                Annot { health, acceptance, host: host.clone(), port_override: Some(port) },
            );
        });
    }

    /// Tombstone `address`, which worker `id` stopped serving outside a
    /// membership recompose (a test double's host move), as a recompose does.
    pub(crate) fn depart(&self, address: ProxyAddr, id: &str) {
        self.tombstones.insert(address, id.to_string(), self.now_ms());
    }

    // ---- read seam (lock-free, hot path) ----

    pub(crate) fn snapshot(&self) -> Vec<WorkerEntry> {
        self.projection.load().as_ref().clone()
    }
    pub(crate) fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.projection.load().iter().find(|w| w.id == id).cloned()
    }
    /// The worker bound at `addr`. An address that left the set answers as its
    /// last ordinal with health `Dead` for [`DEPARTED_ADDRESS_TTL_MS`], so a
    /// response still coming from it is recognised as a departed worker's.
    pub(crate) fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.projection
            .load()
            .iter()
            .find(|w| &w.address == addr)
            .cloned()
            .or_else(|| self.tombstones.lookup(addr, self.now_ms()))
    }
    /// A response for `addr` was seen: a departed address's open window is
    /// re-armed to [`DEPARTED_ADDRESS_TTL_MS`] from now.
    pub(crate) fn rearm_departed(&self, addr: &ProxyAddr) {
        self.tombstones.rearm(addr, self.now_ms());
    }
    pub(crate) fn membership(&self) -> &Arc<dyn Membership> {
        &self.membership
    }
    /// Whether the projection holds the membership's first authoritative
    /// listing. The first time the membership reads synced, the projection is
    /// recomposed from it before this answers `true`, so a snapshot read
    /// after a `true` never predates the listing.
    pub(crate) fn synced(&self) -> bool {
        if self.listed.load(Ordering::Acquire) {
            return true;
        }
        if !self.membership.synced() {
            return false;
        }
        self.recompose();
        self.listed.store(true, Ordering::Release);
        true
    }
}

#[cfg(test)]
mod tests;
